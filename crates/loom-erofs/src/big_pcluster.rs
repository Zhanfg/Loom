#![forbid(unsafe_code)]

use crate::multi_lz4 as lz4;

use loom_map::LoomMap;
use loom_view::{EffectiveBlockStore, ViewError};
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

const SUPERBLOCK_OFFSET: u64 = 1024;
const SUPERBLOCK_SIZE: usize = 128;
const EROFS_MAGIC: u32 = 0xe0f5_e1e2;
const BLOCK_SIZE: u32 = 4096;
const BLOCK_BYTES: usize = 4096;
const BLOCK_BITS: u16 = 12;
const OFFSET_MASK: u16 = (1 << BLOCK_BITS) - 1;
const CBLKCNT: u16 = 1 << 11;
const DIRENT_SIZE: usize = 12;
const MODE_TYPE_MASK: u16 = 0o170_000;
const MODE_DIRECTORY: u16 = 0o040_000;
const MODE_REGULAR: u16 = 0o100_000;
const DATA_FLAT_PLAIN: u8 = 0;
const DATA_FLAT_INLINE: u8 = 2;
const DATA_COMPRESSED_COMPACT: u8 = 3;
const MAP_HEADER_SIZE: u64 = 8;
const LCLUSTER_HEAD1: u16 = 1;
const LCLUSTER_NONHEAD: u16 = 2;
const LCLUSTER_TYPE_MASK: u16 = 3;
const ADVISE_COMPACTED_2B: u16 = 0x0001;
const ADVISE_BIG_PCLUSTER_1: u16 = 0x0002;
const ADVISE_BIG_PCLUSTER_2: u16 = 0x0004;
const BIG_ADVISE: u16 = ADVISE_COMPACTED_2B | ADVISE_BIG_PCLUSTER_1 | ADVISE_BIG_PCLUSTER_2;
const LZ4_ALGORITHM: u8 = 0;
const FEATURE_LZ4_0PADDING: u32 = 0x0000_0001;
const FEATURE_BIG_PCLUSTER: u32 = 0x0000_0002;
const REQUIRED_INCOMPAT: u32 = FEATURE_LZ4_0PADDING | FEATURE_BIG_PCLUSTER;
const PROOF_PHYSICAL_BLOCKS: usize = 2;

#[derive(Debug)]
pub struct CompiledBigPclusterSwap {
    pub map: LoomMap,
    pub shadow: Vec<u8>,
    pub block_size: u32,
    pub origin_nid: u64,
    pub origin_pcluster: u64,
    pub replacement_pcluster: u64,
    pub encoded_bytes: usize,
    pub logical_lclusters: usize,
    pub compact_2b_entries: usize,
    pub shadow_blocks: usize,
}

#[derive(Debug, Clone, Copy)]
struct Superblock {
    root_nid: u64,
    meta_block: u64,
}

#[derive(Debug, Clone, Copy)]
struct Inode {
    nid: u64,
    offset: u64,
    isize: u64,
    xattr_size: u64,
    mode: u16,
    size: u64,
    layout: u8,
    data_word: u32,
}

impl Inode {
    fn file_type(self) -> u16 {
        self.mode & MODE_TYPE_MASK
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CompactRegions {
    initial_4b: usize,
    compact_2b: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CompactEntry {
    kind: u16,
    low: u16,
    slot: usize,
    slots: usize,
    base_pblk: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Extent {
    nid: u64,
    logical_size: u64,
    logical_lclusters: usize,
    pcluster: u64,
    physical_blocks: usize,
    compact_2b_entries: usize,
}

struct Image {
    file: File,
    bytes: u64,
    sb: Superblock,
}

/// Swaps one compact EROFS big pcluster that occupies exactly two physical blocks.
///
/// Both images must use 4 KiB blocks/lclusters, compact LZ4 indexes,
/// `COMPACTED_2B | BIG_PCLUSTER_1 | BIG_PCLUSTER_2`, one logical extent beginning at
/// lcluster zero, and a CBLKCNT value of exactly two physical blocks. Compact metadata is
/// left untouched; Loom substitutes the two encoded origin blocks with the corresponding
/// two encoded blocks from the replacement image.
///
/// # Errors
/// Returns [`BigPclusterError`] for malformed or unsupported EROFS, incompatible replacement
/// topology, ordinary I/O failures, or effective-view compilation failures.
pub fn compile_big_pcluster_swap(
    origin_path: &Path,
    target_path: &str,
    replacement_image_path: &Path,
) -> Result<CompiledBigPclusterSwap, BigPclusterError> {
    let mut origin = Image::open(origin_path)?;
    let mut replacement = Image::open(replacement_image_path)?;
    let origin_nid = origin.resolve_path(target_path)?;
    let replacement_nid = replacement.resolve_path(target_path)?;
    let origin_extent = origin.read_big_extent(origin_nid)?;
    let replacement_extent = replacement.read_big_extent(replacement_nid)?;

    if origin_extent.logical_size != replacement_extent.logical_size
        || origin_extent.logical_lclusters != replacement_extent.logical_lclusters
        || origin_extent.physical_blocks != replacement_extent.physical_blocks
    {
        return Err(BigPclusterError::IncompatibleReplacement(
            "logical size/lcluster count or big-pcluster footprint differs",
        ));
    }

    let replacement_span = replacement.read_span(
        replacement_extent.pcluster,
        replacement_extent.physical_blocks,
    )?;
    compile_span(
        origin_path,
        origin_extent,
        replacement_extent.pcluster,
        &replacement_span,
        PROOF_PHYSICAL_BLOCKS
            .checked_mul(BLOCK_BYTES)
            .ok_or(BigPclusterError::ArithmeticOverflow)?,
    )
}

/// Encodes a plain replacement payload into the existing two-block compact big pcluster.
///
/// The raw LZ4 stream is right-aligned across the complete 8192-byte physical span; the
/// prefix is zero-filled and may cross the first 4 KiB block boundary. Raw and complete
/// 0padding round trips are both validated before the effective block store is opened.
///
/// # Errors
/// Returns [`BigPclusterError`] for unsupported metadata, replacement-size mismatch, LZ4
/// footprint overflow/validation failure, ordinary I/O, or effective-view failures.
pub fn compile_big_pcluster_lz4(
    origin_path: &Path,
    target_path: &str,
    replacement_path: &Path,
) -> Result<CompiledBigPclusterSwap, BigPclusterError> {
    let replacement = fs::read(replacement_path).map_err(BigPclusterError::Io)?;
    let mut origin = Image::open(origin_path)?;
    let origin_nid = origin.resolve_path(target_path)?;
    let extent = origin.read_big_extent(origin_nid)?;
    let actual =
        u64::try_from(replacement.len()).map_err(|_| BigPclusterError::ArithmeticOverflow)?;
    if actual != extent.logical_size {
        return Err(BigPclusterError::ReplacementSizeMismatch {
            expected: extent.logical_size,
            actual,
        });
    }

    let compressed =
        lz4::encode(&replacement).map_err(|_| BigPclusterError::CompressionValidationFailed)?;
    let capacity = extent
        .physical_blocks
        .checked_mul(BLOCK_BYTES)
        .ok_or(BigPclusterError::ArithmeticOverflow)?;
    if compressed.len() > capacity {
        return Err(BigPclusterError::CompressionDoesNotFit {
            encoded: compressed.len(),
            capacity,
        });
    }
    if compressed.first().copied().unwrap_or(0) == 0 {
        return Err(BigPclusterError::CompressionValidationFailed);
    }
    if lz4::decode(&compressed, replacement.len())
        .map_err(|_| BigPclusterError::CompressionValidationFailed)?
        != replacement
    {
        return Err(BigPclusterError::CompressionValidationFailed);
    }

    let mut span = vec![0_u8; capacity];
    let start = capacity
        .checked_sub(compressed.len())
        .ok_or(BigPclusterError::ArithmeticOverflow)?;
    span[start..].copy_from_slice(&compressed);
    if lz4::decode_0padding(&span, replacement.len())
        .map_err(|_| BigPclusterError::CompressionValidationFailed)?
        != replacement
    {
        return Err(BigPclusterError::CompressionValidationFailed);
    }

    compile_span(
        origin_path,
        extent,
        extent.pcluster,
        &span,
        compressed.len(),
    )
}

fn compile_span(
    origin_path: &Path,
    extent: Extent,
    replacement_pcluster: u64,
    replacement_span: &[u8],
    encoded_bytes: usize,
) -> Result<CompiledBigPclusterSwap, BigPclusterError> {
    let expected = extent
        .physical_blocks
        .checked_mul(BLOCK_BYTES)
        .ok_or(BigPclusterError::ArithmeticOverflow)?;
    if replacement_span.len() != expected {
        return Err(BigPclusterError::InvalidFilesystem(
            "big-pcluster replacement span length differs from CBLKCNT footprint",
        ));
    }

    let mut view =
        EffectiveBlockStore::open(origin_path, BLOCK_SIZE).map_err(BigPclusterError::View)?;
    for block_index in 0..extent.physical_blocks {
        let start = block_index
            .checked_mul(BLOCK_BYTES)
            .ok_or(BigPclusterError::ArithmeticOverflow)?;
        let end = start
            .checked_add(BLOCK_BYTES)
            .ok_or(BigPclusterError::ArithmeticOverflow)?;
        let logical_block = extent
            .pcluster
            .checked_add(
                u64::try_from(block_index).map_err(|_| BigPclusterError::ArithmeticOverflow)?,
            )
            .ok_or(BigPclusterError::ArithmeticOverflow)?;
        view.block_mut(logical_block)
            .map_err(BigPclusterError::View)?
            .copy_from_slice(
                replacement_span
                    .get(start..end)
                    .ok_or(BigPclusterError::UnexpectedEndOfStructure)?,
            );
    }
    let compiled = view.finalize().map_err(BigPclusterError::View)?;
    if compiled.shadow_blocks != extent.physical_blocks {
        return Err(BigPclusterError::InvalidFilesystem(
            "big-pcluster shadow block count differs from CBLKCNT",
        ));
    }

    Ok(CompiledBigPclusterSwap {
        map: compiled.map,
        shadow: compiled.shadow,
        block_size: compiled.block_size,
        origin_nid: extent.nid,
        origin_pcluster: extent.pcluster,
        replacement_pcluster,
        encoded_bytes,
        logical_lclusters: extent.logical_lclusters,
        compact_2b_entries: extent.compact_2b_entries,
        shadow_blocks: compiled.shadow_blocks,
    })
}

impl Image {
    fn open(path: &Path) -> Result<Self, BigPclusterError> {
        let mut file = File::open(path).map_err(BigPclusterError::Io)?;
        let bytes = file.metadata().map_err(BigPclusterError::Io)?.len();
        let sb = read_superblock(&mut file, bytes)?;
        Ok(Self { file, bytes, sb })
    }

    fn read_inode(&mut self, nid: u64) -> Result<Inode, BigPclusterError> {
        let metadata_base = self
            .sb
            .meta_block
            .checked_mul(u64::from(BLOCK_SIZE))
            .ok_or(BigPclusterError::ArithmeticOverflow)?;
        let offset = metadata_base
            .checked_add(
                nid.checked_mul(32)
                    .ok_or(BigPclusterError::ArithmeticOverflow)?,
            )
            .ok_or(BigPclusterError::ArithmeticOverflow)?;
        ensure_range(self.bytes, offset, 32)?;
        let mut compact = [0_u8; 32];
        read_exact_at(&mut self.file, offset, &mut compact)?;
        let format = read_u16(&compact, 0)?;
        if format & !0x1f != 0 {
            return Err(BigPclusterError::UnsupportedInode(
                "unknown EROFS inode format bits",
            ));
        }
        let extended = format & 1 != 0;
        let layout =
            u8::try_from((format >> 1) & 7).map_err(|_| BigPclusterError::ArithmeticOverflow)?;
        if layout > 4 {
            return Err(BigPclusterError::UnsupportedInode(
                "reserved EROFS inode data layout",
            ));
        }
        let isize = if extended { 64 } else { 32 };
        let xattr_size = xattr_ibody_size(read_u16(&compact, 2)?)?;
        let mode = read_u16(&compact, 4)?;
        let size = if extended {
            ensure_range(self.bytes, offset, 64)?;
            let mut raw = [0_u8; 64];
            read_exact_at(&mut self.file, offset, &mut raw)?;
            read_u64(&raw, 8)?
        } else {
            u64::from(read_u32(&compact, 8)?)
        };
        Ok(Inode {
            nid,
            offset,
            isize,
            xattr_size,
            mode,
            size,
            layout,
            data_word: read_u32(&compact, 0x10)?,
        })
    }

    fn resolve_path(&mut self, path: &str) -> Result<u64, BigPclusterError> {
        let components = parse_absolute_path(path)?;
        let mut current = self.sb.root_nid;
        for component in components {
            let inode = self.read_inode(current)?;
            if inode.file_type() != MODE_DIRECTORY {
                return Err(BigPclusterError::NotDirectory(current));
            }
            current = self.find_child(&inode, component.as_bytes())?;
        }
        Ok(current)
    }

    fn find_child(&mut self, directory: &Inode, name: &[u8]) -> Result<u64, BigPclusterError> {
        if directory.xattr_size != 0 {
            return Err(BigPclusterError::UnsupportedInode(
                "big-pcluster path traversal refuses directory xattrs",
            ));
        }
        let block_size = u64::from(BLOCK_SIZE);
        let full_blocks = match directory.layout {
            DATA_FLAT_PLAIN => div_ceil(directory.size, block_size)?,
            DATA_FLAT_INLINE => directory.size / block_size,
            _ => {
                return Err(BigPclusterError::UnsupportedInode(
                    "big-pcluster path traversal requires flat directories",
                ))
            }
        };
        for index in 0..full_blocks {
            let block = u64::from(directory.data_word)
                .checked_add(index)
                .ok_or(BigPclusterError::ArithmeticOverflow)?;
            let bytes = self.read_block(block)?;
            if let Some(nid) = find_in_directory_block(&bytes, name)? {
                return Ok(nid);
            }
        }
        if directory.layout == DATA_FLAT_INLINE && directory.size % block_size != 0 {
            return Err(BigPclusterError::UnsupportedInode(
                "target may lie in unsupported inline directory tail",
            ));
        }
        Err(BigPclusterError::PathNotFound(
            String::from_utf8_lossy(name).into_owned(),
        ))
    }

    fn read_big_extent(&mut self, nid: u64) -> Result<Extent, BigPclusterError> {
        let inode = self.read_inode(nid)?;
        let logical_lclusters = validate_big_inode(&inode)?;
        let (ebase, regions) = self.read_big_map_header(&inode, logical_lclusters)?;
        let mut entries = Vec::with_capacity(logical_lclusters);
        for lcn in 0..logical_lclusters {
            entries.push(self.read_compact_entry(ebase, logical_lclusters, lcn)?);
        }
        validate_big_single_extent(&entries, logical_lclusters)?;
        let head = entries.first().ok_or(BigPclusterError::InvalidFilesystem(
            "compact index stream is empty",
        ))?;
        let pcluster = head.base_pblk;
        let block_count = self.bytes / u64::from(BLOCK_SIZE);
        if pcluster
            .checked_add(
                u64::try_from(PROOF_PHYSICAL_BLOCKS)
                    .map_err(|_| BigPclusterError::ArithmeticOverflow)?,
            )
            .ok_or(BigPclusterError::ArithmeticOverflow)?
            > block_count
        {
            return Err(BigPclusterError::InvalidFilesystem(
                "big pcluster extends beyond image",
            ));
        }
        Ok(Extent {
            nid,
            logical_size: inode.size,
            logical_lclusters,
            pcluster,
            physical_blocks: PROOF_PHYSICAL_BLOCKS,
            compact_2b_entries: regions.compact_2b,
        })
    }

    fn read_big_map_header(
        &mut self,
        inode: &Inode,
        logical_lclusters: usize,
    ) -> Result<(u64, CompactRegions), BigPclusterError> {
        let header_offset = align8(
            inode
                .offset
                .checked_add(inode.isize)
                .and_then(|value| value.checked_add(inode.xattr_size))
                .ok_or(BigPclusterError::ArithmeticOverflow)?,
        )?;
        ensure_range(self.bytes, header_offset, MAP_HEADER_SIZE)?;
        let mut header = [0_u8; 8];
        read_exact_at(&mut self.file, header_offset, &mut header)?;
        let advise = read_u16(&header, 4)?;
        if advise != BIG_ADVISE {
            return Err(BigPclusterError::UnsupportedInode(
                "big-pcluster proof requires COMPACTED_2B plus both big-pcluster advice bits",
            ));
        }
        if header[6] != LZ4_ALGORITHM || header[7] != 0 {
            return Err(BigPclusterError::UnsupportedInode(
                "big-pcluster proof requires HEAD1 LZ4 with 4 KiB logical clusters",
            ));
        }
        let ebase = header_offset
            .checked_add(MAP_HEADER_SIZE)
            .ok_or(BigPclusterError::ArithmeticOverflow)?;
        Ok((ebase, compact_regions(ebase, logical_lclusters)?))
    }

    fn read_compact_entry(
        &mut self,
        ebase: u64,
        total: usize,
        lcn: usize,
    ) -> Result<CompactEntry, BigPclusterError> {
        let regions = compact_regions(ebase, total)?;
        let (shift, pos) = compact_entry_position(ebase, regions, lcn)?;
        let entry_bytes = 1_usize
            .checked_shl(shift)
            .ok_or(BigPclusterError::ArithmeticOverflow)?;
        let slots = if entry_bytes == 4 { 2 } else { 16 };
        let pack_bytes = entry_bytes
            .checked_mul(slots)
            .ok_or(BigPclusterError::ArithmeticOverflow)?;
        let pack_bytes_u64 =
            u64::try_from(pack_bytes).map_err(|_| BigPclusterError::ArithmeticOverflow)?;
        let pack_start = pos - (pos % pack_bytes_u64);
        ensure_range(self.bytes, pack_start, pack_bytes_u64)?;
        let mut pack = vec![0_u8; pack_bytes];
        read_exact_at(&mut self.file, pack_start, &mut pack)?;
        let entry_bytes_u64 =
            u64::try_from(entry_bytes).map_err(|_| BigPclusterError::ArithmeticOverflow)?;
        let slot = usize::try_from((pos - pack_start) / entry_bytes_u64)
            .map_err(|_| BigPclusterError::ArithmeticOverflow)?;
        let encode_bits = (pack_bytes
            .checked_mul(8)
            .and_then(|bits| bits.checked_sub(32))
            .ok_or(BigPclusterError::ArithmeticOverflow)?)
            / slots;
        let bit_pos = encode_bits
            .checked_mul(slot)
            .ok_or(BigPclusterError::ArithmeticOverflow)?;
        let word = read_u32(&pack, bit_pos / 8)? >> (bit_pos & 7);
        let low = u16::try_from(word & u32::from(OFFSET_MASK))
            .map_err(|_| BigPclusterError::ArithmeticOverflow)?;
        let kind = u16::try_from((word >> BLOCK_BITS) & u32::from(LCLUSTER_TYPE_MASK))
            .map_err(|_| BigPclusterError::ArithmeticOverflow)?;
        Ok(CompactEntry {
            kind,
            low,
            slot,
            slots,
            base_pblk: u64::from(read_u32(&pack, pack_bytes - 4)?),
        })
    }

    fn read_block(&mut self, block: u64) -> Result<Vec<u8>, BigPclusterError> {
        let offset = block
            .checked_mul(u64::from(BLOCK_SIZE))
            .ok_or(BigPclusterError::ArithmeticOverflow)?;
        ensure_range(self.bytes, offset, u64::from(BLOCK_SIZE))?;
        let mut bytes = vec![0_u8; BLOCK_BYTES];
        read_exact_at(&mut self.file, offset, &mut bytes)?;
        Ok(bytes)
    }

    fn read_span(&mut self, block: u64, count: usize) -> Result<Vec<u8>, BigPclusterError> {
        let mut span = Vec::with_capacity(
            count
                .checked_mul(BLOCK_BYTES)
                .ok_or(BigPclusterError::ArithmeticOverflow)?,
        );
        for index in 0..count {
            let physical = block
                .checked_add(
                    u64::try_from(index).map_err(|_| BigPclusterError::ArithmeticOverflow)?,
                )
                .ok_or(BigPclusterError::ArithmeticOverflow)?;
            span.extend_from_slice(&self.read_block(physical)?);
        }
        Ok(span)
    }
}

fn validate_big_inode(inode: &Inode) -> Result<usize, BigPclusterError> {
    if inode.file_type() != MODE_REGULAR {
        return Err(BigPclusterError::NotRegularFile(inode.nid));
    }
    if inode.layout != DATA_COMPRESSED_COMPACT {
        return Err(BigPclusterError::UnsupportedInode(
            "big-pcluster proof requires EROFS_INODE_COMPRESSED_COMPACT",
        ));
    }
    if inode.xattr_size != 0 {
        return Err(BigPclusterError::UnsupportedInode(
            "big-pcluster target must not carry xattrs",
        ));
    }
    if inode.size < u64::from(BLOCK_SIZE) * 2 || inode.size % u64::from(BLOCK_SIZE) != 0 {
        return Err(BigPclusterError::UnsupportedInode(
            "big-pcluster proof requires a whole-block file of at least two lclusters",
        ));
    }
    if usize::try_from(inode.data_word).map_err(|_| BigPclusterError::ArithmeticOverflow)?
        != PROOF_PHYSICAL_BLOCKS
    {
        return Err(BigPclusterError::UnsupportedInode(
            "big-pcluster proof requires exactly two encoded physical blocks",
        ));
    }
    usize::try_from(inode.size / u64::from(BLOCK_SIZE))
        .map_err(|_| BigPclusterError::ArithmeticOverflow)
}

fn validate_big_single_extent(
    entries: &[CompactEntry],
    total: usize,
) -> Result<(), BigPclusterError> {
    let head = entries.first().ok_or(BigPclusterError::InvalidFilesystem(
        "compact index stream is empty",
    ))?;
    if head.kind != LCLUSTER_HEAD1 || head.low != 0 || head.slot != 0 {
        return Err(BigPclusterError::InvalidFilesystem(
            "big-pcluster extent must begin with slot-0 zero-offset HEAD1",
        ));
    }
    if total < 2 {
        return Err(BigPclusterError::UnsupportedInode(
            "big pcluster needs a following index for CBLKCNT",
        ));
    }
    let cblk = entries
        .get(1)
        .ok_or(BigPclusterError::InvalidFilesystem("missing CBLKCNT index"))?;
    if cblk.kind != LCLUSTER_NONHEAD
        || cblk.low & CBLKCNT == 0
        || usize::from(cblk.low & !CBLKCNT) != PROOF_PHYSICAL_BLOCKS
    {
        return Err(BigPclusterError::InvalidFilesystem(
            "first NONHEAD does not encode a two-block CBLKCNT",
        ));
    }
    for (lcn, entry) in entries.iter().enumerate().skip(2) {
        if entry.kind != LCLUSTER_NONHEAD || entry.low & CBLKCNT != 0 {
            return Err(BigPclusterError::InvalidFilesystem(
                "big-pcluster extent contains an unexpected entry after CBLKCNT",
            ));
        }
        let expected = if entry.slot + 1 == entry.slots {
            total
                .checked_sub(lcn)
                .ok_or(BigPclusterError::ArithmeticOverflow)?
        } else {
            lcn
        };
        let expected = u16::try_from(expected).map_err(|_| BigPclusterError::ArithmeticOverflow)?;
        if entry.low != expected {
            return Err(BigPclusterError::InvalidFilesystem(
                "NONHEAD lookback/lookahead disagrees with one-head big extent",
            ));
        }
    }
    Ok(())
}

fn compact_regions(ebase: u64, total: usize) -> Result<CompactRegions, BigPclusterError> {
    let modulo = usize::try_from(ebase % 32).map_err(|_| BigPclusterError::ArithmeticOverflow)?;
    let mut initial_4b = (32_usize
        .checked_sub(modulo)
        .ok_or(BigPclusterError::ArithmeticOverflow)?)
        / 4;
    if initial_4b == 8 {
        initial_4b = 0;
    }
    let compact_2b = if initial_4b < total {
        ((total - initial_4b) / 16) * 16
    } else {
        0
    };
    Ok(CompactRegions {
        initial_4b,
        compact_2b,
    })
}

fn compact_entry_position(
    ebase: u64,
    regions: CompactRegions,
    lcn: usize,
) -> Result<(u32, u64), BigPclusterError> {
    if lcn < regions.initial_4b {
        let delta = u64::try_from(
            lcn.checked_mul(4)
                .ok_or(BigPclusterError::ArithmeticOverflow)?,
        )
        .map_err(|_| BigPclusterError::ArithmeticOverflow)?;
        return Ok((
            2,
            ebase
                .checked_add(delta)
                .ok_or(BigPclusterError::ArithmeticOverflow)?,
        ));
    }
    let initial_bytes = u64::try_from(
        regions
            .initial_4b
            .checked_mul(4)
            .ok_or(BigPclusterError::ArithmeticOverflow)?,
    )
    .map_err(|_| BigPclusterError::ArithmeticOverflow)?;
    let mut pos = ebase
        .checked_add(initial_bytes)
        .ok_or(BigPclusterError::ArithmeticOverflow)?;
    let relative = lcn
        .checked_sub(regions.initial_4b)
        .ok_or(BigPclusterError::ArithmeticOverflow)?;
    if relative < regions.compact_2b {
        let delta = u64::try_from(
            relative
                .checked_mul(2)
                .ok_or(BigPclusterError::ArithmeticOverflow)?,
        )
        .map_err(|_| BigPclusterError::ArithmeticOverflow)?;
        return Ok((
            1,
            pos.checked_add(delta)
                .ok_or(BigPclusterError::ArithmeticOverflow)?,
        ));
    }
    pos = pos
        .checked_add(
            u64::try_from(
                regions
                    .compact_2b
                    .checked_mul(2)
                    .ok_or(BigPclusterError::ArithmeticOverflow)?,
            )
            .map_err(|_| BigPclusterError::ArithmeticOverflow)?,
        )
        .ok_or(BigPclusterError::ArithmeticOverflow)?;
    let trailing = relative
        .checked_sub(regions.compact_2b)
        .ok_or(BigPclusterError::ArithmeticOverflow)?;
    let delta = u64::try_from(
        trailing
            .checked_mul(4)
            .ok_or(BigPclusterError::ArithmeticOverflow)?,
    )
    .map_err(|_| BigPclusterError::ArithmeticOverflow)?;
    Ok((
        2,
        pos.checked_add(delta)
            .ok_or(BigPclusterError::ArithmeticOverflow)?,
    ))
}

fn read_superblock(file: &mut File, bytes: u64) -> Result<Superblock, BigPclusterError> {
    ensure_range(
        bytes,
        SUPERBLOCK_OFFSET,
        u64::try_from(SUPERBLOCK_SIZE).map_err(|_| BigPclusterError::ArithmeticOverflow)?,
    )?;
    let mut raw = [0_u8; SUPERBLOCK_SIZE];
    read_exact_at(file, SUPERBLOCK_OFFSET, &mut raw)?;
    let magic = read_u32(&raw, 0)?;
    if magic != EROFS_MAGIC {
        return Err(BigPclusterError::BadMagic(magic));
    }
    if raw[0x0c] != 12 {
        return Err(BigPclusterError::UnsupportedFilesystem(
            "big-pcluster proof supports only 4 KiB EROFS blocks",
        ));
    }
    let incompat = read_u32(&raw, 0x50)?;
    if incompat & !REQUIRED_INCOMPAT != 0 || incompat & REQUIRED_INCOMPAT != REQUIRED_INCOMPAT {
        return Err(BigPclusterError::UnsupportedFilesystem(
            "big-pcluster proof requires only LZ4_0PADDING + big-pcluster incompatible features",
        ));
    }
    if raw[0x5a] != 0 || read_u16(&raw, 0x56)? != 0 {
        return Err(BigPclusterError::UnsupportedFilesystem(
            "big-pcluster proof requires primary-device core directories",
        ));
    }
    Ok(Superblock {
        root_nid: u64::from(read_u16(&raw, 0x0e)?),
        meta_block: u64::from(read_u32(&raw, 0x28)?),
    })
}

fn xattr_ibody_size(count: u16) -> Result<u64, BigPclusterError> {
    if count == 0 {
        return Ok(0);
    }
    12_u64
        .checked_add(
            u64::from(count - 1)
                .checked_mul(4)
                .ok_or(BigPclusterError::ArithmeticOverflow)?,
        )
        .ok_or(BigPclusterError::ArithmeticOverflow)
}

fn find_in_directory_block(block: &[u8], target: &[u8]) -> Result<Option<u64>, BigPclusterError> {
    if block.len() < DIRENT_SIZE {
        return Err(BigPclusterError::CorruptDirectory);
    }
    let first_name_offset = usize::from(read_u16(block, 8)?);
    if first_name_offset == 0
        || first_name_offset % DIRENT_SIZE != 0
        || first_name_offset > block.len()
    {
        return Err(BigPclusterError::CorruptDirectory);
    }
    let count = first_name_offset / DIRENT_SIZE;
    for index in 0..count {
        let entry = index
            .checked_mul(DIRENT_SIZE)
            .ok_or(BigPclusterError::ArithmeticOverflow)?;
        let name_offset = usize::from(read_u16(block, entry + 8)?);
        if name_offset < first_name_offset || name_offset >= block.len() {
            return Err(BigPclusterError::CorruptDirectory);
        }
        let name_end = if index + 1 < count {
            usize::from(read_u16(block, entry + DIRENT_SIZE + 8)?)
        } else {
            let tail = block
                .get(name_offset..)
                .ok_or(BigPclusterError::CorruptDirectory)?;
            name_offset
                .checked_add(
                    tail.iter()
                        .position(|byte| *byte == 0)
                        .unwrap_or(tail.len()),
                )
                .ok_or(BigPclusterError::ArithmeticOverflow)?
        };
        if name_end < name_offset || name_end > block.len() {
            return Err(BigPclusterError::CorruptDirectory);
        }
        if block
            .get(name_offset..name_end)
            .ok_or(BigPclusterError::CorruptDirectory)?
            == target
        {
            return Ok(Some(read_u64(block, entry)?));
        }
    }
    Ok(None)
}

fn parse_absolute_path(path: &str) -> Result<Vec<&str>, BigPclusterError> {
    if !path.starts_with('/') {
        return Err(BigPclusterError::InvalidPath("path must be absolute"));
    }
    if path == "/" {
        return Ok(Vec::new());
    }
    let mut components = Vec::new();
    for component in path[1..].split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return Err(BigPclusterError::InvalidPath(
                "empty, dot and parent components are forbidden",
            ));
        }
        components.push(component);
    }
    Ok(components)
}

fn align8(value: u64) -> Result<u64, BigPclusterError> {
    value
        .checked_add(7)
        .map(|rounded| rounded & !7_u64)
        .ok_or(BigPclusterError::ArithmeticOverflow)
}

fn div_ceil(value: u64, divisor: u64) -> Result<u64, BigPclusterError> {
    value
        .checked_add(divisor.saturating_sub(1))
        .map(|rounded| rounded / divisor)
        .ok_or(BigPclusterError::ArithmeticOverflow)
}

fn ensure_range(bytes: u64, offset: u64, length: u64) -> Result<(), BigPclusterError> {
    let end = offset
        .checked_add(length)
        .ok_or(BigPclusterError::ArithmeticOverflow)?;
    if end > bytes {
        return Err(BigPclusterError::UnexpectedEndOfImage);
    }
    Ok(())
}

fn read_exact_at(file: &mut File, offset: u64, buffer: &mut [u8]) -> Result<(), BigPclusterError> {
    file.seek(SeekFrom::Start(offset))
        .map_err(BigPclusterError::Io)?;
    file.read_exact(buffer).map_err(BigPclusterError::Io)
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, BigPclusterError> {
    let end = offset
        .checked_add(2)
        .ok_or(BigPclusterError::ArithmeticOverflow)?;
    let raw: [u8; 2] = bytes
        .get(offset..end)
        .ok_or(BigPclusterError::UnexpectedEndOfStructure)?
        .try_into()
        .map_err(|_| BigPclusterError::UnexpectedEndOfStructure)?;
    Ok(u16::from_le_bytes(raw))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, BigPclusterError> {
    let end = offset
        .checked_add(4)
        .ok_or(BigPclusterError::ArithmeticOverflow)?;
    let raw: [u8; 4] = bytes
        .get(offset..end)
        .ok_or(BigPclusterError::UnexpectedEndOfStructure)?
        .try_into()
        .map_err(|_| BigPclusterError::UnexpectedEndOfStructure)?;
    Ok(u32::from_le_bytes(raw))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, BigPclusterError> {
    let end = offset
        .checked_add(8)
        .ok_or(BigPclusterError::ArithmeticOverflow)?;
    let raw: [u8; 8] = bytes
        .get(offset..end)
        .ok_or(BigPclusterError::UnexpectedEndOfStructure)?
        .try_into()
        .map_err(|_| BigPclusterError::UnexpectedEndOfStructure)?;
    Ok(u64::from_le_bytes(raw))
}

#[derive(Debug)]
pub enum BigPclusterError {
    Io(io::Error),
    View(ViewError),
    BadMagic(u32),
    InvalidFilesystem(&'static str),
    UnsupportedFilesystem(&'static str),
    UnsupportedInode(&'static str),
    IncompatibleReplacement(&'static str),
    ReplacementSizeMismatch { expected: u64, actual: u64 },
    CompressionDoesNotFit { encoded: usize, capacity: usize },
    CompressionValidationFailed,
    InvalidPath(&'static str),
    PathNotFound(String),
    NotDirectory(u64),
    NotRegularFile(u64),
    CorruptDirectory,
    UnexpectedEndOfImage,
    UnexpectedEndOfStructure,
    ArithmeticOverflow,
}

impl fmt::Display for BigPclusterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "EROFS big-pcluster I/O error: {error}"),
            Self::View(error) => write!(f, "Loom effective-view error: {error}"),
            Self::BadMagic(magic) => write!(f, "invalid EROFS magic {magic:#010x}"),
            Self::InvalidFilesystem(reason) => write!(f, "invalid big-pcluster EROFS: {reason}"),
            Self::UnsupportedFilesystem(reason) => {
                write!(f, "unsupported big-pcluster EROFS: {reason}")
            }
            Self::UnsupportedInode(reason) => write!(f, "unsupported big-pcluster inode: {reason}"),
            Self::IncompatibleReplacement(reason) => {
                write!(f, "incompatible big-pcluster replacement: {reason}")
            }
            Self::ReplacementSizeMismatch { expected, actual } => write!(
                f,
                "replacement size mismatch: expected {expected} bytes, got {actual}"
            ),
            Self::CompressionDoesNotFit { encoded, capacity } => write!(
                f,
                "raw LZ4 block does not fit existing big pcluster: encoded {encoded} bytes, capacity {capacity}"
            ),
            Self::CompressionValidationFailed => {
                write!(f, "big-pcluster raw LZ4 round-trip validation failed")
            }
            Self::InvalidPath(reason) => write!(f, "invalid EROFS path: {reason}"),
            Self::PathNotFound(name) => write!(f, "EROFS path component not found: {name:?}"),
            Self::NotDirectory(nid) => write!(f, "EROFS nid {nid} is not a directory"),
            Self::NotRegularFile(nid) => write!(f, "EROFS nid {nid} is not a regular file"),
            Self::CorruptDirectory => write!(f, "malformed EROFS directory block"),
            Self::UnexpectedEndOfImage => write!(f, "EROFS reference lies beyond image bytes"),
            Self::UnexpectedEndOfStructure => write!(f, "unexpected end of EROFS structure"),
            Self::ArithmeticOverflow => {
                write!(f, "integer overflow while parsing big-pcluster EROFS")
            }
        }
    }
}

impl std::error::Error for BigPclusterError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::View(error) => Some(error),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cblkcnt_marker_is_bit_11_of_compact_low_field() {
        assert_eq!(CBLKCNT, 0x0800);
        assert_eq!((CBLKCNT | 2) & !CBLKCNT, 2);
    }

    #[test]
    fn one_head_two_block_extent_accepts_cblkcnt() {
        let entries = vec![
            CompactEntry {
                kind: LCLUSTER_HEAD1,
                low: 0,
                slot: 0,
                slots: 2,
                base_pblk: 100,
            },
            CompactEntry {
                kind: LCLUSTER_NONHEAD,
                low: CBLKCNT | 2,
                slot: 1,
                slots: 2,
                base_pblk: 100,
            },
            CompactEntry {
                kind: LCLUSTER_NONHEAD,
                low: 2,
                slot: 0,
                slots: 2,
                base_pblk: 102,
            },
        ];
        assert!(validate_big_single_extent(&entries, 3).is_ok());
    }

    #[test]
    fn eight_kib_0padding_span_round_trips() {
        let mut input = vec![b'P'; 32768];
        input[64..84].copy_from_slice(b"LOOM-STAGE20-BIG-LZ4");
        let encoded = lz4::encode(&input).unwrap();
        assert!(encoded.len() < 8192);
        let mut span = vec![0_u8; 8192];
        let start = span.len() - encoded.len();
        span[start..].copy_from_slice(&encoded);
        assert_eq!(lz4::decode_0padding(&span, input.len()).unwrap(), input);
    }
}

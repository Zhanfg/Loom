#![forbid(unsafe_code)]

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
const LZ4_ALGORITHM: u8 = 0;
const FEATURE_LZ4_0PADDING: u32 = 0x0000_0001;
const MAX_SINGLE_EXTENT_LCLUSTERS: usize = 2048;

const LZ4_MIN_MATCH: usize = 4;
const LZ4_LAST_LITERALS: usize = 5;
const LZ4_MFLIMIT: usize = 12;
const LZ4_HASH_LOG: u32 = 16;
const LZ4_HASH_SIZE: usize = 1 << LZ4_HASH_LOG;

#[derive(Debug)]
pub struct CompiledSwap {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Extent {
    nid: u64,
    logical_size: u64,
    pcluster: u64,
    algorithm: u8,
    advise: u16,
    logical_lclusters: usize,
    compact_2b_entries: usize,
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

struct Image {
    file: File,
    bytes: u64,
    sb: Superblock,
}

/// Swaps one complete encoded pcluster for a single-extent compact EROFS file.
///
/// The supported proof shape uses 4 KiB blocks/lclusters, `COMPRESSED_COMPACT`, LZ4,
/// `COMPACTED_2B`, one physical pcluster and one logical extent beginning at lcluster 0.
/// The compact index stream may contain initial/trailing 4-byte entries and one or more
/// true 16-entry 2-byte packs. Compact metadata itself is never modified.
///
/// # Errors
/// Returns [`IndexError`] for unsupported/corrupt layouts, incompatible replacement
/// images, ordinary I/O failures, or effective-view compilation failures.
pub fn compile_pcluster_swap(
    origin_path: &Path,
    target_path: &str,
    replacement_image_path: &Path,
) -> Result<CompiledSwap, IndexError> {
    let mut origin = Image::open(origin_path)?;
    let mut replacement = Image::open(replacement_image_path)?;
    let origin_nid = origin.resolve_path(target_path)?;
    let replacement_nid = replacement.resolve_path(target_path)?;
    let origin_extent = origin.read_single_pcluster_extent(origin_nid)?;
    let replacement_extent = replacement.read_single_pcluster_extent(replacement_nid)?;

    if origin_extent.logical_size != replacement_extent.logical_size {
        return Err(IndexError::IncompatibleReplacement(
            "logical file sizes differ",
        ));
    }
    if origin_extent.algorithm != replacement_extent.algorithm {
        return Err(IndexError::IncompatibleReplacement(
            "compression algorithms differ",
        ));
    }
    if origin_extent.advise != replacement_extent.advise {
        return Err(IndexError::IncompatibleReplacement(
            "compact map advice differs",
        ));
    }
    if origin_extent.logical_lclusters != replacement_extent.logical_lclusters {
        return Err(IndexError::IncompatibleReplacement(
            "logical compact-index lengths differ",
        ));
    }

    let encoded = replacement.read_block(replacement_extent.pcluster)?;
    compile_shadow(
        origin_path,
        origin_extent,
        &encoded,
        replacement_extent.pcluster,
        BLOCK_BYTES,
    )
}

impl CompiledSwap {
    /// Encodes a plain replacement payload into the existing compact `0PADDING` LZ4 pcluster.
    ///
    /// The encoded LZ4 block is right-aligned in the 4 KiB physical pcluster and the
    /// leading bytes are zero-filled. Compact indexes remain authoritative and untouched.
    ///
    /// # Errors
    /// Returns [`IndexError`] for unsupported compact metadata, replacement-size mismatch,
    /// LZ4 footprint overflow, codec self-validation failure, I/O, or view errors.
    pub fn compile_lz4_replacement(
        origin_path: &Path,
        target_path: &str,
        replacement_path: &Path,
    ) -> Result<Self, IndexError> {
        let replacement = fs::read(replacement_path).map_err(IndexError::Io)?;
        let mut origin = Image::open(origin_path)?;
        let origin_nid = origin.resolve_path(target_path)?;
        let extent = origin.read_single_pcluster_extent(origin_nid)?;
        let actual = u64::try_from(replacement.len()).map_err(|_| IndexError::ArithmeticOverflow)?;
        if actual != extent.logical_size {
            return Err(IndexError::ReplacementSizeMismatch {
                expected: extent.logical_size,
                actual,
            });
        }

        let compressed = encode_lz4_block(&replacement)?;
        if compressed.len() > BLOCK_BYTES {
            return Err(IndexError::CompressionDoesNotFit {
                encoded: compressed.len(),
                capacity: BLOCK_BYTES,
            });
        }
        if compressed.first().copied().unwrap_or(0) == 0 {
            return Err(IndexError::CompressionValidationFailed);
        }
        if decode_lz4_block(&compressed, replacement.len())? != replacement {
            return Err(IndexError::CompressionValidationFailed);
        }

        let mut pcluster = vec![0_u8; BLOCK_BYTES];
        let start = BLOCK_BYTES
            .checked_sub(compressed.len())
            .ok_or(IndexError::ArithmeticOverflow)?;
        pcluster[start..].copy_from_slice(&compressed);
        if decode_0padding_pcluster(&pcluster, replacement.len())? != replacement {
            return Err(IndexError::CompressionValidationFailed);
        }

        compile_shadow(
            origin_path,
            extent,
            &pcluster,
            extent.pcluster,
            compressed.len(),
        )
    }
}

fn compile_shadow(
    origin_path: &Path,
    extent: Extent,
    encoded_block: &[u8],
    replacement_pcluster: u64,
    encoded_bytes: usize,
) -> Result<CompiledSwap, IndexError> {
    if encoded_block.len() != BLOCK_BYTES {
        return Err(IndexError::IncompatibleReplacement(
            "encoded replacement must occupy exactly one filesystem block",
        ));
    }
    let mut view = EffectiveBlockStore::open(origin_path, BLOCK_SIZE).map_err(IndexError::View)?;
    view.block_mut(extent.pcluster)
        .map_err(IndexError::View)?
        .copy_from_slice(encoded_block);
    let compiled = view.finalize().map_err(IndexError::View)?;
    if compiled.shadow_blocks != 1 {
        return Err(IndexError::IncompatibleReplacement(
            "compact one-pcluster operation did not produce exactly one shadow block",
        ));
    }

    Ok(CompiledSwap {
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
    fn open(path: &Path) -> Result<Self, IndexError> {
        let mut file = File::open(path).map_err(IndexError::Io)?;
        let bytes = file.metadata().map_err(IndexError::Io)?.len();
        let sb = read_superblock(&mut file, bytes)?;
        Ok(Self { file, bytes, sb })
    }

    fn read_inode(&mut self, nid: u64) -> Result<Inode, IndexError> {
        let metadata_base = self
            .sb
            .meta_block
            .checked_mul(u64::from(BLOCK_SIZE))
            .ok_or(IndexError::ArithmeticOverflow)?;
        let offset = metadata_base
            .checked_add(nid.checked_mul(32).ok_or(IndexError::ArithmeticOverflow)?)
            .ok_or(IndexError::ArithmeticOverflow)?;
        ensure_range(self.bytes, offset, 32)?;

        let mut compact = [0_u8; 32];
        read_exact_at(&mut self.file, offset, &mut compact)?;
        let format = read_u16(&compact, 0)?;
        if format & !0x1f != 0 {
            return Err(IndexError::UnsupportedInode(
                "unknown EROFS inode format bits",
            ));
        }
        let extended = format & 1 != 0;
        let layout = u8::try_from((format >> 1) & 7).map_err(|_| IndexError::ArithmeticOverflow)?;
        if layout > 4 {
            return Err(IndexError::UnsupportedInode(
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

    fn resolve_path(&mut self, path: &str) -> Result<u64, IndexError> {
        let components = parse_absolute_path(path)?;
        let mut current = self.sb.root_nid;
        for component in components {
            let inode = self.read_inode(current)?;
            if inode.file_type() != MODE_DIRECTORY {
                return Err(IndexError::NotDirectory(current));
            }
            current = self.find_child(&inode, component.as_bytes())?;
        }
        Ok(current)
    }

    fn find_child(&mut self, directory: &Inode, name: &[u8]) -> Result<u64, IndexError> {
        if directory.xattr_size != 0 {
            return Err(IndexError::UnsupportedInode(
                "compact path traversal refuses directory xattrs",
            ));
        }
        let block_size = u64::from(BLOCK_SIZE);
        let full_blocks = match directory.layout {
            DATA_FLAT_PLAIN => div_ceil(directory.size, block_size)?,
            DATA_FLAT_INLINE => directory.size / block_size,
            _ => {
                return Err(IndexError::UnsupportedInode(
                    "compact path traversal requires flat directories",
                ))
            }
        };

        for index in 0..full_blocks {
            let block = u64::from(directory.data_word)
                .checked_add(index)
                .ok_or(IndexError::ArithmeticOverflow)?;
            let bytes = self.read_block(block)?;
            if let Some(nid) = find_in_directory_block(&bytes, name)? {
                return Ok(nid);
            }
        }
        if directory.layout == DATA_FLAT_INLINE && directory.size % block_size != 0 {
            return Err(IndexError::UnsupportedInode(
                "target may lie in unsupported inline directory tail",
            ));
        }
        Err(IndexError::PathNotFound(
            String::from_utf8_lossy(name).into_owned(),
        ))
    }

    fn read_single_pcluster_extent(&mut self, nid: u64) -> Result<Extent, IndexError> {
        let inode = self.read_inode(nid)?;
        let logical_lclusters = self.validate_target_inode(&inode)?;
        let header_offset = align8(
            inode
                .offset
                .checked_add(inode.isize)
                .and_then(|value| value.checked_add(inode.xattr_size))
                .ok_or(IndexError::ArithmeticOverflow)?,
        )?;
        ensure_range(self.bytes, header_offset, MAP_HEADER_SIZE)?;

        let mut header = [0_u8; 8];
        read_exact_at(&mut self.file, header_offset, &mut header)?;
        let advise = read_u16(&header, 4)?;
        if advise != ADVISE_COMPACTED_2B {
            return Err(IndexError::UnsupportedInode(
                "compact proof requires only COMPACTED_2B advice",
            ));
        }
        let algorithm = header[6] & 0x0f;
        if algorithm != LZ4_ALGORITHM || header[6] >> 4 != 0 {
            return Err(IndexError::UnsupportedInode(
                "compact proof supports only HEAD1 LZ4",
            ));
        }
        if header[7] != 0 {
            return Err(IndexError::UnsupportedInode(
                "compact proof requires 4 KiB logical clusters without packed fragments",
            ));
        }

        let ebase = header_offset
            .checked_add(MAP_HEADER_SIZE)
            .ok_or(IndexError::ArithmeticOverflow)?;
        let regions = compact_regions(ebase, logical_lclusters)?;
        let pcluster = self.validate_single_extent_indexes(ebase, logical_lclusters)?;
        Ok(Extent {
            nid,
            logical_size: inode.size,
            pcluster,
            algorithm,
            advise,
            logical_lclusters,
            compact_2b_entries: regions.compact_2b,
        })
    }

    fn validate_target_inode(&self, inode: &Inode) -> Result<usize, IndexError> {
        if inode.file_type() != MODE_REGULAR {
            return Err(IndexError::NotRegularFile(inode.nid));
        }
        if inode.layout != DATA_COMPRESSED_COMPACT {
            return Err(IndexError::UnsupportedInode(
                "compact proof requires EROFS_INODE_COMPRESSED_COMPACT",
            ));
        }
        if inode.xattr_size != 0 {
            return Err(IndexError::UnsupportedInode(
                "compact target must not carry xattrs",
            ));
        }
        if inode.size < u64::from(BLOCK_SIZE) * 2 || inode.size % u64::from(BLOCK_SIZE) != 0 {
            return Err(IndexError::UnsupportedInode(
                "compact proof requires a whole-block file of at least two lclusters",
            ));
        }
        let logical_lclusters = usize::try_from(inode.size / u64::from(BLOCK_SIZE))
            .map_err(|_| IndexError::ArithmeticOverflow)?;
        if logical_lclusters > MAX_SINGLE_EXTENT_LCLUSTERS {
            return Err(IndexError::UnsupportedInode(
                "single-pcluster proof caps lookback distance below CBLKCNT encoding",
            ));
        }
        if inode.data_word != 1 {
            return Err(IndexError::UnsupportedInode(
                "compact proof requires exactly one encoded physical block",
            ));
        }
        Ok(logical_lclusters)
    }

    fn validate_single_extent_indexes(
        &mut self,
        ebase: u64,
        total: usize,
    ) -> Result<u64, IndexError> {
        let head = self.read_compact_entry(ebase, total, 0)?;
        if head.kind != LCLUSTER_HEAD1 || head.low != 0 || head.slot != 0 {
            return Err(IndexError::UnsupportedInode(
                "single compact extent must begin with slot-0 HEAD1 at offset zero",
            ));
        }
        let pcluster = head
            .base_pblk
            .checked_add(1)
            .ok_or(IndexError::ArithmeticOverflow)?;
        if pcluster >= self.bytes / u64::from(BLOCK_SIZE) {
            return Err(IndexError::InvalidFilesystem(
                "compact pcluster lies beyond image",
            ));
        }

        for lcn in 1..total {
            let entry = self.read_compact_entry(ebase, total, lcn)?;
            if entry.kind != LCLUSTER_NONHEAD {
                return Err(IndexError::UnsupportedInode(
                    "single-pcluster compact proof requires NONHEAD after lcluster zero",
                ));
            }
            let expected = if entry.slot + 1 == entry.slots {
                total
                    .checked_sub(lcn)
                    .ok_or(IndexError::ArithmeticOverflow)?
            } else {
                lcn
            };
            let expected = u16::try_from(expected).map_err(|_| IndexError::ArithmeticOverflow)?;
            if entry.low != expected {
                return Err(IndexError::UnsupportedInode(
                    "compact NONHEAD lookback/lookahead value does not match one-head extent",
                ));
            }
        }
        Ok(pcluster)
    }

    fn read_compact_entry(
        &mut self,
        ebase: u64,
        total: usize,
        lcn: usize,
    ) -> Result<CompactEntry, IndexError> {
        if lcn >= total {
            return Err(IndexError::InvalidFilesystem(
                "compact lcluster index lies beyond logical file",
            ));
        }
        let regions = compact_regions(ebase, total)?;
        let (shift, pos) = compact_entry_position(ebase, regions, lcn)?;
        let entry_bytes = 1_usize
            .checked_shl(shift)
            .ok_or(IndexError::ArithmeticOverflow)?;
        let slots = if entry_bytes == 4 { 2 } else { 16 };
        let pack_bytes = entry_bytes
            .checked_mul(slots)
            .ok_or(IndexError::ArithmeticOverflow)?;
        let pack_bytes_u64 = u64::try_from(pack_bytes).map_err(|_| IndexError::ArithmeticOverflow)?;
        let pack_start = pos - (pos % pack_bytes_u64);
        ensure_range(self.bytes, pack_start, pack_bytes_u64)?;
        let mut pack = vec![0_u8; pack_bytes];
        read_exact_at(&mut self.file, pack_start, &mut pack)?;

        let slot = usize::try_from((pos - pack_start) / u64::try_from(entry_bytes)
            .map_err(|_| IndexError::ArithmeticOverflow)?)
            .map_err(|_| IndexError::ArithmeticOverflow)?;
        if slot >= slots {
            return Err(IndexError::InvalidFilesystem(
                "compact entry slot lies beyond pack",
            ));
        }
        let encode_bits = (pack_bytes
            .checked_mul(8)
            .and_then(|bits| bits.checked_sub(32))
            .ok_or(IndexError::ArithmeticOverflow)?)
            / slots;
        let bit_pos = encode_bits
            .checked_mul(slot)
            .ok_or(IndexError::ArithmeticOverflow)?;
        let byte_pos = bit_pos / 8;
        let word = read_u32(&pack, byte_pos)? >> (bit_pos & 7);
        let low = u16::try_from(word & u32::from(OFFSET_MASK))
            .map_err(|_| IndexError::ArithmeticOverflow)?;
        let kind = u16::try_from((word >> BLOCK_BITS) & u32::from(LCLUSTER_TYPE_MASK))
            .map_err(|_| IndexError::ArithmeticOverflow)?;
        let base_pblk = u64::from(read_u32(&pack, pack_bytes - 4)?);
        Ok(CompactEntry {
            kind,
            low,
            slot,
            slots,
            base_pblk,
        })
    }

    fn read_block(&mut self, block: u64) -> Result<Vec<u8>, IndexError> {
        let block_size = u64::from(BLOCK_SIZE);
        let offset = block
            .checked_mul(block_size)
            .ok_or(IndexError::ArithmeticOverflow)?;
        ensure_range(self.bytes, offset, block_size)?;
        let mut bytes = vec![0_u8; BLOCK_BYTES];
        read_exact_at(&mut self.file, offset, &mut bytes)?;
        Ok(bytes)
    }
}

fn compact_regions(ebase: u64, total: usize) -> Result<CompactRegions, IndexError> {
    let modulo = usize::try_from(ebase % 32).map_err(|_| IndexError::ArithmeticOverflow)?;
    let mut initial_4b = (32_usize
        .checked_sub(modulo)
        .ok_or(IndexError::ArithmeticOverflow)?)
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
) -> Result<(u32, u64), IndexError> {
    if lcn < regions.initial_4b {
        let delta = u64::try_from(lcn.checked_mul(4).ok_or(IndexError::ArithmeticOverflow)?)
            .map_err(|_| IndexError::ArithmeticOverflow)?;
        return Ok((2, ebase.checked_add(delta).ok_or(IndexError::ArithmeticOverflow)?));
    }

    let initial_bytes = u64::try_from(
        regions
            .initial_4b
            .checked_mul(4)
            .ok_or(IndexError::ArithmeticOverflow)?,
    )
    .map_err(|_| IndexError::ArithmeticOverflow)?;
    let mut pos = ebase
        .checked_add(initial_bytes)
        .ok_or(IndexError::ArithmeticOverflow)?;
    let relative = lcn
        .checked_sub(regions.initial_4b)
        .ok_or(IndexError::ArithmeticOverflow)?;
    if relative < regions.compact_2b {
        let delta = u64::try_from(relative.checked_mul(2).ok_or(IndexError::ArithmeticOverflow)?)
            .map_err(|_| IndexError::ArithmeticOverflow)?;
        return Ok((1, pos.checked_add(delta).ok_or(IndexError::ArithmeticOverflow)?));
    }

    let compact_bytes = u64::try_from(
        regions
            .compact_2b
            .checked_mul(2)
            .ok_or(IndexError::ArithmeticOverflow)?,
    )
    .map_err(|_| IndexError::ArithmeticOverflow)?;
    pos = pos
        .checked_add(compact_bytes)
        .ok_or(IndexError::ArithmeticOverflow)?;
    let trailing = relative
        .checked_sub(regions.compact_2b)
        .ok_or(IndexError::ArithmeticOverflow)?;
    let delta = u64::try_from(trailing.checked_mul(4).ok_or(IndexError::ArithmeticOverflow)?)
        .map_err(|_| IndexError::ArithmeticOverflow)?;
    Ok((2, pos.checked_add(delta).ok_or(IndexError::ArithmeticOverflow)?))
}

fn encode_lz4_block(input: &[u8]) -> Result<Vec<u8>, IndexError> {
    let mut output = Vec::with_capacity(input.len());
    let mut table = vec![usize::MAX; LZ4_HASH_SIZE];
    let mut anchor = 0_usize;
    let mut cursor = 0_usize;
    let last_match_start = input.len().saturating_sub(LZ4_MFLIMIT);
    let match_end = input.len().saturating_sub(LZ4_LAST_LITERALS);

    while cursor <= last_match_start && cursor + LZ4_MIN_MATCH <= input.len() {
        let hash = lz4_hash(input, cursor)?;
        let candidate = table[hash];
        table[hash] = cursor;
        let valid = candidate != usize::MAX
            && cursor > candidate
            && cursor - candidate <= usize::from(u16::MAX)
            && input[candidate..candidate + LZ4_MIN_MATCH]
                == input[cursor..cursor + LZ4_MIN_MATCH];
        if !valid {
            cursor += 1;
            continue;
        }

        let mut match_len = LZ4_MIN_MATCH;
        while cursor + match_len < match_end
            && input[candidate + match_len] == input[cursor + match_len]
        {
            match_len += 1;
        }
        emit_lz4_sequence(
            &mut output,
            input,
            anchor,
            cursor,
            candidate,
            match_len,
        )?;

        let next = cursor
            .checked_add(match_len)
            .ok_or(IndexError::ArithmeticOverflow)?;
        let mut update = cursor + 1;
        while update < next && update <= last_match_start {
            table[lz4_hash(input, update)?] = update;
            update += 1;
        }
        cursor = next;
        anchor = next;
    }

    emit_lz4_last_literals(&mut output, &input[anchor..])?;
    Ok(output)
}

fn lz4_hash(input: &[u8], offset: usize) -> Result<usize, IndexError> {
    let end = offset
        .checked_add(LZ4_MIN_MATCH)
        .ok_or(IndexError::ArithmeticOverflow)?;
    let bytes: [u8; 4] = input
        .get(offset..end)
        .ok_or(IndexError::CompressionValidationFailed)?
        .try_into()
        .map_err(|_| IndexError::CompressionValidationFailed)?;
    let value = u32::from_le_bytes(bytes).wrapping_mul(2_654_435_761);
    usize::try_from(value >> (32 - LZ4_HASH_LOG)).map_err(|_| IndexError::ArithmeticOverflow)
}

fn emit_lz4_sequence(
    output: &mut Vec<u8>,
    input: &[u8],
    anchor: usize,
    match_start: usize,
    match_ref: usize,
    match_len: usize,
) -> Result<(), IndexError> {
    let literal_len = match_start
        .checked_sub(anchor)
        .ok_or(IndexError::ArithmeticOverflow)?;
    let match_code = match_len
        .checked_sub(LZ4_MIN_MATCH)
        .ok_or(IndexError::ArithmeticOverflow)?;
    let token = u8::try_from((literal_len.min(15) << 4) | match_code.min(15))
        .map_err(|_| IndexError::ArithmeticOverflow)?;
    output.push(token);
    if literal_len >= 15 {
        emit_lz4_length(output, literal_len - 15)?;
    }
    output.extend_from_slice(
        input
            .get(anchor..match_start)
            .ok_or(IndexError::CompressionValidationFailed)?,
    );

    let offset = match_start
        .checked_sub(match_ref)
        .ok_or(IndexError::ArithmeticOverflow)?;
    let offset = u16::try_from(offset).map_err(|_| IndexError::CompressionValidationFailed)?;
    if offset == 0 {
        return Err(IndexError::CompressionValidationFailed);
    }
    output.extend_from_slice(&offset.to_le_bytes());
    if match_code >= 15 {
        emit_lz4_length(output, match_code - 15)?;
    }
    Ok(())
}

fn emit_lz4_last_literals(output: &mut Vec<u8>, literals: &[u8]) -> Result<(), IndexError> {
    output.push(
        u8::try_from(literals.len().min(15) << 4).map_err(|_| IndexError::ArithmeticOverflow)?,
    );
    if literals.len() >= 15 {
        emit_lz4_length(output, literals.len() - 15)?;
    }
    output.extend_from_slice(literals);
    Ok(())
}

fn emit_lz4_length(output: &mut Vec<u8>, mut length: usize) -> Result<(), IndexError> {
    while length >= 255 {
        output.push(255);
        length -= 255;
    }
    output.push(u8::try_from(length).map_err(|_| IndexError::ArithmeticOverflow)?);
    Ok(())
}

fn decode_0padding_pcluster(pcluster: &[u8], expected: usize) -> Result<Vec<u8>, IndexError> {
    let start = pcluster
        .iter()
        .position(|byte| *byte != 0)
        .ok_or(IndexError::CompressionValidationFailed)?;
    decode_lz4_block(&pcluster[start..], expected)
}

fn decode_lz4_block(encoded: &[u8], expected: usize) -> Result<Vec<u8>, IndexError> {
    let mut input_pos = 0_usize;
    let mut output = Vec::with_capacity(expected);
    while input_pos < encoded.len() {
        let token = encoded[input_pos];
        input_pos += 1;

        let mut literal_len = usize::from(token >> 4);
        if literal_len == 15 {
            literal_len = literal_len
                .checked_add(read_lz4_length(encoded, &mut input_pos)?)
                .ok_or(IndexError::ArithmeticOverflow)?;
        }
        let literal_end = input_pos
            .checked_add(literal_len)
            .ok_or(IndexError::ArithmeticOverflow)?;
        let literals = encoded
            .get(input_pos..literal_end)
            .ok_or(IndexError::CompressionValidationFailed)?;
        if output.len().saturating_add(literals.len()) > expected {
            return Err(IndexError::CompressionValidationFailed);
        }
        output.extend_from_slice(literals);
        input_pos = literal_end;
        if input_pos == encoded.len() {
            break;
        }

        let offset_end = input_pos
            .checked_add(2)
            .ok_or(IndexError::ArithmeticOverflow)?;
        let raw_offset: [u8; 2] = encoded
            .get(input_pos..offset_end)
            .ok_or(IndexError::CompressionValidationFailed)?
            .try_into()
            .map_err(|_| IndexError::CompressionValidationFailed)?;
        input_pos = offset_end;
        let offset = usize::from(u16::from_le_bytes(raw_offset));
        if offset == 0 || offset > output.len() {
            return Err(IndexError::CompressionValidationFailed);
        }

        let mut match_len = usize::from(token & 0x0f) + LZ4_MIN_MATCH;
        if token & 0x0f == 15 {
            match_len = match_len
                .checked_add(read_lz4_length(encoded, &mut input_pos)?)
                .ok_or(IndexError::ArithmeticOverflow)?;
        }
        if output.len().saturating_add(match_len) > expected {
            return Err(IndexError::CompressionValidationFailed);
        }
        for _ in 0..match_len {
            let source = output
                .len()
                .checked_sub(offset)
                .ok_or(IndexError::CompressionValidationFailed)?;
            let byte = *output
                .get(source)
                .ok_or(IndexError::CompressionValidationFailed)?;
            output.push(byte);
        }
    }
    if output.len() != expected {
        return Err(IndexError::CompressionValidationFailed);
    }
    Ok(output)
}

fn read_lz4_length(encoded: &[u8], input_pos: &mut usize) -> Result<usize, IndexError> {
    let mut total = 0_usize;
    loop {
        let byte = *encoded
            .get(*input_pos)
            .ok_or(IndexError::CompressionValidationFailed)?;
        *input_pos = (*input_pos)
            .checked_add(1)
            .ok_or(IndexError::ArithmeticOverflow)?;
        total = total
            .checked_add(usize::from(byte))
            .ok_or(IndexError::ArithmeticOverflow)?;
        if byte != 255 {
            return Ok(total);
        }
    }
}

fn read_superblock(file: &mut File, bytes: u64) -> Result<Superblock, IndexError> {
    ensure_range(
        bytes,
        SUPERBLOCK_OFFSET,
        u64::try_from(SUPERBLOCK_SIZE).map_err(|_| IndexError::ArithmeticOverflow)?,
    )?;
    let mut raw = [0_u8; SUPERBLOCK_SIZE];
    read_exact_at(file, SUPERBLOCK_OFFSET, &mut raw)?;
    let magic = read_u32(&raw, 0)?;
    if magic != EROFS_MAGIC {
        return Err(IndexError::BadMagic(magic));
    }
    if raw[0x0c] != 12 {
        return Err(IndexError::UnsupportedFilesystem(
            "compact proof supports only 4 KiB EROFS blocks",
        ));
    }
    let incompat = read_u32(&raw, 0x50)?;
    if incompat & !FEATURE_LZ4_0PADDING != 0 {
        return Err(IndexError::UnsupportedFilesystem(
            "compact image enables unsupported incompatible EROFS features",
        ));
    }
    if incompat & FEATURE_LZ4_0PADDING == 0 {
        return Err(IndexError::UnsupportedFilesystem(
            "compact proof expects normal LZ4_0PADDING layout",
        ));
    }
    if raw[0x5a] != 0 || read_u16(&raw, 0x56)? != 0 {
        return Err(IndexError::UnsupportedFilesystem(
            "compact proof requires primary-device core directories",
        ));
    }

    Ok(Superblock {
        root_nid: u64::from(read_u16(&raw, 0x0e)?),
        meta_block: u64::from(read_u32(&raw, 0x28)?),
    })
}

fn xattr_ibody_size(count: u16) -> Result<u64, IndexError> {
    if count == 0 {
        return Ok(0);
    }
    12_u64
        .checked_add(
            u64::from(count - 1)
                .checked_mul(4)
                .ok_or(IndexError::ArithmeticOverflow)?,
        )
        .ok_or(IndexError::ArithmeticOverflow)
}

fn find_in_directory_block(block: &[u8], target: &[u8]) -> Result<Option<u64>, IndexError> {
    if block.len() < DIRENT_SIZE {
        return Err(IndexError::CorruptDirectory);
    }
    let first_name_offset = usize::from(read_u16(block, 8)?);
    if first_name_offset == 0
        || first_name_offset % DIRENT_SIZE != 0
        || first_name_offset > block.len()
    {
        return Err(IndexError::CorruptDirectory);
    }
    let count = first_name_offset / DIRENT_SIZE;
    for index in 0..count {
        let entry = index
            .checked_mul(DIRENT_SIZE)
            .ok_or(IndexError::ArithmeticOverflow)?;
        let name_offset = usize::from(read_u16(block, entry + 8)?);
        if name_offset < first_name_offset || name_offset >= block.len() {
            return Err(IndexError::CorruptDirectory);
        }
        let name_end = if index + 1 < count {
            usize::from(read_u16(block, entry + DIRENT_SIZE + 8)?)
        } else {
            let tail = block
                .get(name_offset..)
                .ok_or(IndexError::CorruptDirectory)?;
            name_offset
                .checked_add(
                    tail.iter().position(|byte| *byte == 0).unwrap_or(tail.len()),
                )
                .ok_or(IndexError::ArithmeticOverflow)?
        };
        if name_end < name_offset || name_end > block.len() {
            return Err(IndexError::CorruptDirectory);
        }
        if block
            .get(name_offset..name_end)
            .ok_or(IndexError::CorruptDirectory)?
            == target
        {
            return Ok(Some(read_u64(block, entry)?));
        }
    }
    Ok(None)
}

fn parse_absolute_path(path: &str) -> Result<Vec<&str>, IndexError> {
    if !path.starts_with('/') {
        return Err(IndexError::InvalidPath("path must be absolute"));
    }
    if path == "/" {
        return Ok(Vec::new());
    }
    let mut components = Vec::new();
    for component in path[1..].split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return Err(IndexError::InvalidPath(
                "empty, dot, and parent components are forbidden",
            ));
        }
        components.push(component);
    }
    Ok(components)
}

fn align8(value: u64) -> Result<u64, IndexError> {
    value
        .checked_add(7)
        .map(|rounded| rounded & !7_u64)
        .ok_or(IndexError::ArithmeticOverflow)
}

fn div_ceil(value: u64, divisor: u64) -> Result<u64, IndexError> {
    value
        .checked_add(divisor.saturating_sub(1))
        .map(|rounded| rounded / divisor)
        .ok_or(IndexError::ArithmeticOverflow)
}

fn ensure_range(bytes: u64, offset: u64, length: u64) -> Result<(), IndexError> {
    let end = offset
        .checked_add(length)
        .ok_or(IndexError::ArithmeticOverflow)?;
    if end > bytes {
        return Err(IndexError::UnexpectedEndOfImage);
    }
    Ok(())
}

fn read_exact_at(file: &mut File, offset: u64, buffer: &mut [u8]) -> Result<(), IndexError> {
    file.seek(SeekFrom::Start(offset)).map_err(IndexError::Io)?;
    file.read_exact(buffer).map_err(IndexError::Io)
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, IndexError> {
    let end = offset.checked_add(2).ok_or(IndexError::ArithmeticOverflow)?;
    let raw: [u8; 2] = bytes
        .get(offset..end)
        .ok_or(IndexError::UnexpectedEndOfStructure)?
        .try_into()
        .map_err(|_| IndexError::UnexpectedEndOfStructure)?;
    Ok(u16::from_le_bytes(raw))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, IndexError> {
    let end = offset.checked_add(4).ok_or(IndexError::ArithmeticOverflow)?;
    let raw: [u8; 4] = bytes
        .get(offset..end)
        .ok_or(IndexError::UnexpectedEndOfStructure)?
        .try_into()
        .map_err(|_| IndexError::UnexpectedEndOfStructure)?;
    Ok(u32::from_le_bytes(raw))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, IndexError> {
    let end = offset.checked_add(8).ok_or(IndexError::ArithmeticOverflow)?;
    let raw: [u8; 8] = bytes
        .get(offset..end)
        .ok_or(IndexError::UnexpectedEndOfStructure)?
        .try_into()
        .map_err(|_| IndexError::UnexpectedEndOfStructure)?;
    Ok(u64::from_le_bytes(raw))
}

#[derive(Debug)]
pub enum IndexError {
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

impl fmt::Display for IndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "EROFS compact-index I/O error: {error}"),
            Self::View(error) => write!(f, "Loom effective-view error: {error}"),
            Self::BadMagic(magic) => write!(f, "invalid EROFS magic {magic:#010x}"),
            Self::InvalidFilesystem(reason) => write!(f, "invalid compact EROFS: {reason}"),
            Self::UnsupportedFilesystem(reason) => {
                write!(f, "unsupported compact EROFS: {reason}")
            }
            Self::UnsupportedInode(reason) => write!(f, "unsupported compact inode: {reason}"),
            Self::IncompatibleReplacement(reason) => {
                write!(f, "incompatible compact replacement: {reason}")
            }
            Self::ReplacementSizeMismatch { expected, actual } => write!(
                f,
                "replacement size mismatch: expected {expected} bytes, got {actual}"
            ),
            Self::CompressionDoesNotFit { encoded, capacity } => write!(
                f,
                "raw LZ4 block does not fit existing compact pcluster: encoded {encoded} bytes, capacity {capacity}"
            ),
            Self::CompressionValidationFailed => {
                write!(f, "compact raw LZ4 round-trip validation failed")
            }
            Self::InvalidPath(reason) => write!(f, "invalid EROFS path: {reason}"),
            Self::PathNotFound(name) => write!(f, "EROFS path component not found: {name:?}"),
            Self::NotDirectory(nid) => write!(f, "EROFS nid {nid} is not a directory"),
            Self::NotRegularFile(nid) => write!(f, "EROFS nid {nid} is not a regular file"),
            Self::CorruptDirectory => write!(f, "malformed EROFS directory block"),
            Self::UnexpectedEndOfImage => write!(f, "EROFS reference lies beyond image bytes"),
            Self::UnexpectedEndOfStructure => write!(f, "unexpected end of EROFS structure"),
            Self::ArithmeticOverflow => write!(f, "integer overflow while parsing compact EROFS"),
        }
    }
}

impl std::error::Error for IndexError {
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
    fn two_lclusters_stay_in_4b_compact_tail() {
        for modulo in [0_u64, 8, 16, 24] {
            let regions = compact_regions(64 + modulo, 2).unwrap();
            assert_eq!(regions.compact_2b, 0);
        }
    }

    #[test]
    fn twenty_four_lclusters_force_one_2b_pack_for_every_alignment() {
        for modulo in [0_u64, 8, 16, 24] {
            let regions = compact_regions(64 + modulo, 24).unwrap();
            assert_eq!(regions.compact_2b, 16);
        }
    }

    #[test]
    fn raw_lz4_right_aligned_0padding_round_trips() {
        let mut input = vec![b'Q'; 8192];
        input[64..87].copy_from_slice(b"LOOM-STAGE13-0PADDING!!");
        let encoded = encode_lz4_block(&input).unwrap();
        assert!(!encoded.is_empty());
        assert_ne!(encoded[0], 0);
        assert!(encoded.len() < BLOCK_BYTES);
        let mut pcluster = vec![0_u8; BLOCK_BYTES];
        let start = pcluster.len() - encoded.len();
        pcluster[start..].copy_from_slice(&encoded);
        assert!(pcluster[..start].iter().all(|byte| *byte == 0));
        assert_eq!(decode_0padding_pcluster(&pcluster, input.len()).unwrap(), input);
    }

    #[test]
    fn incompressible_payload_exceeds_compact_pcluster() {
        let mut state = 0x4c4f_4f4d_u32;
        let mut input = vec![0_u8; 8192];
        for byte in &mut input {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            *byte = state as u8;
        }
        let encoded = encode_lz4_block(&input).unwrap();
        assert!(encoded.len() > BLOCK_BYTES);
        assert_eq!(decode_lz4_block(&encoded, input.len()).unwrap(), input);
    }
}

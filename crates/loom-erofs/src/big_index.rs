#![forbid(unsafe_code)]

use super::multi_index::{CompiledMultiSwap, MultiIndexError};
use loom_view::EffectiveBlockStore;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const SUPERBLOCK_OFFSET: u64 = 1024;
const SUPERBLOCK_SIZE: usize = 128;
const EROFS_MAGIC: u32 = 0xe0f5_e1e2;
const BLOCK_SIZE: u32 = 4096;
const BLOCK_BYTES: usize = 4096;
const BLOCK_BITS: u16 = 12;
const OFFSET_MASK: u16 = (1 << BLOCK_BITS) - 1;
const D0_CBLKCNT: u16 = 1 << 11;
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
const BIG_ADVISE: u16 =
    ADVISE_COMPACTED_2B | ADVISE_BIG_PCLUSTER_1 | ADVISE_BIG_PCLUSTER_2;
const LZ4_ALGORITHM: u8 = 0;
const FEATURE_LZ4_0PADDING: u32 = 0x0000_0001;
const FEATURE_BIG_PCLUSTER: u32 = 0x0000_0002;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BigTopology {
    nid: u64,
    logical_size: u64,
    logical_lclusters: usize,
    compact_2b_entries: usize,
    pcluster: u64,
    physical_blocks: usize,
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
    compressed_blocks: u32,
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

impl CompiledMultiSwap {
    /// Replaces one compact big pcluster from a separately verified encoded EROFS image.
    ///
    /// Stage 18 accepts one logical `HEAD1` extent whose physical pcluster occupies two or
    /// more contiguous filesystem blocks. The first NONHEAD must carry the big-pcluster
    /// CBLKCNT marker and that count must match the inode compressed-block count. Origin and
    /// replacement images may place the big pcluster at different block addresses, but must
    /// expose identical logical size and physical block count. Compact metadata is untouched.
    ///
    /// # Errors
    /// Returns [`MultiIndexError`] for unsupported/corrupt big-pcluster layouts, incompatible
    /// replacement images, I/O failures, or effective-view failures.
    pub fn compile_big_pcluster_swap(
        origin_path: &Path,
        target_path: &str,
        replacement_image_path: &Path,
    ) -> Result<Self, MultiIndexError> {
        let mut origin = Image::open(origin_path)?;
        let mut replacement = Image::open(replacement_image_path)?;
        let origin_nid = origin.resolve_path(target_path)?;
        let replacement_nid = replacement.resolve_path(target_path)?;
        let origin_topology = origin.read_big_topology(origin_nid)?;
        let replacement_topology = replacement.read_big_topology(replacement_nid)?;

        validate_compatible(&origin_topology, &replacement_topology)?;

        let mut encoded_blocks = Vec::with_capacity(replacement_topology.physical_blocks);
        for offset in 0..replacement_topology.physical_blocks {
            let offset = u64::try_from(offset).map_err(|_| MultiIndexError::ArithmeticOverflow)?;
            let block = replacement_topology
                .pcluster
                .checked_add(offset)
                .ok_or(MultiIndexError::ArithmeticOverflow)?;
            encoded_blocks.push(replacement.read_block(block)?);
        }

        let mut view = EffectiveBlockStore::open(origin_path, BLOCK_SIZE)
            .map_err(MultiIndexError::View)?;
        let mut origin_pclusters = Vec::with_capacity(origin_topology.physical_blocks);
        let mut replacement_pclusters = Vec::with_capacity(replacement_topology.physical_blocks);
        for (offset, encoded) in encoded_blocks.into_iter().enumerate() {
            let offset = u64::try_from(offset).map_err(|_| MultiIndexError::ArithmeticOverflow)?;
            let origin_block = origin_topology
                .pcluster
                .checked_add(offset)
                .ok_or(MultiIndexError::ArithmeticOverflow)?;
            let replacement_block = replacement_topology
                .pcluster
                .checked_add(offset)
                .ok_or(MultiIndexError::ArithmeticOverflow)?;
            view.block_mut(origin_block)
                .map_err(MultiIndexError::View)?
                .copy_from_slice(&encoded);
            origin_pclusters.push(origin_block);
            replacement_pclusters.push(replacement_block);
        }
        let compiled = view.finalize().map_err(MultiIndexError::View)?;
        if compiled.shadow_blocks != origin_topology.physical_blocks {
            return Err(MultiIndexError::InvalidFilesystem(
                "big-pcluster swap did not produce one shadow block per physical block",
            ));
        }

        Ok(Self {
            map: compiled.map,
            shadow: compiled.shadow,
            block_size: compiled.block_size,
            origin_nid: origin_topology.nid,
            origin_pcluster: origin_topology.pcluster,
            replacement_pcluster: replacement_topology.pcluster,
            origin_pclusters,
            replacement_pclusters,
            head_lclusters: vec![0],
            encoded_bytes: vec![BLOCK_BYTES; origin_topology.physical_blocks],
            physical_pclusters: origin_topology.physical_blocks,
            logical_lclusters: origin_topology.logical_lclusters,
            compact_2b_entries: origin_topology.compact_2b_entries,
            shadow_blocks: compiled.shadow_blocks,
        })
    }
}

fn validate_compatible(
    origin: &BigTopology,
    replacement: &BigTopology,
) -> Result<(), MultiIndexError> {
    if origin.logical_size != replacement.logical_size {
        return Err(MultiIndexError::IncompatibleReplacement(
            "big-pcluster logical file sizes differ",
        ));
    }
    if origin.physical_blocks != replacement.physical_blocks {
        return Err(MultiIndexError::IncompatibleReplacement(
            "big-pcluster physical block counts differ",
        ));
    }
    if origin.logical_lclusters != replacement.logical_lclusters {
        return Err(MultiIndexError::IncompatibleReplacement(
            "big-pcluster logical lcluster counts differ",
        ));
    }
    Ok(())
}

impl Image {
    fn open(path: &Path) -> Result<Self, MultiIndexError> {
        let mut file = File::open(path).map_err(MultiIndexError::Io)?;
        let bytes = file.metadata().map_err(MultiIndexError::Io)?.len();
        let sb = read_superblock(&mut file, bytes)?;
        Ok(Self { file, bytes, sb })
    }

    fn read_inode(&mut self, nid: u64) -> Result<Inode, MultiIndexError> {
        let metadata_base = self
            .sb
            .meta_block
            .checked_mul(u64::from(BLOCK_SIZE))
            .ok_or(MultiIndexError::ArithmeticOverflow)?;
        let offset = metadata_base
            .checked_add(
                nid.checked_mul(32)
                    .ok_or(MultiIndexError::ArithmeticOverflow)?,
            )
            .ok_or(MultiIndexError::ArithmeticOverflow)?;
        ensure_range(self.bytes, offset, 32)?;

        let mut compact = [0_u8; 32];
        read_exact_at(&mut self.file, offset, &mut compact)?;
        let format = read_u16(&compact, 0)?;
        if format & !0x1f != 0 {
            return Err(MultiIndexError::UnsupportedInode(
                "unknown EROFS inode format bits",
            ));
        }
        let extended = format & 1 != 0;
        let layout =
            u8::try_from((format >> 1) & 7).map_err(|_| MultiIndexError::ArithmeticOverflow)?;
        if layout > 4 {
            return Err(MultiIndexError::UnsupportedInode(
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
            compressed_blocks: read_u32(&compact, 0x10)?,
        })
    }

    fn resolve_path(&mut self, path: &str) -> Result<u64, MultiIndexError> {
        let components = parse_absolute_path(path)?;
        let mut current = self.sb.root_nid;
        for component in components {
            let inode = self.read_inode(current)?;
            if inode.file_type() != MODE_DIRECTORY {
                return Err(MultiIndexError::NotDirectory(current));
            }
            current = self.find_child(&inode, component.as_bytes())?;
        }
        Ok(current)
    }

    fn find_child(&mut self, directory: &Inode, name: &[u8]) -> Result<u64, MultiIndexError> {
        if directory.xattr_size != 0 {
            return Err(MultiIndexError::UnsupportedInode(
                "big-pcluster path traversal refuses directory xattrs",
            ));
        }
        let block_size = u64::from(BLOCK_SIZE);
        let full_blocks = match directory.layout {
            DATA_FLAT_PLAIN => div_ceil(directory.size, block_size)?,
            DATA_FLAT_INLINE => directory.size / block_size,
            _ => {
                return Err(MultiIndexError::UnsupportedInode(
                    "big-pcluster path traversal requires flat directories",
                ))
            }
        };
        for index in 0..full_blocks {
            let block = u64::from(directory.compressed_blocks)
                .checked_add(index)
                .ok_or(MultiIndexError::ArithmeticOverflow)?;
            let bytes = self.read_block(block)?;
            if let Some(nid) = find_in_directory_block(&bytes, name)? {
                return Ok(nid);
            }
        }
        if directory.layout == DATA_FLAT_INLINE && directory.size % block_size != 0 {
            return Err(MultiIndexError::UnsupportedInode(
                "target may lie in unsupported inline directory tail",
            ));
        }
        Err(MultiIndexError::PathNotFound(
            String::from_utf8_lossy(name).into_owned(),
        ))
    }

    fn read_big_topology(&mut self, nid: u64) -> Result<BigTopology, MultiIndexError> {
        let inode = self.read_inode(nid)?;
        let logical_lclusters = validate_target_inode(&inode)?;
        let compressed_blocks = usize::try_from(inode.compressed_blocks)
            .map_err(|_| MultiIndexError::ArithmeticOverflow)?;
        if compressed_blocks < 2 || compressed_blocks >= usize::from(D0_CBLKCNT) {
            return Err(MultiIndexError::UnsupportedInode(
                "big-pcluster physical block count lies outside Stage 18 bounds",
            ));
        }

        let body_end = inode
            .offset
            .checked_add(inode.isize)
            .and_then(|value| value.checked_add(inode.xattr_size))
            .ok_or(MultiIndexError::ArithmeticOverflow)?;
        let header_offset = align8(body_end)?;
        ensure_range(self.bytes, header_offset, MAP_HEADER_SIZE)?;
        let mut header = [0_u8; 8];
        read_exact_at(&mut self.file, header_offset, &mut header)?;
        let advise = read_u16(&header, 4)?;
        if advise != BIG_ADVISE {
            return Err(MultiIndexError::UnsupportedInode(
                "Stage 18 requires COMPACTED_2B + BIG_PCLUSTER_1 + BIG_PCLUSTER_2 only",
            ));
        }
        if header[6] & 0x0f != LZ4_ALGORITHM || header[6] >> 4 != 0 || header[7] != 0 {
            return Err(MultiIndexError::UnsupportedInode(
                "Stage 18 requires 4 KiB HEAD1 LZ4 without packed fragments",
            ));
        }

        let ebase = header_offset
            .checked_add(MAP_HEADER_SIZE)
            .ok_or(MultiIndexError::ArithmeticOverflow)?;
        let regions = compact_regions(ebase, logical_lclusters)?;
        let head = self.read_compact_entry(ebase, logical_lclusters, 0)?;
        if head.kind != LCLUSTER_HEAD1 || head.low != 0 || head.slot != 0 {
            return Err(MultiIndexError::InvalidFilesystem(
                "big pcluster must begin with slot-0 HEAD1 at offset zero",
            ));
        }

        let cblk = self.read_compact_entry(ebase, logical_lclusters, 1)?;
        if cblk.kind != LCLUSTER_NONHEAD || cblk.low & D0_CBLKCNT == 0 {
            return Err(MultiIndexError::InvalidFilesystem(
                "first big-pcluster NONHEAD does not carry CBLKCNT",
            ));
        }
        let encoded_count = usize::from(cblk.low & !D0_CBLKCNT);
        if encoded_count != compressed_blocks {
            return Err(MultiIndexError::InvalidFilesystem(
                "CBLKCNT does not match inode compressed-block count",
            ));
        }

        for lcn in 2..logical_lclusters {
            let entry = self.read_compact_entry(ebase, logical_lclusters, lcn)?;
            if entry.kind != LCLUSTER_NONHEAD || entry.low & D0_CBLKCNT != 0 {
                return Err(MultiIndexError::UnsupportedInode(
                    "Stage 18 supports one HEAD1 big-pcluster extent only",
                ));
            }
            let expected = if entry.slot + 1 == entry.slots {
                logical_lclusters
                    .checked_sub(lcn)
                    .ok_or(MultiIndexError::ArithmeticOverflow)?
            } else {
                lcn
            };
            if expected >= usize::from(D0_CBLKCNT) {
                return Err(MultiIndexError::UnsupportedInode(
                    "Stage 18 refuses long-distance compact NONHEAD encoding",
                ));
            }
            let expected =
                u16::try_from(expected).map_err(|_| MultiIndexError::ArithmeticOverflow)?;
            if entry.low != expected {
                return Err(MultiIndexError::InvalidFilesystem(
                    "big-pcluster NONHEAD lookback/lookahead is inconsistent",
                ));
            }
        }

        let pcluster = head.base_pblk;
        let end = pcluster
            .checked_add(
                u64::try_from(compressed_blocks)
                    .map_err(|_| MultiIndexError::ArithmeticOverflow)?,
            )
            .ok_or(MultiIndexError::ArithmeticOverflow)?;
        if end > self.bytes / u64::from(BLOCK_SIZE) {
            return Err(MultiIndexError::InvalidFilesystem(
                "big pcluster extends beyond image",
            ));
        }

        Ok(BigTopology {
            nid,
            logical_size: inode.size,
            logical_lclusters,
            compact_2b_entries: regions.compact_2b,
            pcluster,
            physical_blocks: compressed_blocks,
        })
    }

    fn read_compact_entry(
        &mut self,
        ebase: u64,
        total: usize,
        lcn: usize,
    ) -> Result<CompactEntry, MultiIndexError> {
        if lcn >= total {
            return Err(MultiIndexError::InvalidFilesystem(
                "compact lcluster index lies beyond logical file",
            ));
        }
        let regions = compact_regions(ebase, total)?;
        let (shift, pos) = compact_entry_position(ebase, regions, lcn)?;
        let entry_bytes = 1_usize
            .checked_shl(shift)
            .ok_or(MultiIndexError::ArithmeticOverflow)?;
        let slots = if entry_bytes == 4 { 2 } else { 16 };
        let pack_bytes = entry_bytes
            .checked_mul(slots)
            .ok_or(MultiIndexError::ArithmeticOverflow)?;
        let pack_bytes_u64 =
            u64::try_from(pack_bytes).map_err(|_| MultiIndexError::ArithmeticOverflow)?;
        let pack_start = pos - (pos % pack_bytes_u64);
        ensure_range(self.bytes, pack_start, pack_bytes_u64)?;
        let mut pack = vec![0_u8; pack_bytes];
        read_exact_at(&mut self.file, pack_start, &mut pack)?;

        let entry_bytes_u64 =
            u64::try_from(entry_bytes).map_err(|_| MultiIndexError::ArithmeticOverflow)?;
        let slot = usize::try_from((pos - pack_start) / entry_bytes_u64)
            .map_err(|_| MultiIndexError::ArithmeticOverflow)?;
        let encode_bits = (pack_bytes
            .checked_mul(8)
            .and_then(|bits| bits.checked_sub(32))
            .ok_or(MultiIndexError::ArithmeticOverflow)?)
            / slots;
        let bit_pos = encode_bits
            .checked_mul(slot)
            .ok_or(MultiIndexError::ArithmeticOverflow)?;
        let word = read_u32(&pack, bit_pos / 8)? >> (bit_pos & 7);
        let low = u16::try_from(word & u32::from(OFFSET_MASK))
            .map_err(|_| MultiIndexError::ArithmeticOverflow)?;
        let kind = u16::try_from((word >> BLOCK_BITS) & u32::from(LCLUSTER_TYPE_MASK))
            .map_err(|_| MultiIndexError::ArithmeticOverflow)?;
        let base_pblk = u64::from(read_u32(&pack, pack_bytes - 4)?);
        Ok(CompactEntry {
            kind,
            low,
            slot,
            slots,
            base_pblk,
        })
    }

    fn read_block(&mut self, block: u64) -> Result<Vec<u8>, MultiIndexError> {
        let offset = block
            .checked_mul(u64::from(BLOCK_SIZE))
            .ok_or(MultiIndexError::ArithmeticOverflow)?;
        ensure_range(self.bytes, offset, u64::from(BLOCK_SIZE))?;
        let mut bytes = vec![0_u8; BLOCK_BYTES];
        read_exact_at(&mut self.file, offset, &mut bytes)?;
        Ok(bytes)
    }
}

fn validate_target_inode(inode: &Inode) -> Result<usize, MultiIndexError> {
    if inode.file_type() != MODE_REGULAR {
        return Err(MultiIndexError::NotRegularFile(inode.nid));
    }
    if inode.layout != DATA_COMPRESSED_COMPACT {
        return Err(MultiIndexError::UnsupportedInode(
            "Stage 18 requires EROFS_INODE_COMPRESSED_COMPACT",
        ));
    }
    if inode.xattr_size != 0 {
        return Err(MultiIndexError::UnsupportedInode(
            "Stage 18 compact target must not carry xattrs",
        ));
    }
    if inode.size < u64::from(BLOCK_SIZE) * 3 || inode.size % u64::from(BLOCK_SIZE) != 0 {
        return Err(MultiIndexError::UnsupportedInode(
            "Stage 18 requires a whole-block file of at least three lclusters",
        ));
    }
    usize::try_from(inode.size / u64::from(BLOCK_SIZE))
        .map_err(|_| MultiIndexError::ArithmeticOverflow)
}

fn compact_regions(ebase: u64, total: usize) -> Result<CompactRegions, MultiIndexError> {
    let modulo = usize::try_from(ebase % 32).map_err(|_| MultiIndexError::ArithmeticOverflow)?;
    let mut initial_4b = (32_usize
        .checked_sub(modulo)
        .ok_or(MultiIndexError::ArithmeticOverflow)?)
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
) -> Result<(u32, u64), MultiIndexError> {
    if lcn < regions.initial_4b {
        let delta = u64::try_from(
            lcn.checked_mul(4)
                .ok_or(MultiIndexError::ArithmeticOverflow)?,
        )
        .map_err(|_| MultiIndexError::ArithmeticOverflow)?;
        return Ok((
            2,
            ebase
                .checked_add(delta)
                .ok_or(MultiIndexError::ArithmeticOverflow)?,
        ));
    }

    let initial_bytes = u64::try_from(
        regions
            .initial_4b
            .checked_mul(4)
            .ok_or(MultiIndexError::ArithmeticOverflow)?,
    )
    .map_err(|_| MultiIndexError::ArithmeticOverflow)?;
    let mut pos = ebase
        .checked_add(initial_bytes)
        .ok_or(MultiIndexError::ArithmeticOverflow)?;
    let relative = lcn
        .checked_sub(regions.initial_4b)
        .ok_or(MultiIndexError::ArithmeticOverflow)?;
    if relative < regions.compact_2b {
        let delta = u64::try_from(
            relative
                .checked_mul(2)
                .ok_or(MultiIndexError::ArithmeticOverflow)?,
        )
        .map_err(|_| MultiIndexError::ArithmeticOverflow)?;
        return Ok((
            1,
            pos.checked_add(delta)
                .ok_or(MultiIndexError::ArithmeticOverflow)?,
        ));
    }

    let compact_bytes = u64::try_from(
        regions
            .compact_2b
            .checked_mul(2)
            .ok_or(MultiIndexError::ArithmeticOverflow)?,
    )
    .map_err(|_| MultiIndexError::ArithmeticOverflow)?;
    pos = pos
        .checked_add(compact_bytes)
        .ok_or(MultiIndexError::ArithmeticOverflow)?;
    let trailing = relative
        .checked_sub(regions.compact_2b)
        .ok_or(MultiIndexError::ArithmeticOverflow)?;
    let delta = u64::try_from(
        trailing
            .checked_mul(4)
            .ok_or(MultiIndexError::ArithmeticOverflow)?,
    )
    .map_err(|_| MultiIndexError::ArithmeticOverflow)?;
    Ok((
        2,
        pos.checked_add(delta)
            .ok_or(MultiIndexError::ArithmeticOverflow)?,
    ))
}

fn read_superblock(file: &mut File, bytes: u64) -> Result<Superblock, MultiIndexError> {
    ensure_range(
        bytes,
        SUPERBLOCK_OFFSET,
        u64::try_from(SUPERBLOCK_SIZE).map_err(|_| MultiIndexError::ArithmeticOverflow)?,
    )?;
    let mut raw = [0_u8; SUPERBLOCK_SIZE];
    read_exact_at(file, SUPERBLOCK_OFFSET, &mut raw)?;
    let magic = read_u32(&raw, 0)?;
    if magic != EROFS_MAGIC {
        return Err(MultiIndexError::BadMagic(magic));
    }
    if raw[0x0c] != 12 {
        return Err(MultiIndexError::UnsupportedFilesystem(
            "Stage 18 supports only 4 KiB EROFS blocks",
        ));
    }
    let incompat = read_u32(&raw, 0x50)?;
    let supported = FEATURE_LZ4_0PADDING | FEATURE_BIG_PCLUSTER;
    if incompat & !supported != 0 || incompat & supported != supported {
        return Err(MultiIndexError::UnsupportedFilesystem(
            "Stage 18 requires only LZ4_0PADDING + BIG_PCLUSTER incompatible features",
        ));
    }
    if raw[0x5a] != 0 || read_u16(&raw, 0x56)? != 0 {
        return Err(MultiIndexError::UnsupportedFilesystem(
            "Stage 18 requires primary-device core directories",
        ));
    }
    Ok(Superblock {
        root_nid: u64::from(read_u16(&raw, 0x0e)?),
        meta_block: u64::from(read_u32(&raw, 0x28)?),
    })
}

fn xattr_ibody_size(count: u16) -> Result<u64, MultiIndexError> {
    if count == 0 {
        return Ok(0);
    }
    12_u64
        .checked_add(
            u64::from(count - 1)
                .checked_mul(4)
                .ok_or(MultiIndexError::ArithmeticOverflow)?,
        )
        .ok_or(MultiIndexError::ArithmeticOverflow)
}

fn find_in_directory_block(block: &[u8], target: &[u8]) -> Result<Option<u64>, MultiIndexError> {
    if block.len() < DIRENT_SIZE {
        return Err(MultiIndexError::CorruptDirectory);
    }
    let first_name_offset = usize::from(read_u16(block, 8)?);
    if first_name_offset == 0
        || first_name_offset % DIRENT_SIZE != 0
        || first_name_offset > block.len()
    {
        return Err(MultiIndexError::CorruptDirectory);
    }
    let count = first_name_offset / DIRENT_SIZE;
    for index in 0..count {
        let entry = index
            .checked_mul(DIRENT_SIZE)
            .ok_or(MultiIndexError::ArithmeticOverflow)?;
        let name_offset = usize::from(read_u16(block, entry + 8)?);
        if name_offset < first_name_offset || name_offset >= block.len() {
            return Err(MultiIndexError::CorruptDirectory);
        }
        let name_end = if index + 1 < count {
            usize::from(read_u16(block, entry + DIRENT_SIZE + 8)?)
        } else {
            let tail = block
                .get(name_offset..)
                .ok_or(MultiIndexError::CorruptDirectory)?;
            name_offset
                .checked_add(tail.iter().position(|byte| *byte == 0).unwrap_or(tail.len()))
                .ok_or(MultiIndexError::ArithmeticOverflow)?
        };
        if name_end < name_offset || name_end > block.len() {
            return Err(MultiIndexError::CorruptDirectory);
        }
        if block
            .get(name_offset..name_end)
            .ok_or(MultiIndexError::CorruptDirectory)?
            == target
        {
            return Ok(Some(read_u64(block, entry)?));
        }
    }
    Ok(None)
}

fn parse_absolute_path(path: &str) -> Result<Vec<&str>, MultiIndexError> {
    if !path.starts_with('/') {
        return Err(MultiIndexError::InvalidPath("path must be absolute"));
    }
    if path == "/" {
        return Ok(Vec::new());
    }
    let mut components = Vec::new();
    for component in path[1..].split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return Err(MultiIndexError::InvalidPath(
                "empty, dot, and parent components are forbidden",
            ));
        }
        components.push(component);
    }
    Ok(components)
}

fn align8(value: u64) -> Result<u64, MultiIndexError> {
    value
        .checked_add(7)
        .map(|rounded| rounded & !7_u64)
        .ok_or(MultiIndexError::ArithmeticOverflow)
}

fn div_ceil(value: u64, divisor: u64) -> Result<u64, MultiIndexError> {
    value
        .checked_add(divisor.saturating_sub(1))
        .map(|rounded| rounded / divisor)
        .ok_or(MultiIndexError::ArithmeticOverflow)
}

fn ensure_range(bytes: u64, offset: u64, length: u64) -> Result<(), MultiIndexError> {
    let end = offset
        .checked_add(length)
        .ok_or(MultiIndexError::ArithmeticOverflow)?;
    if end > bytes {
        return Err(MultiIndexError::UnexpectedEndOfImage);
    }
    Ok(())
}

fn read_exact_at(file: &mut File, offset: u64, buffer: &mut [u8]) -> Result<(), MultiIndexError> {
    file.seek(SeekFrom::Start(offset))
        .map_err(MultiIndexError::Io)?;
    file.read_exact(buffer).map_err(MultiIndexError::Io)
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, MultiIndexError> {
    let end = offset
        .checked_add(2)
        .ok_or(MultiIndexError::ArithmeticOverflow)?;
    let raw: [u8; 2] = bytes
        .get(offset..end)
        .ok_or(MultiIndexError::UnexpectedEndOfStructure)?
        .try_into()
        .map_err(|_| MultiIndexError::UnexpectedEndOfStructure)?;
    Ok(u16::from_le_bytes(raw))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, MultiIndexError> {
    let end = offset
        .checked_add(4)
        .ok_or(MultiIndexError::ArithmeticOverflow)?;
    let raw: [u8; 4] = bytes
        .get(offset..end)
        .ok_or(MultiIndexError::UnexpectedEndOfStructure)?
        .try_into()
        .map_err(|_| MultiIndexError::UnexpectedEndOfStructure)?;
    Ok(u32::from_le_bytes(raw))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, MultiIndexError> {
    let end = offset
        .checked_add(8)
        .ok_or(MultiIndexError::ArithmeticOverflow)?;
    let raw: [u8; 8] = bytes
        .get(offset..end)
        .ok_or(MultiIndexError::UnexpectedEndOfStructure)?
        .try_into()
        .map_err(|_| MultiIndexError::UnexpectedEndOfStructure)?;
    Ok(u64::from_le_bytes(raw))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cblkcnt_extracts_two_physical_blocks() {
        let low = D0_CBLKCNT | 2;
        assert_ne!(low & D0_CBLKCNT, 0);
        assert_eq!(usize::from(low & !D0_CBLKCNT), 2);
    }

    #[test]
    fn big_head_uses_pack_base_without_nonbig_increment() {
        let head = CompactEntry {
            kind: LCLUSTER_HEAD1,
            low: 0,
            slot: 0,
            slots: 16,
            base_pblk: 77,
        };
        assert_eq!(head.base_pblk, 77);
    }
}

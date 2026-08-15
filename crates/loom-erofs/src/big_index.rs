#![forbid(unsafe_code)]

use super::multi_index::{
    compact_regions, validate_target_inode, CompactEntry, CompiledMultiSwap, Image, MultiIndexError,
    Superblock, ADVISE_COMPACTED_2B, BLOCK_BYTES, BLOCK_SIZE, D0_CBLKCNT,
    FEATURE_LZ4_0PADDING, LCLUSTER_HEAD1, LCLUSTER_NONHEAD, LZ4_ALGORITHM, MAP_HEADER_SIZE,
};
use loom_view::EffectiveBlockStore;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const FEATURE_BIG_PCLUSTER: u32 = 0x0000_0002;
const ADVISE_BIG_PCLUSTER_1: u16 = 0x0002;
const ADVISE_BIG_PCLUSTER_2: u16 = 0x0004;
const BIG_ADVISE: u16 = ADVISE_COMPACTED_2B | ADVISE_BIG_PCLUSTER_1 | ADVISE_BIG_PCLUSTER_2;
const SUPERBLOCK_OFFSET: u64 = 1024;
const SUPERBLOCK_SIZE: usize = 128;
const EROFS_MAGIC: u32 = 0xe0f5_e1e2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BigTopology {
    nid: u64,
    logical_size: u64,
    logical_lclusters: usize,
    compact_2b_entries: usize,
    pcluster: u64,
    physical_blocks: usize,
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
        let mut origin = open_big_image(origin_path)?;
        let mut replacement = open_big_image(replacement_image_path)?;
        let origin_nid = origin.resolve_path(target_path)?;
        let replacement_nid = replacement.resolve_path(target_path)?;
        let origin_topology = read_big_topology(&mut origin, origin_nid)?;
        let replacement_topology = read_big_topology(&mut replacement, replacement_nid)?;

        if origin_topology.logical_size != replacement_topology.logical_size {
            return Err(MultiIndexError::IncompatibleReplacement(
                "big-pcluster logical file sizes differ",
            ));
        }
        if origin_topology.physical_blocks != replacement_topology.physical_blocks {
            return Err(MultiIndexError::IncompatibleReplacement(
                "big-pcluster physical block counts differ",
            ));
        }
        if origin_topology.logical_lclusters != replacement_topology.logical_lclusters {
            return Err(MultiIndexError::IncompatibleReplacement(
                "big-pcluster logical lcluster counts differ",
            ));
        }

        let mut encoded_blocks = Vec::with_capacity(replacement_topology.physical_blocks);
        for offset in 0..replacement_topology.physical_blocks {
            let offset = u64::try_from(offset).map_err(|_| MultiIndexError::ArithmeticOverflow)?;
            encoded_blocks.push(
                replacement.read_block(
                    replacement_topology
                        .pcluster
                        .checked_add(offset)
                        .ok_or(MultiIndexError::ArithmeticOverflow)?,
                )?,
            );
        }

        let mut view = EffectiveBlockStore::open(origin_path, BLOCK_SIZE)
            .map_err(MultiIndexError::View)?;
        let mut origin_pclusters = Vec::with_capacity(origin_topology.physical_blocks);
        let mut replacement_pclusters = Vec::with_capacity(origin_topology.physical_blocks);
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

fn open_big_image(path: &Path) -> Result<Image, MultiIndexError> {
    let mut file = File::open(path).map_err(MultiIndexError::Io)?;
    let bytes = file.metadata().map_err(MultiIndexError::Io)?.len();
    let sb = read_big_superblock(&mut file, bytes)?;
    Ok(Image { file, bytes, sb })
}

fn read_big_topology(image: &mut Image, nid: u64) -> Result<BigTopology, MultiIndexError> {
    let inode = image.read_inode(nid)?;
    let logical_lclusters = validate_target_inode(&inode)?;
    if logical_lclusters < 3 {
        return Err(MultiIndexError::UnsupportedInode(
            "big-pcluster proof requires HEAD plus CBLKCNT NONHEAD data",
        ));
    }
    let compressed_blocks = usize::try_from(inode.data_word)
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
    ensure_range(image.bytes, header_offset, MAP_HEADER_SIZE)?;
    let mut header = [0_u8; 8];
    read_exact_at(&mut image.file, header_offset, &mut header)?;
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
    let head = image.read_compact_entry(ebase, logical_lclusters, 0)?;
    if head.kind != LCLUSTER_HEAD1 || head.low != 0 || head.slot != 0 {
        return Err(MultiIndexError::InvalidFilesystem(
            "big pcluster must begin with slot-0 HEAD1 at offset zero",
        ));
    }

    let cblk = image.read_compact_entry(ebase, logical_lclusters, 1)?;
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
        let entry = image.read_compact_entry(ebase, logical_lclusters, lcn)?;
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
        if entry.low
            != u16::try_from(expected).map_err(|_| MultiIndexError::ArithmeticOverflow)?
        {
            return Err(MultiIndexError::InvalidFilesystem(
                "big-pcluster NONHEAD lookback/lookahead is inconsistent",
            ));
        }
    }

    let pcluster = head.base_pblk;
    let end = pcluster
        .checked_add(
            u64::try_from(compressed_blocks).map_err(|_| MultiIndexError::ArithmeticOverflow)?,
        )
        .ok_or(MultiIndexError::ArithmeticOverflow)?;
    if end > image.bytes / u64::from(BLOCK_SIZE) {
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

fn read_big_superblock(file: &mut File, bytes: u64) -> Result<Superblock, MultiIndexError> {
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

fn align8(value: u64) -> Result<u64, MultiIndexError> {
    value
        .checked_add(7)
        .map(|rounded| rounded & !7_u64)
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

#![forbid(unsafe_code)]

use loom_map::LoomMap;
use loom_view::{EffectiveBlockStore, ViewError};
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

const SUPERBLOCK_OFFSET: u64 = 1024;
const SUPERBLOCK_SIZE: usize = 128;
const EROFS_MAGIC: u32 = 0xe0f5_e1e2;
const BLOCK_SIZE: u32 = 4096;
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
const COMPACT_PACK_SIZE: usize = 8;
const LCLUSTER_HEAD1: u16 = 1;
const LCLUSTER_NONHEAD: u16 = 2;
const LCLUSTER_TYPE_MASK: u16 = 3;
const ADVISE_COMPACTED_2B: u16 = 0x0001;
const LZ4_ALGORITHM: u8 = 0;
const FEATURE_LZ4_0PADDING: u32 = 0x0000_0001;

#[derive(Debug)]
pub struct CompiledSwap {
    pub map: LoomMap,
    pub shadow: Vec<u8>,
    pub block_size: u32,
    pub origin_nid: u64,
    pub origin_pcluster: u64,
    pub replacement_pcluster: u64,
    pub shadow_blocks: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Extent {
    nid: u64,
    logical_size: u64,
    pcluster: u64,
    algorithm: u8,
    advise: u16,
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

struct Image {
    file: File,
    bytes: u64,
    sb: Superblock,
}

/// Swaps one complete encoded pcluster for the minimal two-lcluster EROFS compact layout.
///
/// Both images must use 4 KiB blocks, `COMPRESSED_COMPACT`, one encoded physical block,
/// one two-entry 4-byte compact pack representing `HEAD1 + NONHEAD`, LZ4, zero HEAD
/// cluster offset, and no compact features other than the standard `COMPACTED_2B` advise.
/// The compact metadata itself is never modified.
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
    let origin_extent = origin.read_minimal_extent(origin_nid)?;
    let replacement_extent = replacement.read_minimal_extent(replacement_nid)?;

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

    let encoded = replacement.read_block(replacement_extent.pcluster)?;
    let mut view = EffectiveBlockStore::open(origin_path, BLOCK_SIZE).map_err(IndexError::View)?;
    view.block_mut(origin_extent.pcluster)
        .map_err(IndexError::View)?
        .copy_from_slice(&encoded);
    let compiled = view.finalize().map_err(IndexError::View)?;
    if compiled.shadow_blocks != 1 {
        return Err(IndexError::IncompatibleReplacement(
            "compact one-pcluster swap did not produce exactly one shadow block",
        ));
    }

    Ok(CompiledSwap {
        map: compiled.map,
        shadow: compiled.shadow,
        block_size: compiled.block_size,
        origin_nid,
        origin_pcluster: origin_extent.pcluster,
        replacement_pcluster: replacement_extent.pcluster,
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
                "Stage 12 path traversal refuses directory xattrs",
            ));
        }
        let block_size = u64::from(BLOCK_SIZE);
        let full_blocks = match directory.layout {
            DATA_FLAT_PLAIN => div_ceil(directory.size, block_size)?,
            DATA_FLAT_INLINE => directory.size / block_size,
            _ => {
                return Err(IndexError::UnsupportedInode(
                    "Stage 12 path traversal requires flat directories",
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

    fn read_minimal_extent(&mut self, nid: u64) -> Result<Extent, IndexError> {
        let inode = self.read_inode(nid)?;
        self.validate_target_inode(&inode)?;
        let header_offset = align8(
            inode
                .offset
                .checked_add(inode.isize)
                .and_then(|value| value.checked_add(inode.xattr_size))
                .ok_or(IndexError::ArithmeticOverflow)?,
        )?;
        ensure_range(self.bytes, header_offset, MAP_HEADER_SIZE + 8)?;

        let mut header = [0_u8; 8];
        read_exact_at(&mut self.file, header_offset, &mut header)?;
        let advise = read_u16(&header, 4)?;
        if advise != ADVISE_COMPACTED_2B {
            return Err(IndexError::UnsupportedInode(
                "Stage 12 requires only COMPACTED_2B compact advice",
            ));
        }
        let algorithm = header[6] & 0x0f;
        if algorithm != LZ4_ALGORITHM || header[6] >> 4 != 0 {
            return Err(IndexError::UnsupportedInode(
                "Stage 12 supports only HEAD1 LZ4",
            ));
        }
        if header[7] != 0 {
            return Err(IndexError::UnsupportedInode(
                "Stage 12 requires 4 KiB logical clusters with no packed-fragment bit",
            ));
        }

        let pcluster = self.read_compact_pack(
            header_offset
                .checked_add(MAP_HEADER_SIZE)
                .ok_or(IndexError::ArithmeticOverflow)?,
        )?;
        Ok(Extent {
            nid,
            logical_size: inode.size,
            pcluster,
            algorithm,
            advise,
        })
    }

    fn validate_target_inode(&self, inode: &Inode) -> Result<(), IndexError> {
        if inode.file_type() != MODE_REGULAR {
            return Err(IndexError::NotRegularFile(inode.nid));
        }
        if inode.layout != DATA_COMPRESSED_COMPACT {
            return Err(IndexError::UnsupportedInode(
                "Stage 12 requires EROFS_INODE_COMPRESSED_COMPACT",
            ));
        }
        if inode.xattr_size != 0 {
            return Err(IndexError::UnsupportedInode(
                "Stage 12 compact target must not carry xattrs",
            ));
        }
        if inode.size != u64::from(BLOCK_SIZE) * 2 {
            return Err(IndexError::UnsupportedInode(
                "Stage 12 requires exactly two logical filesystem blocks",
            ));
        }
        if inode.data_word != 1 {
            return Err(IndexError::UnsupportedInode(
                "Stage 12 requires exactly one encoded physical block",
            ));
        }
        Ok(())
    }

    fn read_compact_pack(&mut self, offset: u64) -> Result<u64, IndexError> {
        ensure_range(
            self.bytes,
            offset,
            u64::try_from(COMPACT_PACK_SIZE).map_err(|_| IndexError::ArithmeticOverflow)?,
        )?;
        let mut pack = [0_u8; COMPACT_PACK_SIZE];
        read_exact_at(&mut self.file, offset, &mut pack)?;

        let head = read_u16(&pack, 0)?;
        if compact_type(head) != LCLUSTER_HEAD1 || compact_value(head) != 0 {
            return Err(IndexError::UnsupportedInode(
                "Stage 12 first compact entry must be HEAD1 with zero cluster offset",
            ));
        }
        let nonhead = read_u16(&pack, 2)?;
        if compact_type(nonhead) != LCLUSTER_NONHEAD || compact_value(nonhead) != 1 {
            return Err(IndexError::UnsupportedInode(
                "Stage 12 final compact NONHEAD must encode one-lcluster lookahead",
            ));
        }

        let base = u64::from(read_u32(&pack, 4)?);
        let pcluster = base.checked_add(1).ok_or(IndexError::ArithmeticOverflow)?;
        if pcluster >= self.bytes / u64::from(BLOCK_SIZE) {
            return Err(IndexError::InvalidFilesystem(
                "compact pcluster lies beyond image",
            ));
        }
        Ok(pcluster)
    }

    fn read_block(&mut self, block: u64) -> Result<Vec<u8>, IndexError> {
        let block_size = u64::from(BLOCK_SIZE);
        let offset = block
            .checked_mul(block_size)
            .ok_or(IndexError::ArithmeticOverflow)?;
        ensure_range(self.bytes, offset, block_size)?;
        let mut bytes = vec![
            0_u8;
            usize::try_from(block_size).map_err(|_| IndexError::ArithmeticOverflow)?
        ];
        read_exact_at(&mut self.file, offset, &mut bytes)?;
        Ok(bytes)
    }
}

fn compact_type(value: u16) -> u16 {
    (value >> BLOCK_BITS) & LCLUSTER_TYPE_MASK
}

fn compact_value(value: u16) -> u16 {
    value & OFFSET_MASK
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
            "Stage 12 supports only 4 KiB EROFS blocks",
        ));
    }
    let incompat = read_u32(&raw, 0x50)?;
    if incompat & !FEATURE_LZ4_0PADDING != 0 {
        return Err(IndexError::UnsupportedFilesystem(
            "Stage 12 image enables unsupported incompatible EROFS features",
        ));
    }
    if incompat & FEATURE_LZ4_0PADDING == 0 {
        return Err(IndexError::UnsupportedFilesystem(
            "Stage 12 expects normal compact LZ4 with 0padding enabled",
        ));
    }
    if raw[0x5a] != 0 || read_u16(&raw, 0x56)? != 0 {
        return Err(IndexError::UnsupportedFilesystem(
            "Stage 12 requires primary-device core directories",
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
                    tail.iter()
                        .position(|byte| *byte == 0)
                        .unwrap_or(tail.len()),
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
    fn minimal_pack_decodes_expected_types_and_offsets() {
        assert_eq!(compact_type(0x1000), LCLUSTER_HEAD1);
        assert_eq!(compact_value(0x1000), 0);
        assert_eq!(compact_type(0x2001), LCLUSTER_NONHEAD);
        assert_eq!(compact_value(0x2001), 1);
    }

    #[test]
    fn compact_pack_base_points_one_block_before_first_head() {
        let base = 41_u64;
        assert_eq!(base.checked_add(1), Some(42));
    }
}

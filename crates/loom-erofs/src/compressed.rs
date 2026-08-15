#![forbid(unsafe_code)]

use loom_map::LoomMap;
use loom_view::{EffectiveBlockStore, ViewError};
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

const SB_OFFSET: u64 = 1024;
const SB_SIZE: usize = 128;
const MAGIC: u32 = 0xe0f5_e1e2;
const DIRENT_SIZE: usize = 12;
const MODE_TYPE_MASK: u16 = 0o170_000;
const MODE_DIRECTORY: u16 = 0o040_000;
const MODE_REGULAR: u16 = 0o100_000;
const FLAT_PLAIN: u8 = 0;
const FLAT_INLINE: u8 = 2;
const COMPRESSED_FULL: u8 = 1;
const LCLUSTER_HEAD1: u16 = 1;
const LCLUSTER_NONHEAD: u16 = 2;
const LCLUSTER_TYPE_MASK: u16 = 3;
const MAP_HEADER_SIZE: u64 = 8;
const FULL_INDEX_GAP: u64 = 8;
const FULL_INDEX_SIZE: usize = 8;
const FULL_INDEX_COUNT: usize = 2;
const LZ4_ALGORITHM: u8 = 0;
const FEATURE_LZ4_0PADDING: u32 = 0x0000_0001;
const FEATURE_COMPR_CFGS_OR_BIG_PCLUSTER: u32 = 0x0000_0002;
const SUPPORTED_INCOMPAT: u32 = FEATURE_LZ4_0PADDING | FEATURE_COMPR_CFGS_OR_BIG_PCLUSTER;

#[derive(Debug)]
pub struct CompiledPclusterSwap {
    pub map: LoomMap,
    pub shadow: Vec<u8>,
    pub block_size: u32,
    pub origin_nid: u64,
    pub origin_pcluster: u64,
    pub replacement_pcluster: u64,
    pub shadow_blocks: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CompressedExtent {
    nid: u64,
    logical_size: u64,
    pcluster: u64,
    algorithm: u8,
    cluster_offset: u16,
    map_advise: u16,
}

#[derive(Debug, Clone, Copy)]
struct Superblock {
    block_size: u32,
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

#[derive(Debug, Clone, Copy)]
struct FullMapHeader {
    index_offset: u64,
    advise: u16,
    algorithm: u8,
}

struct Image {
    file: File,
    bytes: u64,
    sb: Superblock,
}

/// Replaces one complete one-block LZ4 pcluster while keeping stock compressed indexes.
///
/// Stage 10 uses the smallest layout that is actually compression-beneficial: two logical
/// filesystem blocks are represented by one physical pcluster. Both images must expose a
/// `COMPRESSED_FULL` target with a HEAD1 + NONHEAD full-index chain, LZ4, zero cluster
/// offset, one encoded block, and no big/inline/fragment/extent metadata features.
/// The encoded replacement pcluster is taken from a separately verified EROFS image.
///
/// # Errors
/// Returns [`CompressedError`] for incompatible compressed layouts, path failures,
/// malformed indexes, I/O failures, or effective-view errors.
pub fn compile_full_pcluster_swap(
    origin_path: &Path,
    target_path: &str,
    replacement_image_path: &Path,
) -> Result<CompiledPclusterSwap, CompressedError> {
    let mut origin = Image::open(origin_path)?;
    let mut replacement = Image::open(replacement_image_path)?;
    if origin.sb.block_size != replacement.sb.block_size {
        return Err(CompressedError::IncompatibleReplacement(
            "filesystem block sizes differ",
        ));
    }

    let origin_nid = origin.resolve_path(target_path)?;
    let replacement_nid = replacement.resolve_path(target_path)?;
    let origin_extent = origin.read_two_lcluster_full_extent(origin_nid)?;
    let replacement_extent = replacement.read_two_lcluster_full_extent(replacement_nid)?;

    if origin_extent.logical_size != replacement_extent.logical_size {
        return Err(CompressedError::IncompatibleReplacement(
            "logical file sizes differ",
        ));
    }
    if origin_extent.algorithm != replacement_extent.algorithm {
        return Err(CompressedError::IncompatibleReplacement(
            "compression algorithms differ",
        ));
    }
    if origin_extent.cluster_offset != replacement_extent.cluster_offset {
        return Err(CompressedError::IncompatibleReplacement(
            "compressed cluster offsets differ",
        ));
    }
    if origin_extent.map_advise != replacement_extent.map_advise {
        return Err(CompressedError::IncompatibleReplacement(
            "compressed map advice differs",
        ));
    }

    let encoded = replacement.read_block(replacement_extent.pcluster)?;
    let mut view = EffectiveBlockStore::open(origin_path, origin.sb.block_size)
        .map_err(CompressedError::View)?;
    view.block_mut(origin_extent.pcluster)
        .map_err(CompressedError::View)?
        .copy_from_slice(&encoded);
    let compiled = view.finalize().map_err(CompressedError::View)?;
    if compiled.shadow_blocks != 1 {
        return Err(CompressedError::IncompatibleReplacement(
            "one-pcluster proof did not finalize to exactly one shadow block",
        ));
    }

    Ok(CompiledPclusterSwap {
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
    fn open(path: &Path) -> Result<Self, CompressedError> {
        let mut file = File::open(path).map_err(CompressedError::Io)?;
        let bytes = file.metadata().map_err(CompressedError::Io)?.len();
        let sb = read_superblock(&mut file, bytes)?;
        Ok(Self { file, bytes, sb })
    }

    fn read_inode(&mut self, nid: u64) -> Result<Inode, CompressedError> {
        let metadata_base = self
            .sb
            .meta_block
            .checked_mul(u64::from(self.sb.block_size))
            .ok_or(CompressedError::ArithmeticOverflow)?;
        let offset = metadata_base
            .checked_add(
                nid.checked_mul(32)
                    .ok_or(CompressedError::ArithmeticOverflow)?,
            )
            .ok_or(CompressedError::ArithmeticOverflow)?;
        ensure_range(self.bytes, offset, 32)?;

        let mut compact = [0_u8; 32];
        read_exact_at(&mut self.file, offset, &mut compact)?;
        let format = read_u16(&compact, 0)?;
        if format & !0x1f != 0 {
            return Err(CompressedError::UnsupportedInode(
                "unknown EROFS inode format bits",
            ));
        }
        let extended = format & 1 != 0;
        let layout =
            u8::try_from((format >> 1) & 7).map_err(|_| CompressedError::ArithmeticOverflow)?;
        if layout > 4 {
            return Err(CompressedError::UnsupportedInode(
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

    fn resolve_path(&mut self, path: &str) -> Result<u64, CompressedError> {
        let components = parse_absolute_path(path)?;
        let mut current = self.sb.root_nid;
        for component in components {
            let inode = self.read_inode(current)?;
            if inode.file_type() != MODE_DIRECTORY {
                return Err(CompressedError::NotDirectory(current));
            }
            current = self.find_child(&inode, component.as_bytes())?;
        }
        Ok(current)
    }

    fn find_child(&mut self, directory: &Inode, name: &[u8]) -> Result<u64, CompressedError> {
        if directory.xattr_size != 0 {
            return Err(CompressedError::UnsupportedInode(
                "Stage 10 path traversal refuses directory xattrs",
            ));
        }
        let block_size = u64::from(self.sb.block_size);
        let full_blocks = match directory.layout {
            FLAT_PLAIN => div_ceil(directory.size, block_size)?,
            FLAT_INLINE => directory.size / block_size,
            _ => {
                return Err(CompressedError::UnsupportedInode(
                    "Stage 10 path traversal requires flat directories",
                ))
            }
        };
        for index in 0..full_blocks {
            let block = u64::from(directory.data_word)
                .checked_add(index)
                .ok_or(CompressedError::ArithmeticOverflow)?;
            let bytes = self.read_block(block)?;
            if let Some(nid) = find_in_directory_block(&bytes, name)? {
                return Ok(nid);
            }
        }
        if directory.layout == FLAT_INLINE && directory.size % block_size != 0 {
            return Err(CompressedError::UnsupportedInode(
                "target may lie in unsupported inline directory tail",
            ));
        }
        Err(CompressedError::PathNotFound(
            String::from_utf8_lossy(name).into_owned(),
        ))
    }

    fn read_two_lcluster_full_extent(
        &mut self,
        nid: u64,
    ) -> Result<CompressedExtent, CompressedError> {
        let inode = self.read_inode(nid)?;
        self.validate_stage10_inode(&inode)?;
        let header = self.read_stage10_map_header(&inode)?;
        let (cluster_offset, pcluster) = self.read_stage10_full_indexes(header.index_offset)?;

        Ok(CompressedExtent {
            nid: inode.nid,
            logical_size: inode.size,
            pcluster,
            algorithm: header.algorithm,
            cluster_offset,
            map_advise: header.advise,
        })
    }

    fn validate_stage10_inode(&self, inode: &Inode) -> Result<(), CompressedError> {
        if inode.file_type() != MODE_REGULAR {
            return Err(CompressedError::NotRegularFile(inode.nid));
        }
        if inode.layout != COMPRESSED_FULL {
            return Err(CompressedError::UnsupportedInode(
                "Stage 10 requires EROFS_INODE_COMPRESSED_FULL",
            ));
        }
        if inode.xattr_size != 0 {
            return Err(CompressedError::UnsupportedInode(
                "Stage 10 compressed target must not carry xattrs",
            ));
        }
        let logical_size = u64::from(self.sb.block_size)
            .checked_mul(2)
            .ok_or(CompressedError::ArithmeticOverflow)?;
        if inode.size != logical_size {
            return Err(CompressedError::UnsupportedInode(
                "Stage 10 requires exactly two logical filesystem blocks",
            ));
        }
        if inode.data_word != 1 {
            return Err(CompressedError::UnsupportedInode(
                "Stage 10 requires exactly one encoded physical block",
            ));
        }
        Ok(())
    }

    fn read_stage10_map_header(&mut self, inode: &Inode) -> Result<FullMapHeader, CompressedError> {
        let body_end = inode
            .offset
            .checked_add(inode.isize)
            .and_then(|value| value.checked_add(inode.xattr_size))
            .ok_or(CompressedError::ArithmeticOverflow)?;
        let header_offset = align8(body_end)?;
        ensure_range(self.bytes, header_offset, MAP_HEADER_SIZE)?;
        let mut header = [0_u8; 8];
        read_exact_at(&mut self.file, header_offset, &mut header)?;

        let advise = read_u16(&header, 4)?;
        if advise != 0 {
            return Err(CompressedError::UnsupportedInode(
                "Stage 10 refuses big/inline/fragment/extent compressed advice",
            ));
        }
        let algorithm = header[6] & 0x0f;
        if algorithm != LZ4_ALGORITHM || header[6] >> 4 != 0 {
            return Err(CompressedError::UnsupportedInode(
                "Stage 10 supports only HEAD1 LZ4",
            ));
        }
        if header[7] != 0 {
            return Err(CompressedError::UnsupportedInode(
                "Stage 10 requires lcluster size equal to filesystem block size",
            ));
        }
        let index_offset = header_offset
            .checked_add(MAP_HEADER_SIZE)
            .and_then(|value| value.checked_add(FULL_INDEX_GAP))
            .ok_or(CompressedError::ArithmeticOverflow)?;
        Ok(FullMapHeader {
            index_offset,
            advise,
            algorithm,
        })
    }

    fn read_stage10_full_indexes(
        &mut self,
        index_offset: u64,
    ) -> Result<(u16, u64), CompressedError> {
        let index_bytes = FULL_INDEX_SIZE
            .checked_mul(FULL_INDEX_COUNT)
            .ok_or(CompressedError::ArithmeticOverflow)?;
        ensure_range(
            self.bytes,
            index_offset,
            u64::try_from(index_bytes).map_err(|_| CompressedError::ArithmeticOverflow)?,
        )?;
        let mut indexes = [0_u8; FULL_INDEX_SIZE * FULL_INDEX_COUNT];
        read_exact_at(&mut self.file, index_offset, &mut indexes)?;

        let head_advise = read_u16(&indexes, 0)?;
        if head_advise & LCLUSTER_TYPE_MASK != LCLUSTER_HEAD1
            || head_advise & !LCLUSTER_TYPE_MASK != 0
        {
            return Err(CompressedError::UnsupportedInode(
                "Stage 10 requires an ordinary HEAD1 first full index",
            ));
        }
        let cluster_offset = read_u16(&indexes, 2)?;
        if cluster_offset != 0 {
            return Err(CompressedError::UnsupportedInode(
                "Stage 10 requires zero HEAD cluster offset",
            ));
        }
        let pcluster = u64::from(read_u32(&indexes, 4)?);
        if pcluster >= self.bytes / u64::from(self.sb.block_size) {
            return Err(CompressedError::InvalidFilesystem(
                "compressed pcluster lies beyond image",
            ));
        }

        let nonhead = FULL_INDEX_SIZE;
        let nonhead_advise = read_u16(&indexes, nonhead)?;
        if nonhead_advise & LCLUSTER_TYPE_MASK != LCLUSTER_NONHEAD
            || nonhead_advise & !LCLUSTER_TYPE_MASK != 0
        {
            return Err(CompressedError::UnsupportedInode(
                "Stage 10 requires an ordinary NONHEAD second full index",
            ));
        }
        if read_u16(&indexes, nonhead + 4)? != 1 {
            return Err(CompressedError::UnsupportedInode(
                "Stage 10 NONHEAD must look back exactly one lcluster to HEAD1",
            ));
        }
        Ok((cluster_offset, pcluster))
    }

    fn read_block(&mut self, block: u64) -> Result<Vec<u8>, CompressedError> {
        let block_size = u64::from(self.sb.block_size);
        let offset = block
            .checked_mul(block_size)
            .ok_or(CompressedError::ArithmeticOverflow)?;
        ensure_range(self.bytes, offset, block_size)?;
        let mut bytes = vec![
            0_u8;
            usize::try_from(block_size)
                .map_err(|_| CompressedError::ArithmeticOverflow)?
        ];
        read_exact_at(&mut self.file, offset, &mut bytes)?;
        Ok(bytes)
    }
}

fn read_superblock(file: &mut File, bytes: u64) -> Result<Superblock, CompressedError> {
    ensure_range(bytes, SB_OFFSET, SB_SIZE as u64)?;
    let mut raw = [0_u8; SB_SIZE];
    read_exact_at(file, SB_OFFSET, &mut raw)?;
    let magic = read_u32(&raw, 0)?;
    if magic != MAGIC {
        return Err(CompressedError::BadMagic(magic));
    }
    let block_bits = raw[0x0c];
    if !(9..=20).contains(&block_bits) {
        return Err(CompressedError::InvalidFilesystem(
            "invalid EROFS block-size bits",
        ));
    }
    let block_size = 1_u32
        .checked_shl(u32::from(block_bits))
        .ok_or(CompressedError::ArithmeticOverflow)?;
    let feature_incompat = read_u32(&raw, 0x50)?;
    if feature_incompat & !SUPPORTED_INCOMPAT != 0 {
        return Err(CompressedError::UnsupportedFilesystem(
            "Stage 10 compressed image enables unsupported incompatible features",
        ));
    }
    if raw[0x5a] != 0 || read_u16(&raw, 0x56)? != 0 {
        return Err(CompressedError::UnsupportedFilesystem(
            "Stage 10 requires primary-device core directories",
        ));
    }
    Ok(Superblock {
        block_size,
        root_nid: u64::from(read_u16(&raw, 0x0e)?),
        meta_block: u64::from(read_u32(&raw, 0x28)?),
    })
}

fn xattr_ibody_size(count: u16) -> Result<u64, CompressedError> {
    if count == 0 {
        return Ok(0);
    }
    12_u64
        .checked_add(
            u64::from(count - 1)
                .checked_mul(4)
                .ok_or(CompressedError::ArithmeticOverflow)?,
        )
        .ok_or(CompressedError::ArithmeticOverflow)
}

fn find_in_directory_block(block: &[u8], target: &[u8]) -> Result<Option<u64>, CompressedError> {
    if block.len() < DIRENT_SIZE {
        return Err(CompressedError::CorruptDirectory);
    }
    let first_name_offset = usize::from(read_u16(block, 8)?);
    if first_name_offset == 0
        || first_name_offset % DIRENT_SIZE != 0
        || first_name_offset > block.len()
    {
        return Err(CompressedError::CorruptDirectory);
    }
    let count = first_name_offset / DIRENT_SIZE;
    for index in 0..count {
        let entry = index
            .checked_mul(DIRENT_SIZE)
            .ok_or(CompressedError::ArithmeticOverflow)?;
        let name_offset = usize::from(read_u16(block, entry + 8)?);
        if name_offset < first_name_offset || name_offset >= block.len() {
            return Err(CompressedError::CorruptDirectory);
        }
        let name_end = if index + 1 < count {
            usize::from(read_u16(block, entry + DIRENT_SIZE + 8)?)
        } else {
            let tail = block
                .get(name_offset..)
                .ok_or(CompressedError::CorruptDirectory)?;
            name_offset
                .checked_add(
                    tail.iter()
                        .position(|byte| *byte == 0)
                        .unwrap_or(tail.len()),
                )
                .ok_or(CompressedError::ArithmeticOverflow)?
        };
        if name_end < name_offset || name_end > block.len() {
            return Err(CompressedError::CorruptDirectory);
        }
        if block
            .get(name_offset..name_end)
            .ok_or(CompressedError::CorruptDirectory)?
            == target
        {
            return Ok(Some(read_u64(block, entry)?));
        }
    }
    Ok(None)
}

fn parse_absolute_path(path: &str) -> Result<Vec<&str>, CompressedError> {
    if !path.starts_with('/') {
        return Err(CompressedError::InvalidPath("path must be absolute"));
    }
    if path == "/" {
        return Ok(Vec::new());
    }
    let mut components = Vec::new();
    for component in path[1..].split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return Err(CompressedError::InvalidPath(
                "empty, dot, and parent components are forbidden",
            ));
        }
        components.push(component);
    }
    Ok(components)
}

fn align8(value: u64) -> Result<u64, CompressedError> {
    value
        .checked_add(7)
        .map(|rounded| rounded & !7_u64)
        .ok_or(CompressedError::ArithmeticOverflow)
}

fn div_ceil(value: u64, divisor: u64) -> Result<u64, CompressedError> {
    value
        .checked_add(divisor.saturating_sub(1))
        .map(|rounded| rounded / divisor)
        .ok_or(CompressedError::ArithmeticOverflow)
}

fn ensure_range(bytes: u64, offset: u64, length: u64) -> Result<(), CompressedError> {
    let end = offset
        .checked_add(length)
        .ok_or(CompressedError::ArithmeticOverflow)?;
    if end > bytes {
        return Err(CompressedError::UnexpectedEndOfImage);
    }
    Ok(())
}

fn read_exact_at(file: &mut File, offset: u64, buffer: &mut [u8]) -> Result<(), CompressedError> {
    file.seek(SeekFrom::Start(offset))
        .map_err(CompressedError::Io)?;
    file.read_exact(buffer).map_err(CompressedError::Io)
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, CompressedError> {
    let end = offset
        .checked_add(2)
        .ok_or(CompressedError::ArithmeticOverflow)?;
    let raw: [u8; 2] = bytes
        .get(offset..end)
        .ok_or(CompressedError::UnexpectedEndOfStructure)?
        .try_into()
        .map_err(|_| CompressedError::UnexpectedEndOfStructure)?;
    Ok(u16::from_le_bytes(raw))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, CompressedError> {
    let end = offset
        .checked_add(4)
        .ok_or(CompressedError::ArithmeticOverflow)?;
    let raw: [u8; 4] = bytes
        .get(offset..end)
        .ok_or(CompressedError::UnexpectedEndOfStructure)?
        .try_into()
        .map_err(|_| CompressedError::UnexpectedEndOfStructure)?;
    Ok(u32::from_le_bytes(raw))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, CompressedError> {
    let end = offset
        .checked_add(8)
        .ok_or(CompressedError::ArithmeticOverflow)?;
    let raw: [u8; 8] = bytes
        .get(offset..end)
        .ok_or(CompressedError::UnexpectedEndOfStructure)?
        .try_into()
        .map_err(|_| CompressedError::UnexpectedEndOfStructure)?;
    Ok(u64::from_le_bytes(raw))
}

#[derive(Debug)]
pub enum CompressedError {
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

impl fmt::Display for CompressedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "EROFS compressed I/O error: {error}"),
            Self::View(error) => write!(f, "Loom effective-view error: {error}"),
            Self::BadMagic(magic) => write!(f, "invalid EROFS magic {magic:#010x}"),
            Self::InvalidFilesystem(reason) => write!(f, "invalid compressed EROFS: {reason}"),
            Self::UnsupportedFilesystem(reason) => {
                write!(f, "unsupported compressed EROFS: {reason}")
            }
            Self::UnsupportedInode(reason) => write!(f, "unsupported compressed inode: {reason}"),
            Self::IncompatibleReplacement(reason) => {
                write!(f, "incompatible encoded replacement: {reason}")
            }
            Self::InvalidPath(reason) => write!(f, "invalid EROFS path: {reason}"),
            Self::PathNotFound(name) => write!(f, "EROFS path component not found: {name:?}"),
            Self::NotDirectory(nid) => write!(f, "EROFS nid {nid} is not a directory"),
            Self::NotRegularFile(nid) => write!(f, "EROFS nid {nid} is not a regular file"),
            Self::CorruptDirectory => write!(f, "malformed EROFS directory block"),
            Self::UnexpectedEndOfImage => write!(f, "EROFS reference lies beyond image bytes"),
            Self::UnexpectedEndOfStructure => write!(f, "unexpected end of EROFS structure"),
            Self::ArithmeticOverflow => {
                write!(f, "integer overflow while parsing compressed EROFS")
            }
        }
    }
}

impl std::error::Error for CompressedError {
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
    fn xattr_ibody_formula_matches_erofs_layout() {
        assert_eq!(xattr_ibody_size(0).unwrap(), 0);
        assert_eq!(xattr_ibody_size(1).unwrap(), 12);
        assert_eq!(xattr_ibody_size(3).unwrap(), 20);
    }

    #[test]
    fn align8_is_checked_and_monotonic() {
        assert_eq!(align8(33).unwrap(), 40);
        assert_eq!(align8(40).unwrap(), 40);
    }
}

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
const DIRENT_SIZE: usize = 12;
const MODE_TYPE_MASK: u16 = 0o170_000;
const MODE_DIRECTORY: u16 = 0o040_000;
const MODE_REGULAR: u16 = 0o100_000;
const DATA_FLAT_PLAIN: u8 = 0;
const DATA_FLAT_INLINE: u8 = 2;

#[derive(Debug)]
pub struct CompiledErofsReplacement {
    pub map: LoomMap,
    pub shadow: Vec<u8>,
    pub block_size: u32,
    pub nid: u64,
    pub start_block: u64,
    pub shadow_blocks: usize,
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
    mode: u16,
    size: u64,
    start_block: u64,
    layout: u8,
    xattr_icount: u16,
}

impl Inode {
    fn file_type(self) -> u16 {
        self.mode & MODE_TYPE_MASK
    }
}

struct ErofsImage {
    file: File,
    image_bytes: u64,
    superblock: Superblock,
}

/// Compiles a same-size one-block replacement for an uncompressed flat EROFS file.
///
/// Stage 9 intentionally targets the minimal EROFS core format. It resolves the path
/// itself, requires a regular `FLAT_PLAIN` file exactly one filesystem block long, then
/// replaces only that physical data block through the filesystem-agnostic Loom view.
/// No EROFS metadata and no authoritative-origin bytes are modified.
///
/// # Errors
/// Returns [`ErofsError`] for malformed/unsupported images, path failures, replacement
/// size mismatches, effective-view failures, or ordinary I/O errors.
pub fn compile_same_size_replacement(
    origin_path: &Path,
    target_path: &str,
    replacement_path: &Path,
) -> Result<CompiledErofsReplacement, ErofsError> {
    let replacement = fs::read(replacement_path).map_err(ErofsError::Io)?;
    let mut image = ErofsImage::open(origin_path)?;
    let nid = image.resolve_path(target_path)?;
    let inode = image.read_inode(nid)?;

    if inode.file_type() != MODE_REGULAR {
        return Err(ErofsError::NotRegularFile(nid));
    }
    if inode.xattr_icount != 0 {
        return Err(ErofsError::UnsupportedInodeFeature(
            "Stage 9 refuses regular files with inline/shared xattr metadata",
        ));
    }
    if inode.layout != DATA_FLAT_PLAIN {
        return Err(ErofsError::UnsupportedInodeFeature(
            "Stage 9 replacement requires EROFS_INODE_FLAT_PLAIN",
        ));
    }
    let expected_size = u64::from(image.superblock.block_size);
    if inode.size != expected_size {
        return Err(ErofsError::UnsupportedInodeFeature(
            "Stage 9 replacement requires exactly one filesystem data block",
        ));
    }
    let replacement_size =
        u64::try_from(replacement.len()).map_err(|_| ErofsError::ArithmeticOverflow)?;
    if replacement_size != inode.size {
        return Err(ErofsError::ReplacementSizeMismatch {
            original: inode.size,
            replacement: replacement_size,
        });
    }

    let mut view = EffectiveBlockStore::open(origin_path, image.superblock.block_size)
        .map_err(ErofsError::View)?;
    view.block_mut(inode.start_block)
        .map_err(ErofsError::View)?
        .copy_from_slice(&replacement);
    let compiled = view.finalize().map_err(ErofsError::View)?;

    Ok(CompiledErofsReplacement {
        map: compiled.map,
        shadow: compiled.shadow,
        block_size: compiled.block_size,
        nid: inode.nid,
        start_block: inode.start_block,
        shadow_blocks: compiled.shadow_blocks,
    })
}

impl ErofsImage {
    fn open(path: &Path) -> Result<Self, ErofsError> {
        let mut file = File::open(path).map_err(ErofsError::Io)?;
        let image_bytes = file.metadata().map_err(ErofsError::Io)?.len();
        let superblock = read_superblock(&mut file, image_bytes)?;
        Ok(Self {
            file,
            image_bytes,
            superblock,
        })
    }

    fn read_inode(&mut self, nid: u64) -> Result<Inode, ErofsError> {
        let metadata_base = self
            .superblock
            .meta_block
            .checked_mul(u64::from(self.superblock.block_size))
            .ok_or(ErofsError::ArithmeticOverflow)?;
        let slot_offset = nid.checked_mul(32).ok_or(ErofsError::ArithmeticOverflow)?;
        let offset = metadata_base
            .checked_add(slot_offset)
            .ok_or(ErofsError::ArithmeticOverflow)?;
        ensure_range(self.image_bytes, offset, 32)?;

        let mut compact = [0_u8; 32];
        read_exact_at(&mut self.file, offset, &mut compact)?;
        let format = read_u16(&compact, 0)?;
        if format & 0xfff0 != 0 {
            return Err(ErofsError::UnsupportedInodeFeature(
                "non-core EROFS inode format bits are set",
            ));
        }
        let extended = format & 1 != 0;
        let layout =
            u8::try_from((format >> 1) & 0x7).map_err(|_| ErofsError::ArithmeticOverflow)?;
        if layout > 4 {
            return Err(ErofsError::UnsupportedInodeFeature(
                "reserved EROFS inode data layout",
            ));
        }
        let xattr_icount = read_u16(&compact, 2)?;
        let mode = read_u16(&compact, 4)?;
        let start_block = u64::from(read_u32(&compact, 0x10)?);

        let size = if extended {
            ensure_range(self.image_bytes, offset, 64)?;
            let mut raw = [0_u8; 64];
            read_exact_at(&mut self.file, offset, &mut raw)?;
            read_u64(&raw, 8)?
        } else {
            u64::from(read_u32(&compact, 8)?)
        };

        Ok(Inode {
            nid,
            mode,
            size,
            start_block,
            layout,
            xattr_icount,
        })
    }

    fn resolve_path(&mut self, path: &str) -> Result<u64, ErofsError> {
        let components = parse_absolute_path(path)?;
        let mut current = self.superblock.root_nid;
        for component in components {
            let directory = self.read_inode(current)?;
            if directory.file_type() != MODE_DIRECTORY {
                return Err(ErofsError::NotDirectory(current));
            }
            current = self.find_child(&directory, component.as_bytes())?;
        }
        Ok(current)
    }

    fn find_child(&mut self, directory: &Inode, name: &[u8]) -> Result<u64, ErofsError> {
        if directory.xattr_icount != 0 {
            return Err(ErofsError::UnsupportedInodeFeature(
                "Stage 9 path traversal refuses directories with xattrs",
            ));
        }
        let block_size = u64::from(self.superblock.block_size);
        let full_blocks = match directory.layout {
            DATA_FLAT_PLAIN => div_ceil(directory.size, block_size)?,
            DATA_FLAT_INLINE => directory.size / block_size,
            _ => {
                return Err(ErofsError::UnsupportedInodeFeature(
                    "Stage 9 path traversal supports only flat EROFS directories",
                ))
            }
        };

        for index in 0..full_blocks {
            let block = directory
                .start_block
                .checked_add(index)
                .ok_or(ErofsError::ArithmeticOverflow)?;
            let bytes = self.read_block(block)?;
            if let Some(nid) = find_in_directory_block(&bytes, name)? {
                return Ok(nid);
            }
        }

        if directory.layout == DATA_FLAT_INLINE && directory.size % block_size != 0 {
            return Err(ErofsError::InlineDirectoryTailUnsupported(
                String::from_utf8_lossy(name).into_owned(),
            ));
        }
        Err(ErofsError::PathNotFound(
            String::from_utf8_lossy(name).into_owned(),
        ))
    }

    fn read_block(&mut self, block: u64) -> Result<Vec<u8>, ErofsError> {
        let block_size = u64::from(self.superblock.block_size);
        let offset = block
            .checked_mul(block_size)
            .ok_or(ErofsError::ArithmeticOverflow)?;
        ensure_range(self.image_bytes, offset, block_size)?;
        let mut bytes =
            vec![0_u8; usize::try_from(block_size).map_err(|_| ErofsError::ArithmeticOverflow)?];
        read_exact_at(&mut self.file, offset, &mut bytes)?;
        Ok(bytes)
    }
}

fn read_superblock(file: &mut File, image_bytes: u64) -> Result<Superblock, ErofsError> {
    ensure_range(
        image_bytes,
        SUPERBLOCK_OFFSET,
        u64::try_from(SUPERBLOCK_SIZE).map_err(|_| ErofsError::ArithmeticOverflow)?,
    )?;
    let mut raw = [0_u8; SUPERBLOCK_SIZE];
    read_exact_at(file, SUPERBLOCK_OFFSET, &mut raw)?;
    let magic = read_u32(&raw, 0)?;
    if magic != EROFS_MAGIC {
        return Err(ErofsError::BadMagic(magic));
    }
    let block_bits = raw[0x0c];
    if !(9..=20).contains(&block_bits) {
        return Err(ErofsError::InvalidFilesystem(
            "EROFS blkszbits lies outside Stage 9 bounds",
        ));
    }
    let block_size = 1_u32
        .checked_shl(u32::from(block_bits))
        .ok_or(ErofsError::ArithmeticOverflow)?;
    if u64::from(block_size) % 512 != 0 {
        return Err(ErofsError::InvalidFilesystem(
            "EROFS block size is not sector aligned",
        ));
    }
    let feature_incompat = read_u32(&raw, 0x50)?;
    if feature_incompat != 0 {
        return Err(ErofsError::UnsupportedFilesystemFeature(
            "Stage 9 supports only the EROFS core format (feature_incompat == 0)",
        ));
    }
    if read_u16(&raw, 0x54)? != 0 {
        return Err(ErofsError::UnsupportedFilesystemFeature(
            "Stage 9 refuses compressed EROFS images",
        ));
    }
    if raw[0x5a] != 0 {
        return Err(ErofsError::UnsupportedFilesystemFeature(
            "Stage 9 requires core directory block sizing",
        ));
    }

    Ok(Superblock {
        block_size,
        root_nid: u64::from(read_u16(&raw, 0x0e)?),
        meta_block: u64::from(read_u32(&raw, 0x28)?),
    })
}

fn find_in_directory_block(block: &[u8], target: &[u8]) -> Result<Option<u64>, ErofsError> {
    if block.len() < DIRENT_SIZE {
        return Err(ErofsError::CorruptDirectory);
    }
    let first_name_offset = usize::from(read_u16(block, 8)?);
    if first_name_offset == 0
        || first_name_offset % DIRENT_SIZE != 0
        || first_name_offset > block.len()
    {
        return Err(ErofsError::CorruptDirectory);
    }
    let entry_count = first_name_offset / DIRENT_SIZE;
    if entry_count == 0 {
        return Err(ErofsError::CorruptDirectory);
    }

    for index in 0..entry_count {
        let entry_offset = index
            .checked_mul(DIRENT_SIZE)
            .ok_or(ErofsError::ArithmeticOverflow)?;
        let name_offset = usize::from(read_u16(block, entry_offset + 8)?);
        if name_offset < first_name_offset || name_offset >= block.len() {
            return Err(ErofsError::CorruptDirectory);
        }
        let name_end = if index + 1 < entry_count {
            let next_entry = entry_offset
                .checked_add(DIRENT_SIZE)
                .ok_or(ErofsError::ArithmeticOverflow)?;
            let next = usize::from(read_u16(block, next_entry + 8)?);
            if next < name_offset || next > block.len() {
                return Err(ErofsError::CorruptDirectory);
            }
            next
        } else {
            let tail = block
                .get(name_offset..)
                .ok_or(ErofsError::CorruptDirectory)?;
            name_offset
                .checked_add(
                    tail.iter()
                        .position(|byte| *byte == 0)
                        .unwrap_or(tail.len()),
                )
                .ok_or(ErofsError::ArithmeticOverflow)?
        };
        let entry_name = block
            .get(name_offset..name_end)
            .ok_or(ErofsError::CorruptDirectory)?;
        if entry_name == target {
            return Ok(Some(read_u64(block, entry_offset)?));
        }
    }
    Ok(None)
}

fn parse_absolute_path(path: &str) -> Result<Vec<&str>, ErofsError> {
    if !path.starts_with('/') {
        return Err(ErofsError::InvalidPath("path must be absolute"));
    }
    if path == "/" {
        return Ok(Vec::new());
    }
    let mut result = Vec::new();
    for component in path[1..].split('/') {
        if component.is_empty() {
            return Err(ErofsError::InvalidPath(
                "empty/repeated path component is not allowed",
            ));
        }
        if component == "." || component == ".." {
            return Err(ErofsError::InvalidPath(
                "dot and parent components are not allowed",
            ));
        }
        result.push(component);
    }
    Ok(result)
}

fn div_ceil(value: u64, divisor: u64) -> Result<u64, ErofsError> {
    value
        .checked_add(divisor.saturating_sub(1))
        .map(|rounded| rounded / divisor)
        .ok_or(ErofsError::ArithmeticOverflow)
}

fn ensure_range(image_bytes: u64, offset: u64, length: u64) -> Result<(), ErofsError> {
    let end = offset
        .checked_add(length)
        .ok_or(ErofsError::ArithmeticOverflow)?;
    if end > image_bytes {
        return Err(ErofsError::UnexpectedEndOfImage);
    }
    Ok(())
}

fn read_exact_at(file: &mut File, offset: u64, buffer: &mut [u8]) -> Result<(), ErofsError> {
    file.seek(SeekFrom::Start(offset)).map_err(ErofsError::Io)?;
    file.read_exact(buffer).map_err(ErofsError::Io)
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, ErofsError> {
    let end = offset
        .checked_add(2)
        .ok_or(ErofsError::ArithmeticOverflow)?;
    let raw: [u8; 2] = bytes
        .get(offset..end)
        .ok_or(ErofsError::UnexpectedEndOfStructure)?
        .try_into()
        .map_err(|_| ErofsError::UnexpectedEndOfStructure)?;
    Ok(u16::from_le_bytes(raw))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, ErofsError> {
    let end = offset
        .checked_add(4)
        .ok_or(ErofsError::ArithmeticOverflow)?;
    let raw: [u8; 4] = bytes
        .get(offset..end)
        .ok_or(ErofsError::UnexpectedEndOfStructure)?
        .try_into()
        .map_err(|_| ErofsError::UnexpectedEndOfStructure)?;
    Ok(u32::from_le_bytes(raw))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, ErofsError> {
    let end = offset
        .checked_add(8)
        .ok_or(ErofsError::ArithmeticOverflow)?;
    let raw: [u8; 8] = bytes
        .get(offset..end)
        .ok_or(ErofsError::UnexpectedEndOfStructure)?
        .try_into()
        .map_err(|_| ErofsError::UnexpectedEndOfStructure)?;
    Ok(u64::from_le_bytes(raw))
}

#[derive(Debug)]
pub enum ErofsError {
    Io(io::Error),
    View(ViewError),
    BadMagic(u32),
    InvalidFilesystem(&'static str),
    UnsupportedFilesystemFeature(&'static str),
    UnsupportedInodeFeature(&'static str),
    InvalidPath(&'static str),
    PathNotFound(String),
    InlineDirectoryTailUnsupported(String),
    NotDirectory(u64),
    NotRegularFile(u64),
    CorruptDirectory,
    ReplacementSizeMismatch { original: u64, replacement: u64 },
    UnexpectedEndOfImage,
    UnexpectedEndOfStructure,
    ArithmeticOverflow,
}

impl fmt::Display for ErofsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "EROFS I/O error: {error}"),
            Self::View(error) => write!(f, "Loom effective-view error: {error}"),
            Self::BadMagic(magic) => write!(f, "invalid EROFS magic {magic:#010x}"),
            Self::InvalidFilesystem(reason) => write!(f, "invalid EROFS filesystem: {reason}"),
            Self::UnsupportedFilesystemFeature(feature) => {
                write!(f, "unsupported EROFS filesystem feature: {feature}")
            }
            Self::UnsupportedInodeFeature(feature) => {
                write!(f, "unsupported EROFS inode feature: {feature}")
            }
            Self::InvalidPath(reason) => write!(f, "invalid EROFS target path: {reason}"),
            Self::PathNotFound(name) => write!(f, "EROFS path component not found: {name:?}"),
            Self::InlineDirectoryTailUnsupported(name) => write!(
                f,
                "EROFS path component {name:?} may lie in an inline directory tail unsupported by Stage 9"
            ),
            Self::NotDirectory(nid) => write!(f, "EROFS nid {nid} is not a directory"),
            Self::NotRegularFile(nid) => write!(f, "EROFS nid {nid} is not a regular file"),
            Self::CorruptDirectory => write!(f, "malformed EROFS directory block"),
            Self::ReplacementSizeMismatch {
                original,
                replacement,
            } => write!(
                f,
                "EROFS replacement size {replacement} does not match original size {original}"
            ),
            Self::UnexpectedEndOfImage => write!(f, "EROFS reference lies beyond image bytes"),
            Self::UnexpectedEndOfStructure => write!(f, "unexpected end of EROFS structure"),
            Self::ArithmeticOverflow => write!(f, "integer overflow while parsing EROFS"),
        }
    }
}

impl std::error::Error for ErofsError {
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
    fn absolute_paths_reject_parent_components() {
        assert!(matches!(
            parse_absolute_path("/a/../b"),
            Err(ErofsError::InvalidPath(_))
        ));
    }

    #[test]
    fn directory_block_parser_finds_exact_name() {
        let mut block = vec![0_u8; 4096];
        // Two 12-byte dirents; names begin at 24 and 27.
        block[0..8].copy_from_slice(&11_u64.to_le_bytes());
        block[8..10].copy_from_slice(&24_u16.to_le_bytes());
        block[12..20].copy_from_slice(&22_u64.to_le_bytes());
        block[20..22].copy_from_slice(&27_u16.to_le_bytes());
        block[24..27].copy_from_slice(b"abc");
        block[27..30].copy_from_slice(b"xyz");
        assert_eq!(find_in_directory_block(&block, b"xyz").unwrap(), Some(22));
    }
}

#![forbid(unsafe_code)]

mod allocate;
mod checksum;
mod create;
mod remove;
mod resize;
mod xattr;

pub use allocate::{compile_grow_with_block_allocation, CompiledAllocationGrow};
pub use create::{compile_create_file, CompiledCreateFile};
pub use remove::{compile_remove_file, CompiledRemoveFile};
pub use resize::{compile_resize_within_allocation, CompiledResize};
pub use xattr::{compile_selinux_xattr, CompiledSelinuxXattr};

use loom_map::{LoomMap, ReplacementExtent};
use loom_types::{Sector, SectorCount};
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

const SECTOR_SIZE: u64 = 512;
const SUPERBLOCK_OFFSET: u64 = 1024;
const SUPERBLOCK_SIZE: usize = 1024;
const EXT4_MAGIC: u16 = 0xEF53;
const EXT4_EXTENT_MAGIC: u16 = 0xF30A;
const ROOT_INODE: u32 = 2;

const INCOMPAT_FILETYPE: u32 = 0x0002;
const INCOMPAT_RECOVER: u32 = 0x0004;
const INCOMPAT_META_BG: u32 = 0x0010;
const INCOMPAT_EXTENTS: u32 = 0x0040;
const INCOMPAT_64BIT: u32 = 0x0080;
const INCOMPAT_FLEX_BG: u32 = 0x0200;
const INCOMPAT_CSUM_SEED: u32 = 0x2000;
const INCOMPAT_INLINE_DATA: u32 = 0x8000;
const INCOMPAT_ENCRYPT: u32 = 0x1_0000;
const INCOMPAT_CASEFOLD: u32 = 0x2_0000;
const SUPPORTED_INCOMPAT: u32 =
    INCOMPAT_FILETYPE | INCOMPAT_EXTENTS | INCOMPAT_64BIT | INCOMPAT_FLEX_BG | INCOMPAT_CSUM_SEED;

const RO_COMPAT_BIGALLOC: u32 = 0x0200;
const RO_COMPAT_ORPHAN_PRESENT: u32 = 0x1_0000;

const INODE_EXTENTS_FL: u32 = 0x0008_0000;
const INODE_VERITY_FL: u32 = 0x0010_0000;
const INODE_INLINE_DATA_FL: u32 = 0x1000_0000;
const MODE_TYPE_MASK: u16 = 0xF000;
const MODE_DIRECTORY: u16 = 0x4000;
const MODE_REGULAR: u16 = 0x8000;

/// Seekable read source consumed by the ext4 compiler.
pub trait ImageReader: Read + Seek {}

impl<T: Read + Seek> ImageReader for T {}

/// One compiler session over an arbitrary immutable effective-image reader.
pub struct Ext4Session {
    image: Ext4Image,
}

impl Ext4Session {
    /// Opens an ext4 compiler session over a virtual image reader.
    ///
    /// # Errors
    /// Returns [`Ext4Error`] when the supplied image is malformed or unsupported.
    pub fn from_reader<R>(reader: R, image_bytes: u64) -> Result<Self, Ext4Error>
    where
        R: ImageReader + 'static,
    {
        Ok(Self {
            image: Ext4Image::from_reader(Box::new(reader), image_bytes)?,
        })
    }

    /// Compiles a same-size replacement against the session's current effective view.
    ///
    /// # Errors
    /// Returns [`Ext4Error`] when the path or replacement is invalid.
    pub fn replace(
        &mut self,
        target_path: &str,
        replacement: &[u8],
    ) -> Result<CompiledReplacement, Ext4Error> {
        let inode = self.image.resolve_path(target_path)?;
        self.image.compile_regular_replacement(inode, replacement)
    }

    /// Compiles a within-allocation resize against the current effective view.
    ///
    /// # Errors
    /// Returns [`Ext4Error`] when the resize violates ext4 Stage 2 invariants.
    pub fn resize(
        &mut self,
        target_path: &str,
        replacement: &[u8],
    ) -> Result<CompiledResize, Ext4Error> {
        let inode = self.image.resolve_path(target_path)?;
        self.image.compile_resize(inode, replacement)
    }

    /// Compiles one-block growth against the current effective view.
    ///
    /// # Errors
    /// Returns [`Ext4Error`] when allocator or extent invariants are not satisfied.
    pub fn grow(
        &mut self,
        target_path: &str,
        replacement: &[u8],
    ) -> Result<CompiledAllocationGrow, Ext4Error> {
        let inode = self.image.resolve_path(target_path)?;
        self.image.compile_one_block_growth(inode, replacement)
    }

    /// Compiles creation of one regular file against the current effective view.
    ///
    /// # Errors
    /// Returns [`Ext4Error`] when allocation or directory invariants are not satisfied.
    pub fn create(
        &mut self,
        target_path: &str,
        payload: &[u8],
    ) -> Result<CompiledCreateFile, Ext4Error> {
        self.image.compile_create_file(target_path, payload)
    }

    /// Compiles removal of one regular file from the current effective view.
    ///
    /// # Errors
    /// Returns [`Ext4Error`] when removal invariants are not satisfied.
    pub fn remove(&mut self, target_path: &str) -> Result<CompiledRemoveFile, Ext4Error> {
        self.image.compile_remove_file(target_path)
    }

    /// Compiles an in-inode `security.selinux` xattr against the current effective view.
    ///
    /// # Errors
    /// Returns [`Ext4Error`] when the xattr cannot be represented safely.
    pub fn selinux(
        &mut self,
        target_path: &str,
        value: &[u8],
    ) -> Result<CompiledSelinuxXattr, Ext4Error> {
        self.image.compile_selinux_xattr_bytes(target_path, value)
    }
}

#[derive(Debug)]
pub struct CompiledReplacement {
    pub map: LoomMap,
    pub shadow: Vec<u8>,
    pub block_size: u32,
    pub inode: u32,
    pub data_blocks: usize,
    pub shadow_blocks: usize,
}

/// Compiles one same-size ext4 path replacement into shadow blocks and a Loom map.
///
/// The origin is opened read-only. Stage 1 deliberately rejects filesystem or inode
/// features whose on-disk semantics are not modeled yet.
///
/// # Errors
/// Returns [`Ext4Error`] for malformed/unsupported ext4 structures, invalid paths,
/// size mismatches, mapping failures, or filesystem I/O errors.
pub fn compile_same_size_replacement(
    origin_path: &Path,
    target_path: &str,
    replacement_path: &Path,
) -> Result<CompiledReplacement, Ext4Error> {
    let replacement = fs::read(replacement_path).map_err(Ext4Error::Io)?;
    let mut image = Ext4Image::open(origin_path)?;
    let inode_number = image.resolve_path(target_path)?;
    image.compile_regular_replacement(inode_number, &replacement)
}

struct Ext4Image {
    file: Box<dyn ImageReader>,
    image_bytes: u64,
    superblock: Superblock,
}

impl Ext4Image {
    fn open(path: &Path) -> Result<Self, Ext4Error> {
        let file = File::open(path).map_err(Ext4Error::Io)?;
        let image_bytes = file.metadata().map_err(Ext4Error::Io)?.len();
        Self::from_reader(Box::new(file), image_bytes)
    }

    fn from_reader(mut file: Box<dyn ImageReader>, image_bytes: u64) -> Result<Self, Ext4Error> {
        if image_bytes % SECTOR_SIZE != 0 {
            return Err(Ext4Error::InvalidFilesystem(
                "origin size is not a multiple of 512 bytes",
            ));
        }

        let mut bytes = [0_u8; SUPERBLOCK_SIZE];
        read_exact_at(&mut file, SUPERBLOCK_OFFSET, &mut bytes)?;
        let superblock = Superblock::parse(&bytes)?;
        let fs_bytes = superblock
            .blocks_count
            .checked_mul(u64::from(superblock.block_size))
            .ok_or(Ext4Error::ArithmeticOverflow)?;
        if fs_bytes > image_bytes {
            return Err(Ext4Error::InvalidFilesystem(
                "filesystem block count exceeds origin size",
            ));
        }

        Ok(Self {
            file,
            image_bytes,
            superblock,
        })
    }

    fn resolve_path(&mut self, path: &str) -> Result<u32, Ext4Error> {
        let components = parse_absolute_path(path)?;
        let mut inode_number = ROOT_INODE;

        for component in components {
            let directory = self.read_inode(inode_number)?;
            if directory.file_type() != MODE_DIRECTORY {
                return Err(Ext4Error::NotDirectory(inode_number));
            }
            inode_number = self.find_directory_entry(inode_number, &directory, component)?;
        }

        Ok(inode_number)
    }

    fn compile_regular_replacement(
        &mut self,
        inode_number: u32,
        replacement: &[u8],
    ) -> Result<CompiledReplacement, Ext4Error> {
        let inode = self.read_inode(inode_number)?;
        if inode.file_type() != MODE_REGULAR {
            return Err(Ext4Error::NotRegularFile(inode_number));
        }
        if inode.links_count != 1 {
            return Err(Ext4Error::HardLinkedTarget {
                inode: inode_number,
                links: inode.links_count,
            });
        }
        if inode.flags & INODE_INLINE_DATA_FL != 0 {
            return Err(Ext4Error::UnsupportedInodeFeature("inline data"));
        }
        if inode.flags & INODE_VERITY_FL != 0 {
            return Err(Ext4Error::UnsupportedInodeFeature("fs-verity"));
        }

        let replacement_len =
            u64::try_from(replacement.len()).map_err(|_| Ext4Error::ArithmeticOverflow)?;
        if replacement_len != inode.size {
            return Err(Ext4Error::ReplacementSizeMismatch {
                original: inode.size,
                replacement: replacement_len,
            });
        }

        let blocks = self.file_blocks(&inode)?;
        let block_size = usize::try_from(self.superblock.block_size)
            .map_err(|_| Ext4Error::ArithmeticOverflow)?;
        let sectors_per_block = u64::from(self.superblock.block_size) / SECTOR_SIZE;
        let mut shadow = Vec::new();
        let mut replacements = Vec::with_capacity(blocks.len());
        let mut changed_blocks = 0_usize;

        for (file_block_index, physical_block) in blocks.iter().copied().enumerate() {
            let origin_block = self.read_block(physical_block)?;
            let mut effective_block = origin_block.clone();
            let replacement_offset = file_block_index
                .checked_mul(block_size)
                .ok_or(Ext4Error::ArithmeticOverflow)?;
            let remaining = replacement.len().saturating_sub(replacement_offset);
            let copy_len = remaining.min(block_size);
            if copy_len != 0 {
                let replacement_end = replacement_offset
                    .checked_add(copy_len)
                    .ok_or(Ext4Error::ArithmeticOverflow)?;
                effective_block[..copy_len]
                    .copy_from_slice(&replacement[replacement_offset..replacement_end]);
            }

            if effective_block == origin_block {
                continue;
            }

            let shadow_index_u64 =
                u64::try_from(changed_blocks).map_err(|_| Ext4Error::ArithmeticOverflow)?;
            let logical_start = physical_block
                .checked_mul(sectors_per_block)
                .ok_or(Ext4Error::ArithmeticOverflow)?;
            let shadow_start = shadow_index_u64
                .checked_mul(sectors_per_block)
                .ok_or(Ext4Error::ArithmeticOverflow)?;

            replacements.push(ReplacementExtent {
                logical_start: Sector(logical_start),
                sector_count: SectorCount(sectors_per_block),
                shadow_start: Sector(shadow_start),
            });
            shadow.extend_from_slice(&effective_block);
            changed_blocks = changed_blocks
                .checked_add(1)
                .ok_or(Ext4Error::ArithmeticOverflow)?;
        }

        let total_sectors = self.image_bytes / SECTOR_SIZE;
        let map = LoomMap::from_replacements(SectorCount(total_sectors), &replacements)
            .map_err(Ext4Error::Map)?;

        Ok(CompiledReplacement {
            map,
            shadow,
            block_size: self.superblock.block_size,
            inode: inode_number,
            data_blocks: blocks.len(),
            shadow_blocks: changed_blocks,
        })
    }

    fn find_directory_entry(
        &mut self,
        directory_inode_number: u32,
        directory: &Inode,
        name: &str,
    ) -> Result<u32, Ext4Error> {
        let blocks = self.file_blocks(directory)?;
        let needle = name.as_bytes();

        for physical_block in blocks {
            let block = self.read_block(physical_block)?;
            let mut offset = 0_usize;
            while offset < block.len() {
                let remaining = block.len() - offset;
                if remaining < 8 {
                    return Err(Ext4Error::CorruptDirectory(directory_inode_number));
                }

                let inode_number = read_u32(&block, offset)?;
                let record_len = usize::from(read_u16(&block, offset + 4)?);
                if record_len < 8 || record_len % 4 != 0 || record_len > remaining {
                    return Err(Ext4Error::CorruptDirectory(directory_inode_number));
                }

                let name_len = if self.superblock.has_filetype {
                    usize::from(block[offset + 6])
                } else {
                    usize::from(read_u16(&block, offset + 6)?)
                };
                if name_len > record_len - 8 {
                    return Err(Ext4Error::CorruptDirectory(directory_inode_number));
                }

                if inode_number != 0 {
                    let name_start = offset + 8;
                    let name_end = name_start
                        .checked_add(name_len)
                        .ok_or(Ext4Error::ArithmeticOverflow)?;
                    if block.get(name_start..name_end) == Some(needle) {
                        return Ok(inode_number);
                    }
                }

                offset = offset
                    .checked_add(record_len)
                    .ok_or(Ext4Error::ArithmeticOverflow)?;
            }
        }

        Err(Ext4Error::PathNotFound(name.to_owned()))
    }

    fn file_blocks(&mut self, inode: &Inode) -> Result<Vec<u64>, Ext4Error> {
        if inode.flags & INODE_EXTENTS_FL == 0 {
            return Err(Ext4Error::UnsupportedInodeFeature(
                "legacy indirect block mapping",
            ));
        }
        if inode.flags & INODE_INLINE_DATA_FL != 0 {
            return Err(Ext4Error::UnsupportedInodeFeature("inline data"));
        }

        let mut extents = Vec::new();
        self.collect_extent_node(&inode.block, None, &mut extents)?;
        extents.sort_by_key(|extent| extent.logical_start);

        let block_size = u64::from(self.superblock.block_size);
        let blocks_needed = inode
            .size
            .checked_add(block_size.saturating_sub(1))
            .ok_or(Ext4Error::ArithmeticOverflow)?
            / block_size;
        let blocks_needed_u32 =
            u32::try_from(blocks_needed).map_err(|_| Ext4Error::ArithmeticOverflow)?;

        let mut blocks = Vec::with_capacity(
            usize::try_from(blocks_needed).map_err(|_| Ext4Error::ArithmeticOverflow)?,
        );
        let mut expected_logical = 0_u32;

        for extent in extents {
            let extent_end = extent
                .logical_start
                .checked_add(extent.length)
                .ok_or(Ext4Error::ArithmeticOverflow)?;
            if extent.logical_start < expected_logical {
                return Err(Ext4Error::CorruptExtentTree);
            }
            if extent.logical_start > expected_logical && expected_logical < blocks_needed_u32 {
                return Err(Ext4Error::SparseFileUnsupported);
            }

            let usable_end = extent_end.min(blocks_needed_u32);
            for logical in extent.logical_start..usable_end {
                let delta = u64::from(logical - extent.logical_start);
                let physical = extent
                    .physical_start
                    .checked_add(delta)
                    .ok_or(Ext4Error::ArithmeticOverflow)?;
                blocks.push(physical);
            }
            expected_logical = extent_end;
            if expected_logical >= blocks_needed_u32 {
                break;
            }
        }

        if expected_logical < blocks_needed_u32 {
            return Err(Ext4Error::SparseFileUnsupported);
        }
        Ok(blocks)
    }

    fn collect_extent_node(
        &mut self,
        node: &[u8],
        expected_depth: Option<u16>,
        output: &mut Vec<FileExtent>,
    ) -> Result<(), Ext4Error> {
        if node.len() < 12 {
            return Err(Ext4Error::CorruptExtentTree);
        }
        if read_u16(node, 0)? != EXT4_EXTENT_MAGIC {
            return Err(Ext4Error::CorruptExtentTree);
        }

        let entries = usize::from(read_u16(node, 2)?);
        let maximum = usize::from(read_u16(node, 4)?);
        let depth = read_u16(node, 6)?;
        if depth > 5 || expected_depth.is_some_and(|expected| expected != depth) {
            return Err(Ext4Error::CorruptExtentTree);
        }

        let capacity = (node.len() - 12) / 12;
        if entries > maximum || maximum > capacity || entries > capacity {
            return Err(Ext4Error::CorruptExtentTree);
        }

        for index in 0..entries {
            let offset = 12_usize
                .checked_add(index.checked_mul(12).ok_or(Ext4Error::ArithmeticOverflow)?)
                .ok_or(Ext4Error::ArithmeticOverflow)?;
            if depth == 0 {
                let logical_start = read_u32(node, offset)?;
                let raw_length = read_u16(node, offset + 4)?;
                if raw_length == 0 || raw_length > 32_768 {
                    return Err(Ext4Error::UnsupportedInodeFeature("unwritten extent"));
                }
                let start_hi = u64::from(read_u16(node, offset + 6)?);
                let start_lo = u64::from(read_u32(node, offset + 8)?);
                let physical_start = (start_hi << 32) | start_lo;
                let length = u32::from(raw_length);
                let physical_end = physical_start
                    .checked_add(u64::from(length))
                    .ok_or(Ext4Error::ArithmeticOverflow)?;
                if physical_end > self.superblock.blocks_count {
                    return Err(Ext4Error::CorruptExtentTree);
                }
                output.push(FileExtent {
                    logical_start,
                    length,
                    physical_start,
                });
            } else {
                let leaf_lo = u64::from(read_u32(node, offset + 4)?);
                let leaf_hi = u64::from(read_u16(node, offset + 8)?);
                let child_block = (leaf_hi << 32) | leaf_lo;
                if child_block >= self.superblock.blocks_count {
                    return Err(Ext4Error::CorruptExtentTree);
                }
                let child = self.read_block(child_block)?;
                self.collect_extent_node(&child, Some(depth - 1), output)?;
            }
        }

        Ok(())
    }

    fn read_inode(&mut self, inode_number: u32) -> Result<Inode, Ext4Error> {
        if inode_number == 0 || inode_number > self.superblock.inodes_count {
            return Err(Ext4Error::InvalidInode(inode_number));
        }

        let zero_based = inode_number - 1;
        let group = zero_based / self.superblock.inodes_per_group;
        let index = zero_based % self.superblock.inodes_per_group;
        let inode_table_block = self.inode_table_block(group)?;
        let table_offset = inode_table_block
            .checked_mul(u64::from(self.superblock.block_size))
            .ok_or(Ext4Error::ArithmeticOverflow)?;
        let inode_offset = u64::from(index)
            .checked_mul(u64::from(self.superblock.inode_size))
            .ok_or(Ext4Error::ArithmeticOverflow)?;
        let offset = table_offset
            .checked_add(inode_offset)
            .ok_or(Ext4Error::ArithmeticOverflow)?;
        let inode_size = usize::from(self.superblock.inode_size);
        let mut bytes = vec![0_u8; inode_size];
        read_exact_at(&mut self.file, offset, &mut bytes)?;
        Inode::parse(&bytes)
    }

    fn inode_table_block(&mut self, group: u32) -> Result<u64, Ext4Error> {
        let descriptor_start_block = if self.superblock.block_size == 1024 {
            2_u64
        } else {
            1_u64
        };
        let descriptor_table_offset = descriptor_start_block
            .checked_mul(u64::from(self.superblock.block_size))
            .ok_or(Ext4Error::ArithmeticOverflow)?;
        let group_offset = u64::from(group)
            .checked_mul(u64::from(self.superblock.descriptor_size))
            .ok_or(Ext4Error::ArithmeticOverflow)?;
        let offset = descriptor_table_offset
            .checked_add(group_offset)
            .ok_or(Ext4Error::ArithmeticOverflow)?;
        let descriptor_size = usize::from(self.superblock.descriptor_size);
        let mut descriptor = vec![0_u8; descriptor_size];
        read_exact_at(&mut self.file, offset, &mut descriptor)?;

        let low = u64::from(read_u32(&descriptor, 0x08)?);
        let high = if self.superblock.has_64bit {
            u64::from(read_u32(&descriptor, 0x28)?)
        } else {
            0
        };
        let block = (high << 32) | low;
        if block >= self.superblock.blocks_count {
            return Err(Ext4Error::InvalidFilesystem(
                "inode table lies outside filesystem",
            ));
        }
        Ok(block)
    }

    fn read_block(&mut self, block: u64) -> Result<Vec<u8>, Ext4Error> {
        if block >= self.superblock.blocks_count {
            return Err(Ext4Error::InvalidFilesystem(
                "block lies outside filesystem",
            ));
        }
        let offset = block
            .checked_mul(u64::from(self.superblock.block_size))
            .ok_or(Ext4Error::ArithmeticOverflow)?;
        let block_size = usize::try_from(self.superblock.block_size)
            .map_err(|_| Ext4Error::ArithmeticOverflow)?;
        let mut bytes = vec![0_u8; block_size];
        read_exact_at(&mut self.file, offset, &mut bytes)?;
        Ok(bytes)
    }
}

#[derive(Debug, Clone, Copy)]
struct Superblock {
    inodes_count: u32,
    blocks_count: u64,
    block_size: u32,
    inodes_per_group: u32,
    inode_size: u16,
    descriptor_size: u16,
    has_64bit: bool,
    has_filetype: bool,
}

impl Superblock {
    fn parse(bytes: &[u8]) -> Result<Self, Ext4Error> {
        if read_u16(bytes, 0x38)? != EXT4_MAGIC {
            return Err(Ext4Error::InvalidFilesystem("bad ext4 magic"));
        }

        let log_block_size = read_u32(bytes, 0x18)?;
        if log_block_size > 6 {
            return Err(Ext4Error::InvalidFilesystem("invalid ext4 block size"));
        }
        let block_size = 1024_u32
            .checked_shl(log_block_size)
            .ok_or(Ext4Error::ArithmeticOverflow)?;
        if u64::from(block_size) % SECTOR_SIZE != 0 {
            return Err(Ext4Error::UnsupportedFilesystemFeature(
                "block size is not sector aligned",
            ));
        }

        let feature_incompat = read_u32(bytes, 0x60)?;
        let feature_ro_compat = read_u32(bytes, 0x64)?;
        if feature_incompat & INCOMPAT_RECOVER != 0 {
            return Err(Ext4Error::UnsupportedFilesystemFeature(
                "journal recovery required",
            ));
        }
        if feature_incompat & INCOMPAT_META_BG != 0 {
            return Err(Ext4Error::UnsupportedFilesystemFeature("meta_bg"));
        }
        if feature_incompat & INCOMPAT_INLINE_DATA != 0 {
            return Err(Ext4Error::UnsupportedFilesystemFeature("inline data"));
        }
        if feature_incompat & INCOMPAT_ENCRYPT != 0 {
            return Err(Ext4Error::UnsupportedFilesystemFeature("encryption"));
        }
        if feature_incompat & INCOMPAT_CASEFOLD != 0 {
            return Err(Ext4Error::UnsupportedFilesystemFeature("casefold"));
        }
        if feature_incompat & INCOMPAT_EXTENTS == 0 {
            return Err(Ext4Error::UnsupportedFilesystemFeature(
                "filesystem does not advertise extents",
            ));
        }
        let unknown_incompat = feature_incompat & !SUPPORTED_INCOMPAT;
        if unknown_incompat != 0 {
            return Err(Ext4Error::UnknownIncompatibleFeatures(unknown_incompat));
        }
        if feature_ro_compat & RO_COMPAT_BIGALLOC != 0 {
            return Err(Ext4Error::UnsupportedFilesystemFeature("bigalloc"));
        }
        if feature_ro_compat & RO_COMPAT_ORPHAN_PRESENT != 0 {
            return Err(Ext4Error::UnsupportedFilesystemFeature(
                "orphan cleanup required",
            ));
        }

        let has_64bit = feature_incompat & INCOMPAT_64BIT != 0;
        let descriptor_size = if has_64bit {
            let size = read_u16(bytes, 0xFE)?;
            if size < 64 {
                return Err(Ext4Error::InvalidFilesystem(
                    "64-bit ext4 descriptor is smaller than 64 bytes",
                ));
            }
            size
        } else {
            32
        };

        let inode_size = read_u16(bytes, 0x58)?;
        if inode_size < 128 || !inode_size.is_power_of_two() {
            return Err(Ext4Error::InvalidFilesystem("invalid inode size"));
        }
        if u32::from(inode_size) > block_size {
            return Err(Ext4Error::InvalidFilesystem(
                "inode size exceeds filesystem block size",
            ));
        }
        let inodes_per_group = read_u32(bytes, 0x28)?;
        if inodes_per_group == 0 {
            return Err(Ext4Error::InvalidFilesystem(
                "inodes_per_group must be non-zero",
            ));
        }

        let blocks_count_low = u64::from(read_u32(bytes, 0x04)?);
        let blocks_count_high = if has_64bit {
            u64::from(read_u32(bytes, 0x150)?)
        } else {
            0
        };
        let blocks_count = (blocks_count_high << 32) | blocks_count_low;
        if blocks_count == 0 {
            return Err(Ext4Error::InvalidFilesystem("filesystem has zero blocks"));
        }

        Ok(Self {
            inodes_count: read_u32(bytes, 0x00)?,
            blocks_count,
            block_size,
            inodes_per_group,
            inode_size,
            descriptor_size,
            has_64bit,
            has_filetype: feature_incompat & INCOMPAT_FILETYPE != 0,
        })
    }
}

#[derive(Debug, Clone)]
struct Inode {
    mode: u16,
    size: u64,
    flags: u32,
    links_count: u16,
    block: [u8; 60],
}

impl Inode {
    fn parse(bytes: &[u8]) -> Result<Self, Ext4Error> {
        if bytes.len() < 128 {
            return Err(Ext4Error::InvalidFilesystem("inode record too small"));
        }
        let size_low = u64::from(read_u32(bytes, 0x04)?);
        let size_high = u64::from(read_u32(bytes, 0x6C)?);
        let block: [u8; 60] = bytes
            .get(0x28..0x64)
            .ok_or(Ext4Error::InvalidFilesystem("inode i_block missing"))?
            .try_into()
            .map_err(|_| Ext4Error::InvalidFilesystem("inode i_block malformed"))?;

        Ok(Self {
            mode: read_u16(bytes, 0x00)?,
            size: (size_high << 32) | size_low,
            flags: read_u32(bytes, 0x20)?,
            links_count: read_u16(bytes, 0x1A)?,
            block,
        })
    }

    fn file_type(&self) -> u16 {
        self.mode & MODE_TYPE_MASK
    }
}

#[derive(Debug, Clone, Copy)]
struct FileExtent {
    logical_start: u32,
    length: u32,
    physical_start: u64,
}

fn parse_absolute_path(path: &str) -> Result<Vec<&str>, Ext4Error> {
    if !path.starts_with('/') {
        return Err(Ext4Error::InvalidPath("path must be absolute"));
    }
    if path == "/" {
        return Err(Ext4Error::InvalidPath("root cannot be replaced"));
    }
    if path.contains("//") {
        return Err(Ext4Error::InvalidPath("empty path component"));
    }

    let mut components = Vec::new();
    for component in path.trim_start_matches('/').split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return Err(Ext4Error::InvalidPath("invalid path component"));
        }
        if component.len() > 255 || component.as_bytes().contains(&0) {
            return Err(Ext4Error::InvalidPath(
                "path component is not ext4-compatible",
            ));
        }
        components.push(component);
    }
    Ok(components)
}

fn read_exact_at<R: Read + Seek + ?Sized>(
    file: &mut R,
    offset: u64,
    buffer: &mut [u8],
) -> Result<(), Ext4Error> {
    file.seek(SeekFrom::Start(offset)).map_err(Ext4Error::Io)?;
    file.read_exact(buffer).map_err(Ext4Error::Io)
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, Ext4Error> {
    let end = offset.checked_add(2).ok_or(Ext4Error::ArithmeticOverflow)?;
    let raw: [u8; 2] = bytes
        .get(offset..end)
        .ok_or(Ext4Error::UnexpectedEndOfStructure)?
        .try_into()
        .map_err(|_| Ext4Error::UnexpectedEndOfStructure)?;
    Ok(u16::from_le_bytes(raw))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, Ext4Error> {
    let end = offset.checked_add(4).ok_or(Ext4Error::ArithmeticOverflow)?;
    let raw: [u8; 4] = bytes
        .get(offset..end)
        .ok_or(Ext4Error::UnexpectedEndOfStructure)?
        .try_into()
        .map_err(|_| Ext4Error::UnexpectedEndOfStructure)?;
    Ok(u32::from_le_bytes(raw))
}

#[derive(Debug)]
pub enum Ext4Error {
    Io(io::Error),
    Map(loom_map::MapError),
    Checksum(checksum::ChecksumError),
    InvalidFilesystem(&'static str),
    UnsupportedFilesystemFeature(&'static str),
    UnknownIncompatibleFeatures(u32),
    UnsupportedInodeFeature(&'static str),
    InvalidPath(&'static str),
    PathNotFound(String),
    InvalidInode(u32),
    NotDirectory(u32),
    NotRegularFile(u32),
    HardLinkedTarget {
        inode: u32,
        links: u16,
    },
    CorruptDirectory(u32),
    CorruptExtentTree,
    SparseFileUnsupported,
    ReplacementSizeMismatch {
        original: u64,
        replacement: u64,
    },
    ResizeSizeUnchanged(u64),
    ResizeCrossesAllocationBoundary {
        original_size: u64,
        effective_size: u64,
        allocated_blocks: u64,
        required_blocks: u64,
    },
    UnexpectedEndOfStructure,
    ArithmeticOverflow,
}

impl fmt::Display for Ext4Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "I/O error: {error}"),
            Self::Map(error) => write!(f, "Loom map error: {error}"),
            Self::Checksum(error) => write!(f, "ext4 inode checksum error: {error}"),
            Self::InvalidFilesystem(reason) => write!(f, "invalid ext4 filesystem: {reason}"),
            Self::UnsupportedFilesystemFeature(feature) => {
                write!(f, "unsupported ext4 filesystem feature: {feature}")
            }
            Self::UnknownIncompatibleFeatures(bits) => {
                write!(f, "unknown ext4 incompatible feature bits: {bits:#x}")
            }
            Self::UnsupportedInodeFeature(feature) => {
                write!(f, "unsupported ext4 inode feature: {feature}")
            }
            Self::InvalidPath(reason) => write!(f, "invalid target path: {reason}"),
            Self::PathNotFound(name) => write!(f, "path component not found: {name:?}"),
            Self::InvalidInode(inode) => write!(f, "invalid inode number {inode}"),
            Self::NotDirectory(inode) => write!(f, "inode {inode} is not a directory"),
            Self::NotRegularFile(inode) => write!(f, "inode {inode} is not a regular file"),
            Self::HardLinkedTarget { inode, links } => write!(
                f,
                "inode {inode} is hard-linked ({links} links); Stage 1 path replacement refuses alias-wide mutation"
            ),
            Self::CorruptDirectory(inode) => write!(f, "directory inode {inode} is malformed"),
            Self::CorruptExtentTree => write!(f, "ext4 extent tree is malformed"),
            Self::SparseFileUnsupported => {
                write!(f, "sparse ext4 files are not supported in Stage 1")
            }
            Self::ReplacementSizeMismatch {
                original,
                replacement,
            } => write!(
                f,
                "replacement size {replacement} does not match original size {original}"
            ),
            Self::ResizeSizeUnchanged(size) => {
                write!(f, "resize replacement keeps the existing size {size}")
            }
            Self::ResizeCrossesAllocationBoundary {
                original_size,
                effective_size,
                allocated_blocks,
                required_blocks,
            } => write!(
                f,
                "Stage 2 resize {original_size} -> {effective_size} bytes changes the logical allocation boundary: allocated={allocated_blocks} required={required_blocks}"
            ),
            Self::UnexpectedEndOfStructure => write!(f, "unexpected end of ext4 structure"),
            Self::ArithmeticOverflow => write!(f, "integer overflow while parsing ext4"),
        }
    }
}

impl std::error::Error for Ext4Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Map(error) => Some(error),
            Self::Checksum(error) => Some(error),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_path_parser_rejects_parent_components() {
        let error = parse_absolute_path("/system/../vendor").unwrap_err();
        assert!(matches!(error, Ext4Error::InvalidPath(_)));
    }

    #[test]
    fn absolute_path_parser_preserves_components() {
        let components = parse_absolute_path("/system/etc/loom.conf").unwrap();
        assert_eq!(components, vec!["system", "etc", "loom.conf"]);
    }
}

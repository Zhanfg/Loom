#![forbid(unsafe_code)]

use super::checksum::{crc32c, inode_seed, rewrite_inode_checksum};
use super::{
    parse_absolute_path, read_exact_at, read_u16, read_u32, Ext4Error, Ext4Image, Inode,
    INODE_EXTENTS_FL, INODE_INLINE_DATA_FL, MODE_DIRECTORY, MODE_REGULAR, ROOT_INODE, SECTOR_SIZE,
    SUPERBLOCK_OFFSET, SUPERBLOCK_SIZE,
};
use loom_map::{LoomMap, ReplacementExtent};
use loom_types::{Sector, SectorCount};
use std::fs;
use std::path::Path;

const RO_COMPAT_METADATA_CSUM: u32 = 0x0400;
const INCOMPAT_CSUM_SEED: u32 = 0x2000;
const EXT4_OS_LINUX: u32 = 0;
const EXT4_CRC32C_CHKSUM: u8 = 1;
const EXT4_BG_INODE_UNINIT: u16 = 0x0001;
const EXT4_BG_BLOCK_UNINIT: u16 = 0x0002;
const EXT4_BG_INODE_ZEROED: u16 = 0x0004;
const EXT4_INDEX_FL: u32 = 0x0000_1000;
const EXT4_FT_REG_FILE: u8 = 1;
const EXT4_FT_DIR_CSUM: u8 = 0xde;

const SB_FREE_BLOCKS_LO: usize = 0x0c;
const SB_FREE_INODES: usize = 0x10;
const SB_FIRST_DATA_BLOCK: usize = 0x14;
const SB_BLOCKS_PER_GROUP: usize = 0x20;
const SB_FIRST_INO: usize = 0x54;
const SB_CREATOR_OS: usize = 0x48;
const SB_FEATURE_INCOMPAT: usize = 0x60;
const SB_FEATURE_RO_COMPAT: usize = 0x64;
const SB_UUID: usize = 0x68;
const SB_UUID_SIZE: usize = 16;
const SB_FREE_BLOCKS_HI: usize = 0x158;
const SB_CHECKSUM_TYPE: usize = 0x175;
const SB_CHECKSUM_SEED: usize = 0x270;
const SB_CHECKSUM: usize = 0x3fc;

const GD_BLOCK_BITMAP_LO: usize = 0x00;
const GD_INODE_BITMAP_LO: usize = 0x04;
const GD_INODE_TABLE_LO: usize = 0x08;
const GD_FREE_BLOCKS_LO: usize = 0x0c;
const GD_FREE_INODES_LO: usize = 0x0e;
const GD_FLAGS: usize = 0x12;
const GD_BLOCK_BITMAP_CSUM_LO: usize = 0x18;
const GD_INODE_BITMAP_CSUM_LO: usize = 0x1a;
const GD_ITABLE_UNUSED_LO: usize = 0x1c;
const GD_CHECKSUM: usize = 0x1e;
const GD_BLOCK_BITMAP_HI: usize = 0x20;
const GD_INODE_BITMAP_HI: usize = 0x24;
const GD_INODE_TABLE_HI: usize = 0x28;
const GD_FREE_BLOCKS_HI: usize = 0x2c;
const GD_FREE_INODES_HI: usize = 0x2e;
const GD_ITABLE_UNUSED_HI: usize = 0x32;
const GD_BLOCK_BITMAP_CSUM_HI: usize = 0x38;
const GD_INODE_BITMAP_CSUM_HI: usize = 0x3a;
const GD_BLOCK_BITMAP_CSUM_HI_END: usize = 0x3a;
const GD_INODE_BITMAP_CSUM_HI_END: usize = 0x3c;

const INODE_MODE: usize = 0x00;
const INODE_UID: usize = 0x02;
const INODE_SIZE_LO: usize = 0x04;
const INODE_GID: usize = 0x18;
const INODE_LINKS: usize = 0x1a;
const INODE_BLOCKS_LO: usize = 0x1c;
const INODE_FLAGS: usize = 0x20;
const INODE_BLOCK: usize = 0x28;
const INODE_GENERATION: usize = 0x64;
const INODE_SIZE_HI: usize = 0x6c;
const INODE_BLOCKS_HI: usize = 0x74;
const INODE_EXTRA_ISIZE: usize = 0x80;

const EXTENT_MAGIC: u16 = 0xf30a;
const EXTENT_HEADER_SIZE: usize = 12;
const EXTENT_INLINE_MAX: u16 = 4;

const DIRENT_HEADER: usize = 8;
const DIRENT_TAIL_SIZE: usize = 12;
const DIRENT_TAIL_REC_LEN: usize = 4;
const DIRENT_TAIL_RESERVED2: usize = 6;
const DIRENT_TAIL_FILETYPE: usize = 7;
const DIRENT_TAIL_CHECKSUM: usize = 8;

#[derive(Debug)]
pub struct CompiledCreateFile {
    pub map: LoomMap,
    pub shadow: Vec<u8>,
    pub block_size: u32,
    pub parent_inode: u32,
    pub inode: u32,
    pub allocated_block: u64,
    pub allocation_group: u32,
    pub shadow_blocks: usize,
}

struct FsMetadata {
    raw_superblock: [u8; SUPERBLOCK_SIZE],
    checksum_seed: u32,
    first_data_block: u64,
    blocks_per_group: u32,
    first_inode: u32,
}

struct GroupState {
    group: u32,
    descriptor_block: u64,
    descriptor_offset: usize,
    descriptor_block_bytes: Vec<u8>,
    descriptor: Vec<u8>,
    block_bitmap_block: u64,
    inode_bitmap_block: u64,
    block_bitmap: Vec<u8>,
    inode_bitmap: Vec<u8>,
    free_blocks: u32,
    free_inodes: u32,
    itable_unused: u32,
}

/// Compiles one new single-block regular file into an existing non-indexed directory.
///
/// Stage 4 keeps all changes in the effective view. It allocates one inode and one data
/// block in the parent directory's block group, inserts the dirent into existing directory
/// slack, and shadows the corresponding allocator/accounting metadata.
///
/// # Errors
/// Returns [`Ext4Error`] for malformed or unsupported ext4 state, an existing target path,
/// insufficient directory slack, unavailable safe inode/block allocation, or mapping I/O.
pub fn compile_create_file(
    origin_path: &Path,
    target_path: &str,
    payload_path: &Path,
) -> Result<CompiledCreateFile, Ext4Error> {
    let payload = fs::read(payload_path).map_err(Ext4Error::Io)?;
    let mut image = Ext4Image::open(origin_path)?;
    image.compile_create_file(target_path, &payload)
}

impl Ext4Image {
    #[allow(clippy::too_many_lines)] // explicit one-generation ext4 metadata transaction
    pub(crate) fn compile_create_file(
        &mut self,
        target_path: &str,
        payload: &[u8],
    ) -> Result<CompiledCreateFile, Ext4Error> {
        if payload.is_empty() {
            return Err(Ext4Error::UnsupportedInodeFeature(
                "Stage 4 requires a non-empty payload",
            ));
        }
        let block_size = usize::try_from(self.superblock.block_size)
            .map_err(|_| Ext4Error::ArithmeticOverflow)?;
        if payload.len() > block_size {
            return Err(Ext4Error::UnsupportedInodeFeature(
                "Stage 4 supports exactly one data block per new file",
            ));
        }
        if !self.superblock.has_filetype {
            return Err(Ext4Error::UnsupportedFilesystemFeature(
                "Stage 4 requires filetype dirents",
            ));
        }

        let (parent_path, name) = split_parent(target_path)?;
        match self.resolve_path(target_path) {
            Ok(_) => return Err(Ext4Error::InvalidPath("target path already exists")),
            Err(Ext4Error::PathNotFound(_)) => {}
            Err(error) => return Err(error),
        }
        let parent_inode_number = if parent_path == "/" {
            ROOT_INODE
        } else {
            self.resolve_path(&parent_path)?
        };
        let parent_inode = self.read_inode(parent_inode_number)?;
        validate_parent(parent_inode_number, &parent_inode)?;

        let metadata = self.read_create_metadata()?;
        let group_number = (parent_inode_number - 1) / self.superblock.inodes_per_group;
        let mut group = self.read_create_group(group_number, &metadata)?;
        let new_inode = allocate_inode(&mut group, &metadata, &self.superblock)?;
        let new_block = allocate_data_block(&mut group, &metadata, self.superblock.blocks_count)?;

        let sectors_per_block = u64::from(self.superblock.block_size) / SECTOR_SIZE;
        let mut shadow = Vec::new();
        let mut replacements = Vec::new();

        let mut data_block = vec![0_u8; block_size];
        data_block[..payload.len()].copy_from_slice(payload);
        append_shadow_block(
            &mut shadow,
            &mut replacements,
            new_block,
            sectors_per_block,
            &data_block,
        )?;

        let (inode_table_block, inode_table_shadow) = self.build_new_inode_block(
            new_inode,
            new_block,
            payload.len(),
            sectors_per_block,
            metadata.checksum_seed,
        )?;
        append_shadow_block(
            &mut shadow,
            &mut replacements,
            inode_table_block,
            sectors_per_block,
            &inode_table_shadow,
        )?;

        let (directory_block, directory_shadow) = self.build_directory_insert(
            parent_inode_number,
            &parent_inode,
            &name,
            new_inode,
            metadata.checksum_seed,
        )?;
        append_shadow_block(
            &mut shadow,
            &mut replacements,
            directory_block,
            sectors_per_block,
            &directory_shadow,
        )?;

        rewrite_group_for_create(&mut group, &metadata, self.superblock.inodes_per_group)?;
        append_shadow_block(
            &mut shadow,
            &mut replacements,
            group.block_bitmap_block,
            sectors_per_block,
            &group.block_bitmap,
        )?;
        append_shadow_block(
            &mut shadow,
            &mut replacements,
            group.inode_bitmap_block,
            sectors_per_block,
            &group.inode_bitmap,
        )?;
        append_shadow_block(
            &mut shadow,
            &mut replacements,
            group.descriptor_block,
            sectors_per_block,
            &group.descriptor_block_bytes,
        )?;

        let (superblock_block, superblock_shadow) = self.build_superblock_for_create(&metadata)?;
        append_shadow_block(
            &mut shadow,
            &mut replacements,
            superblock_block,
            sectors_per_block,
            &superblock_shadow,
        )?;

        let map =
            LoomMap::from_replacements(SectorCount(self.image_bytes / SECTOR_SIZE), &replacements)
                .map_err(Ext4Error::Map)?;
        let shadow_blocks = shadow.len() / block_size;

        Ok(CompiledCreateFile {
            map,
            shadow,
            block_size: self.superblock.block_size,
            parent_inode: parent_inode_number,
            inode: new_inode,
            allocated_block: new_block,
            allocation_group: group.group,
            shadow_blocks,
        })
    }

    fn read_create_metadata(&mut self) -> Result<FsMetadata, Ext4Error> {
        let mut raw = [0_u8; SUPERBLOCK_SIZE];
        read_exact_at(&mut self.file, SUPERBLOCK_OFFSET, &mut raw)?;
        if read_u32(&raw, SB_FEATURE_RO_COMPAT)? & RO_COMPAT_METADATA_CSUM == 0 {
            return Err(Ext4Error::UnsupportedFilesystemFeature(
                "Stage 4 requires metadata_csum",
            ));
        }
        if read_u32(&raw, SB_CREATOR_OS)? != EXT4_OS_LINUX
            || raw[SB_CHECKSUM_TYPE] != EXT4_CRC32C_CHKSUM
        {
            return Err(Ext4Error::UnsupportedFilesystemFeature(
                "Stage 4 requires Linux CRC32C metadata checksums",
            ));
        }
        let incompat = read_u32(&raw, SB_FEATURE_INCOMPAT)?;
        let checksum_seed = if incompat & INCOMPAT_CSUM_SEED != 0 {
            read_u32(&raw, SB_CHECKSUM_SEED)?
        } else {
            let uuid = raw
                .get(SB_UUID..SB_UUID + SB_UUID_SIZE)
                .ok_or(Ext4Error::UnexpectedEndOfStructure)?;
            crc32c(u32::MAX, uuid)
        };
        let blocks_per_group = read_u32(&raw, SB_BLOCKS_PER_GROUP)?;
        if blocks_per_group == 0 || blocks_per_group % 8 != 0 {
            return Err(Ext4Error::InvalidFilesystem("invalid blocks_per_group"));
        }
        Ok(FsMetadata {
            raw_superblock: raw,
            checksum_seed,
            first_data_block: u64::from(read_u32(&raw, SB_FIRST_DATA_BLOCK)?),
            blocks_per_group,
            first_inode: read_u32(&raw, SB_FIRST_INO)?,
        })
    }

    fn read_create_group(
        &mut self,
        group: u32,
        metadata: &FsMetadata,
    ) -> Result<GroupState, Ext4Error> {
        let descriptor_size = usize::from(self.superblock.descriptor_size);
        let descriptor_start_block: u64 = if self.superblock.block_size == 1024 {
            2
        } else {
            1
        };
        let descriptor_byte_offset = u64::from(group)
            .checked_mul(u64::from(self.superblock.descriptor_size))
            .ok_or(Ext4Error::ArithmeticOverflow)?;
        let descriptor_block = descriptor_start_block
            .checked_add(descriptor_byte_offset / u64::from(self.superblock.block_size))
            .ok_or(Ext4Error::ArithmeticOverflow)?;
        let descriptor_offset =
            usize::try_from(descriptor_byte_offset % u64::from(self.superblock.block_size))
                .map_err(|_| Ext4Error::ArithmeticOverflow)?;
        let descriptor_end = descriptor_offset
            .checked_add(descriptor_size)
            .ok_or(Ext4Error::ArithmeticOverflow)?;
        let descriptor_block_bytes = self.read_block(descriptor_block)?;
        let descriptor = descriptor_block_bytes
            .get(descriptor_offset..descriptor_end)
            .ok_or(Ext4Error::UnexpectedEndOfStructure)?
            .to_vec();
        verify_group_descriptor_checksum(&descriptor, group, metadata.checksum_seed)?;

        let flags = read_u16(&descriptor, GD_FLAGS)?;
        if flags & (EXT4_BG_INODE_UNINIT | EXT4_BG_BLOCK_UNINIT) != 0 {
            return Err(Ext4Error::UnsupportedFilesystemFeature(
                "Stage 4 refuses uninitialized inode/block bitmaps",
            ));
        }
        if flags & EXT4_BG_INODE_ZEROED == 0 {
            return Err(Ext4Error::UnsupportedFilesystemFeature(
                "Stage 4 requires a zeroed inode table",
            ));
        }

        let block_bitmap_block = descriptor_block_number(
            &descriptor,
            GD_BLOCK_BITMAP_LO,
            GD_BLOCK_BITMAP_HI,
            self.superblock.has_64bit,
        )?;
        let inode_bitmap_block = descriptor_block_number(
            &descriptor,
            GD_INODE_BITMAP_LO,
            GD_INODE_BITMAP_HI,
            self.superblock.has_64bit,
        )?;
        let inode_table_block = descriptor_block_number(
            &descriptor,
            GD_INODE_TABLE_LO,
            GD_INODE_TABLE_HI,
            self.superblock.has_64bit,
        )?;
        for block in [block_bitmap_block, inode_bitmap_block, inode_table_block] {
            if block >= self.superblock.blocks_count {
                return Err(Ext4Error::InvalidFilesystem(
                    "allocator metadata block lies outside filesystem",
                ));
            }
        }

        let block_bitmap = self.read_block(block_bitmap_block)?;
        let inode_bitmap = self.read_block(inode_bitmap_block)?;
        verify_bitmap_checksum(
            &block_bitmap,
            &descriptor,
            metadata.checksum_seed,
            usize::try_from(metadata.blocks_per_group / 8)
                .map_err(|_| Ext4Error::ArithmeticOverflow)?,
            GD_BLOCK_BITMAP_CSUM_LO,
            GD_BLOCK_BITMAP_CSUM_HI,
            GD_BLOCK_BITMAP_CSUM_HI_END,
        )?;
        verify_bitmap_checksum(
            &inode_bitmap,
            &descriptor,
            metadata.checksum_seed,
            usize::try_from(self.superblock.inodes_per_group / 8)
                .map_err(|_| Ext4Error::ArithmeticOverflow)?,
            GD_INODE_BITMAP_CSUM_LO,
            GD_INODE_BITMAP_CSUM_HI,
            GD_INODE_BITMAP_CSUM_HI_END,
        )?;

        let free_blocks = descriptor_u32_count(&descriptor, GD_FREE_BLOCKS_LO, GD_FREE_BLOCKS_HI)?;
        let free_inodes = descriptor_u32_count(&descriptor, GD_FREE_INODES_LO, GD_FREE_INODES_HI)?;
        let itable_unused =
            descriptor_u32_count(&descriptor, GD_ITABLE_UNUSED_LO, GD_ITABLE_UNUSED_HI)?;

        Ok(GroupState {
            group,
            descriptor_block,
            descriptor_offset,
            descriptor_block_bytes,
            descriptor,
            block_bitmap_block,
            inode_bitmap_block,
            block_bitmap,
            inode_bitmap,
            free_blocks,
            free_inodes,
            itable_unused,
        })
    }

    fn build_new_inode_block(
        &mut self,
        inode_number: u32,
        data_block: u64,
        size: usize,
        sectors_per_block: u64,
        checksum_seed: u32,
    ) -> Result<(u64, Vec<u8>), Ext4Error> {
        let zero_based = inode_number
            .checked_sub(1)
            .ok_or(Ext4Error::InvalidInode(inode_number))?;
        let group = zero_based / self.superblock.inodes_per_group;
        let index = zero_based % self.superblock.inodes_per_group;
        let table_start = self.inode_table_block(group)?;
        let byte_offset = u64::from(index)
            .checked_mul(u64::from(self.superblock.inode_size))
            .ok_or(Ext4Error::ArithmeticOverflow)?;
        let block_size = u64::from(self.superblock.block_size);
        let table_block = table_start
            .checked_add(byte_offset / block_size)
            .ok_or(Ext4Error::ArithmeticOverflow)?;
        let inode_offset =
            usize::try_from(byte_offset % block_size).map_err(|_| Ext4Error::ArithmeticOverflow)?;
        let inode_size = usize::from(self.superblock.inode_size);
        let inode_end = inode_offset
            .checked_add(inode_size)
            .ok_or(Ext4Error::ArithmeticOverflow)?;
        let mut inode_table = self.read_block(table_block)?;
        let raw =
            inode_table
                .get_mut(inode_offset..inode_end)
                .ok_or(Ext4Error::InvalidFilesystem(
                    "new inode record crosses inode-table block",
                ))?;
        raw.fill(0);
        write_u16(raw, INODE_MODE, MODE_REGULAR | 0o644)?;
        write_u16(raw, INODE_UID, 0)?;
        write_u16(raw, INODE_GID, 0)?;
        write_u16(raw, INODE_LINKS, 1)?;
        write_u32(
            raw,
            INODE_SIZE_LO,
            u32::try_from(size).map_err(|_| Ext4Error::ArithmeticOverflow)?,
        )?;
        write_u32(raw, INODE_SIZE_HI, 0)?;
        write_u32(
            raw,
            INODE_BLOCKS_LO,
            u32::try_from(sectors_per_block).map_err(|_| Ext4Error::ArithmeticOverflow)?,
        )?;
        write_u16(raw, INODE_BLOCKS_HI, 0)?;
        write_u32(raw, INODE_FLAGS, INODE_EXTENTS_FL)?;
        write_u32(raw, INODE_GENERATION, 1)?;
        if inode_size >= 160 {
            write_u16(raw, INODE_EXTRA_ISIZE, 32)?;
        }
        initialize_extent_root(raw, data_block)?;
        rewrite_inode_checksum(raw, checksum_seed, inode_number).map_err(Ext4Error::Checksum)?;
        Ok((table_block, inode_table))
    }

    fn build_directory_insert(
        &mut self,
        parent_inode_number: u32,
        parent_inode: &Inode,
        name: &str,
        new_inode: u32,
        checksum_seed: u32,
    ) -> Result<(u64, Vec<u8>), Ext4Error> {
        let parent_raw = self.read_raw_inode(parent_inode_number)?;
        let generation = read_u32(&parent_raw, INODE_GENERATION)?;
        let dir_seed = inode_seed(checksum_seed, parent_inode_number, generation);
        let needed = dir_rec_len(name.len())?;

        for physical_block in self.file_blocks(parent_inode)? {
            let mut block = self.read_block(physical_block)?;
            verify_dirblock_checksum(&block, dir_seed)?;
            if insert_dirent(&mut block, name.as_bytes(), new_inode, needed)? {
                rewrite_dirblock_checksum(&mut block, dir_seed)?;
                return Ok((physical_block, block));
            }
        }
        Err(Ext4Error::UnsupportedInodeFeature(
            "Stage 4 parent directory has no existing dirent slack",
        ))
    }

    fn read_raw_inode(&mut self, inode_number: u32) -> Result<Vec<u8>, Ext4Error> {
        if inode_number == 0 || inode_number > self.superblock.inodes_count {
            return Err(Ext4Error::InvalidInode(inode_number));
        }
        let zero_based = inode_number - 1;
        let group = zero_based / self.superblock.inodes_per_group;
        let index = zero_based % self.superblock.inodes_per_group;
        let table = self.inode_table_block(group)?;
        let byte_offset = u64::from(index)
            .checked_mul(u64::from(self.superblock.inode_size))
            .ok_or(Ext4Error::ArithmeticOverflow)?;
        let absolute = table
            .checked_mul(u64::from(self.superblock.block_size))
            .and_then(|base| base.checked_add(byte_offset))
            .ok_or(Ext4Error::ArithmeticOverflow)?;
        let mut raw = vec![0_u8; usize::from(self.superblock.inode_size)];
        read_exact_at(&mut self.file, absolute, &mut raw)?;
        Ok(raw)
    }

    fn build_superblock_for_create(
        &mut self,
        metadata: &FsMetadata,
    ) -> Result<(u64, Vec<u8>), Ext4Error> {
        let mut raw = metadata.raw_superblock;
        let free_blocks = superblock_free_blocks(&raw, self.superblock.has_64bit)?
            .checked_sub(1)
            .ok_or(Ext4Error::InvalidFilesystem(
                "superblock free blocks is zero",
            ))?;
        let free_inodes =
            read_u32(&raw, SB_FREE_INODES)?
                .checked_sub(1)
                .ok_or(Ext4Error::InvalidFilesystem(
                    "superblock free inodes is zero",
                ))?;
        write_u32(&mut raw, SB_FREE_BLOCKS_LO, low_u32(free_blocks))?;
        if self.superblock.has_64bit {
            write_u32(
                &mut raw,
                SB_FREE_BLOCKS_HI,
                u32::try_from(free_blocks >> 32).map_err(|_| Ext4Error::ArithmeticOverflow)?,
            )?;
        }
        write_u32(&mut raw, SB_FREE_INODES, free_inodes)?;
        write_u32(&mut raw, SB_CHECKSUM, 0)?;
        let checksum = crc32c(u32::MAX, &raw[..SB_CHECKSUM]);
        write_u32(&mut raw, SB_CHECKSUM, checksum)?;

        let block_size = u64::from(self.superblock.block_size);
        let fs_block = SUPERBLOCK_OFFSET / block_size;
        let offset = usize::try_from(SUPERBLOCK_OFFSET % block_size)
            .map_err(|_| Ext4Error::ArithmeticOverflow)?;
        let mut block = self.read_block(fs_block)?;
        let end = offset
            .checked_add(SUPERBLOCK_SIZE)
            .ok_or(Ext4Error::ArithmeticOverflow)?;
        block
            .get_mut(offset..end)
            .ok_or(Ext4Error::UnexpectedEndOfStructure)?
            .copy_from_slice(&raw);
        Ok((fs_block, block))
    }
}

fn split_parent(path: &str) -> Result<(String, String), Ext4Error> {
    let components = parse_absolute_path(path)?;
    let name = components
        .last()
        .copied()
        .ok_or(Ext4Error::InvalidPath("missing filename"))?
        .to_string();
    let parent = if components.len() == 1 {
        "/".to_string()
    } else {
        format!("/{}", components[..components.len() - 1].join("/"))
    };
    Ok((parent, name))
}

fn validate_parent(inode_number: u32, inode: &Inode) -> Result<(), Ext4Error> {
    if inode.file_type() != MODE_DIRECTORY {
        return Err(Ext4Error::NotDirectory(inode_number));
    }
    if inode.flags & EXT4_INDEX_FL != 0 {
        return Err(Ext4Error::UnsupportedInodeFeature(
            "Stage 4 refuses indexed directories",
        ));
    }
    if inode.flags & INODE_INLINE_DATA_FL != 0 {
        return Err(Ext4Error::UnsupportedInodeFeature("inline directory data"));
    }
    if inode.flags & INODE_EXTENTS_FL == 0 {
        return Err(Ext4Error::UnsupportedInodeFeature(
            "legacy directory block mapping",
        ));
    }
    Ok(())
}

fn descriptor_block_number(
    descriptor: &[u8],
    lo_offset: usize,
    hi_offset: usize,
    has_64bit: bool,
) -> Result<u64, Ext4Error> {
    let lo = u64::from(read_u32(descriptor, lo_offset)?);
    let hi = if has_64bit {
        u64::from(read_u32(descriptor, hi_offset)?)
    } else {
        0
    };
    Ok((hi << 32) | lo)
}

fn descriptor_u32_count(
    descriptor: &[u8],
    lo_offset: usize,
    hi_offset: usize,
) -> Result<u32, Ext4Error> {
    let lo = u32::from(read_u16(descriptor, lo_offset)?);
    let hi = if descriptor.len() >= 64 {
        u32::from(read_u16(descriptor, hi_offset)?)
    } else {
        0
    };
    Ok((hi << 16) | lo)
}

fn allocate_inode(
    group: &mut GroupState,
    metadata: &FsMetadata,
    superblock: &super::Superblock,
) -> Result<u32, Ext4Error> {
    if group.free_inodes == 0 {
        return Err(Ext4Error::InvalidFilesystem(
            "parent group has no free inode",
        ));
    }
    let group_base = group
        .group
        .checked_mul(superblock.inodes_per_group)
        .ok_or(Ext4Error::ArithmeticOverflow)?;
    for bit in 0..superblock.inodes_per_group {
        let inode_number = group_base
            .checked_add(bit)
            .and_then(|value| value.checked_add(1))
            .ok_or(Ext4Error::ArithmeticOverflow)?;
        if inode_number < metadata.first_inode || inode_number > superblock.inodes_count {
            continue;
        }
        if !bitmap_is_set(&group.inode_bitmap, bit)? {
            bitmap_set(&mut group.inode_bitmap, bit)?;
            let initialized = bit.checked_add(1).ok_or(Ext4Error::ArithmeticOverflow)?;
            let new_unused = superblock.inodes_per_group.saturating_sub(initialized);
            group.itable_unused = group.itable_unused.min(new_unused);
            return Ok(inode_number);
        }
    }
    Err(Ext4Error::InvalidFilesystem(
        "parent group inode bitmap has no free inode",
    ))
}

fn allocate_data_block(
    group: &mut GroupState,
    metadata: &FsMetadata,
    blocks_count: u64,
) -> Result<u64, Ext4Error> {
    if group.free_blocks == 0 {
        return Err(Ext4Error::InvalidFilesystem(
            "parent group has no free block",
        ));
    }
    let group_first = metadata
        .first_data_block
        .checked_add(
            u64::from(group.group)
                .checked_mul(u64::from(metadata.blocks_per_group))
                .ok_or(Ext4Error::ArithmeticOverflow)?,
        )
        .ok_or(Ext4Error::ArithmeticOverflow)?;
    let valid = blocks_count
        .saturating_sub(group_first)
        .min(u64::from(metadata.blocks_per_group));
    let valid_u32 = u32::try_from(valid).map_err(|_| Ext4Error::ArithmeticOverflow)?;
    for bit in 0..valid_u32 {
        if !bitmap_is_set(&group.block_bitmap, bit)? {
            bitmap_set(&mut group.block_bitmap, bit)?;
            return group_first
                .checked_add(u64::from(bit))
                .ok_or(Ext4Error::ArithmeticOverflow);
        }
    }
    Err(Ext4Error::InvalidFilesystem(
        "parent group block bitmap has no free block",
    ))
}

fn rewrite_group_for_create(
    group: &mut GroupState,
    metadata: &FsMetadata,
    inodes_per_group: u32,
) -> Result<(), Ext4Error> {
    group.free_blocks = group
        .free_blocks
        .checked_sub(1)
        .ok_or(Ext4Error::InvalidFilesystem("group free blocks is zero"))?;
    group.free_inodes = group
        .free_inodes
        .checked_sub(1)
        .ok_or(Ext4Error::InvalidFilesystem("group free inodes is zero"))?;
    write_descriptor_count(
        &mut group.descriptor,
        GD_FREE_BLOCKS_LO,
        GD_FREE_BLOCKS_HI,
        group.free_blocks,
    )?;
    write_descriptor_count(
        &mut group.descriptor,
        GD_FREE_INODES_LO,
        GD_FREE_INODES_HI,
        group.free_inodes,
    )?;
    write_descriptor_count(
        &mut group.descriptor,
        GD_ITABLE_UNUSED_LO,
        GD_ITABLE_UNUSED_HI,
        group.itable_unused,
    )?;

    rewrite_bitmap_checksum(
        &group.block_bitmap,
        &mut group.descriptor,
        metadata.checksum_seed,
        usize::try_from(metadata.blocks_per_group / 8)
            .map_err(|_| Ext4Error::ArithmeticOverflow)?,
        GD_BLOCK_BITMAP_CSUM_LO,
        GD_BLOCK_BITMAP_CSUM_HI,
        GD_BLOCK_BITMAP_CSUM_HI_END,
    )?;
    rewrite_bitmap_checksum(
        &group.inode_bitmap,
        &mut group.descriptor,
        metadata.checksum_seed,
        usize::try_from(inodes_per_group / 8).map_err(|_| Ext4Error::ArithmeticOverflow)?,
        GD_INODE_BITMAP_CSUM_LO,
        GD_INODE_BITMAP_CSUM_HI,
        GD_INODE_BITMAP_CSUM_HI_END,
    )?;
    write_u16(&mut group.descriptor, GD_CHECKSUM, 0)?;
    let mut checksum = crc32c(metadata.checksum_seed, &group.group.to_le_bytes());
    checksum = crc32c(checksum, &group.descriptor);
    write_u16(
        &mut group.descriptor,
        GD_CHECKSUM,
        low_u16(u64::from(checksum)),
    )?;

    let end = group
        .descriptor_offset
        .checked_add(group.descriptor.len())
        .ok_or(Ext4Error::ArithmeticOverflow)?;
    group
        .descriptor_block_bytes
        .get_mut(group.descriptor_offset..end)
        .ok_or(Ext4Error::UnexpectedEndOfStructure)?
        .copy_from_slice(&group.descriptor);
    Ok(())
}

fn verify_group_descriptor_checksum(
    descriptor: &[u8],
    group: u32,
    seed: u32,
) -> Result<(), Ext4Error> {
    let provided = read_u16(descriptor, GD_CHECKSUM)?;
    let mut copy = descriptor.to_vec();
    write_u16(&mut copy, GD_CHECKSUM, 0)?;
    let mut checksum = crc32c(seed, &group.to_le_bytes());
    checksum = crc32c(checksum, &copy);
    if provided != low_u16(u64::from(checksum)) {
        return Err(Ext4Error::InvalidFilesystem(
            "group descriptor checksum mismatch",
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn verify_bitmap_checksum(
    bitmap: &[u8],
    descriptor: &[u8],
    seed: u32,
    bytes: usize,
    lo_offset: usize,
    hi_offset: usize,
    hi_end: usize,
) -> Result<(), Ext4Error> {
    let region = bitmap
        .get(..bytes)
        .ok_or(Ext4Error::UnexpectedEndOfStructure)?;
    let calculated = crc32c(seed, region);
    let mut provided = u32::from(read_u16(descriptor, lo_offset)?);
    if descriptor.len() >= hi_end {
        provided |= u32::from(read_u16(descriptor, hi_offset)?) << 16;
    } else {
        if provided == (calculated & 0xffff) {
            return Ok(());
        }
        return Err(Ext4Error::InvalidFilesystem("bitmap checksum mismatch"));
    }
    if provided != calculated {
        return Err(Ext4Error::InvalidFilesystem("bitmap checksum mismatch"));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn rewrite_bitmap_checksum(
    bitmap: &[u8],
    descriptor: &mut [u8],
    seed: u32,
    bytes: usize,
    lo_offset: usize,
    hi_offset: usize,
    hi_end: usize,
) -> Result<(), Ext4Error> {
    let region = bitmap
        .get(..bytes)
        .ok_or(Ext4Error::UnexpectedEndOfStructure)?;
    let checksum = crc32c(seed, region);
    write_u16(descriptor, lo_offset, low_u16(u64::from(checksum)))?;
    if descriptor.len() >= hi_end {
        write_u16(
            descriptor,
            hi_offset,
            u16::try_from(checksum >> 16).map_err(|_| Ext4Error::ArithmeticOverflow)?,
        )?;
    }
    Ok(())
}

fn verify_dirblock_checksum(block: &[u8], seed: u32) -> Result<(), Ext4Error> {
    if block.len() < DIRENT_TAIL_SIZE {
        return Err(Ext4Error::InvalidFilesystem("directory block is too small"));
    }
    let tail = block.len() - DIRENT_TAIL_SIZE;
    if read_u32(block, tail)? != 0
        || read_u16(block, tail + DIRENT_TAIL_REC_LEN)?
            != u16::try_from(DIRENT_TAIL_SIZE).map_err(|_| Ext4Error::ArithmeticOverflow)?
        || block[tail + DIRENT_TAIL_RESERVED2] != 0
        || block[tail + DIRENT_TAIL_FILETYPE] != EXT4_FT_DIR_CSUM
    {
        return Err(Ext4Error::UnsupportedInodeFeature(
            "directory block has no standard checksum tail",
        ));
    }
    let provided = read_u32(block, tail + DIRENT_TAIL_CHECKSUM)?;
    let calculated = crc32c(seed, &block[..tail]);
    if provided != calculated {
        return Err(Ext4Error::InvalidFilesystem(
            "directory block checksum mismatch",
        ));
    }
    Ok(())
}

fn rewrite_dirblock_checksum(block: &mut [u8], seed: u32) -> Result<(), Ext4Error> {
    let tail = block
        .len()
        .checked_sub(DIRENT_TAIL_SIZE)
        .ok_or(Ext4Error::UnexpectedEndOfStructure)?;
    let checksum = crc32c(seed, &block[..tail]);
    write_u32(block, tail + DIRENT_TAIL_CHECKSUM, checksum)
}

fn insert_dirent(
    block: &mut [u8],
    name: &[u8],
    inode: u32,
    needed: usize,
) -> Result<bool, Ext4Error> {
    let limit = block
        .len()
        .checked_sub(DIRENT_TAIL_SIZE)
        .ok_or(Ext4Error::UnexpectedEndOfStructure)?;
    let mut offset = 0_usize;
    while offset < limit {
        let remaining = limit - offset;
        if remaining < DIRENT_HEADER {
            return Err(Ext4Error::InvalidFilesystem("corrupt directory entry"));
        }
        let current_inode = read_u32(block, offset)?;
        let rec_len = usize::from(read_u16(block, offset + 4)?);
        if rec_len < DIRENT_HEADER || rec_len % 4 != 0 || rec_len > remaining {
            return Err(Ext4Error::InvalidFilesystem("corrupt directory rec_len"));
        }
        let name_len = usize::from(block[offset + 6]);
        if name_len > rec_len.saturating_sub(DIRENT_HEADER) {
            return Err(Ext4Error::InvalidFilesystem("corrupt directory name_len"));
        }

        if current_inode == 0 && rec_len >= needed {
            write_dirent(block, offset, rec_len, inode, name)?;
            return Ok(true);
        }
        if current_inode != 0 {
            let minimal = dir_rec_len(name_len)?;
            let slack = rec_len.saturating_sub(minimal);
            if slack >= needed {
                write_u16(
                    block,
                    offset + 4,
                    u16::try_from(minimal).map_err(|_| Ext4Error::ArithmeticOverflow)?,
                )?;
                let new_offset = offset
                    .checked_add(minimal)
                    .ok_or(Ext4Error::ArithmeticOverflow)?;
                write_dirent(block, new_offset, slack, inode, name)?;
                return Ok(true);
            }
        }
        offset = offset
            .checked_add(rec_len)
            .ok_or(Ext4Error::ArithmeticOverflow)?;
    }
    Ok(false)
}

fn write_dirent(
    block: &mut [u8],
    offset: usize,
    rec_len: usize,
    inode: u32,
    name: &[u8],
) -> Result<(), Ext4Error> {
    let end = offset
        .checked_add(rec_len)
        .ok_or(Ext4Error::ArithmeticOverflow)?;
    let entry = block
        .get_mut(offset..end)
        .ok_or(Ext4Error::UnexpectedEndOfStructure)?;
    entry.fill(0);
    write_u32(entry, 0, inode)?;
    write_u16(
        entry,
        4,
        u16::try_from(rec_len).map_err(|_| Ext4Error::ArithmeticOverflow)?,
    )?;
    entry[6] = u8::try_from(name.len()).map_err(|_| Ext4Error::ArithmeticOverflow)?;
    entry[7] = EXT4_FT_REG_FILE;
    entry
        .get_mut(DIRENT_HEADER..DIRENT_HEADER + name.len())
        .ok_or(Ext4Error::UnexpectedEndOfStructure)?
        .copy_from_slice(name);
    Ok(())
}

fn dir_rec_len(name_len: usize) -> Result<usize, Ext4Error> {
    DIRENT_HEADER
        .checked_add(name_len)
        .and_then(|value| value.checked_add(3))
        .map(|value| value & !3)
        .ok_or(Ext4Error::ArithmeticOverflow)
}

fn initialize_extent_root(raw_inode: &mut [u8], data_block: u64) -> Result<(), Ext4Error> {
    let root = raw_inode
        .get_mut(INODE_BLOCK..INODE_BLOCK + 60)
        .ok_or(Ext4Error::UnexpectedEndOfStructure)?;
    root.fill(0);
    write_u16(root, 0, EXTENT_MAGIC)?;
    write_u16(root, 2, 1)?;
    write_u16(root, 4, EXTENT_INLINE_MAX)?;
    write_u16(root, 6, 0)?;
    write_u32(root, 8, 0)?;
    write_u32(root, EXTENT_HEADER_SIZE, 0)?;
    write_u16(root, EXTENT_HEADER_SIZE + 4, 1)?;
    write_u16(
        root,
        EXTENT_HEADER_SIZE + 6,
        u16::try_from(data_block >> 32).map_err(|_| Ext4Error::ArithmeticOverflow)?,
    )?;
    write_u32(root, EXTENT_HEADER_SIZE + 8, low_u32(data_block))?;
    Ok(())
}

fn bitmap_is_set(bitmap: &[u8], bit: u32) -> Result<bool, Ext4Error> {
    let byte = usize::try_from(bit / 8).map_err(|_| Ext4Error::ArithmeticOverflow)?;
    let shift = bit % 8;
    let value = *bitmap
        .get(byte)
        .ok_or(Ext4Error::UnexpectedEndOfStructure)?;
    Ok(value & (1_u8 << shift) != 0)
}

fn bitmap_set(bitmap: &mut [u8], bit: u32) -> Result<(), Ext4Error> {
    let byte = usize::try_from(bit / 8).map_err(|_| Ext4Error::ArithmeticOverflow)?;
    let shift = bit % 8;
    let value = bitmap
        .get_mut(byte)
        .ok_or(Ext4Error::UnexpectedEndOfStructure)?;
    *value |= 1_u8 << shift;
    Ok(())
}

fn write_descriptor_count(
    descriptor: &mut [u8],
    lo_offset: usize,
    hi_offset: usize,
    value: u32,
) -> Result<(), Ext4Error> {
    write_u16(descriptor, lo_offset, low_u16(u64::from(value)))?;
    if descriptor.len() >= 64 {
        write_u16(
            descriptor,
            hi_offset,
            u16::try_from(value >> 16).map_err(|_| Ext4Error::ArithmeticOverflow)?,
        )?;
    } else if value > u32::from(u16::MAX) {
        return Err(Ext4Error::InvalidFilesystem(
            "32-byte descriptor count overflow",
        ));
    }
    Ok(())
}

fn superblock_free_blocks(raw: &[u8], has_64bit: bool) -> Result<u64, Ext4Error> {
    let lo = u64::from(read_u32(raw, SB_FREE_BLOCKS_LO)?);
    let hi = if has_64bit {
        u64::from(read_u32(raw, SB_FREE_BLOCKS_HI)?)
    } else {
        0
    };
    Ok((hi << 32) | lo)
}

fn append_shadow_block(
    shadow: &mut Vec<u8>,
    replacements: &mut Vec<ReplacementExtent>,
    physical_block: u64,
    sectors_per_block: u64,
    block: &[u8],
) -> Result<(), Ext4Error> {
    let expected = usize::try_from(
        sectors_per_block
            .checked_mul(SECTOR_SIZE)
            .ok_or(Ext4Error::ArithmeticOverflow)?,
    )
    .map_err(|_| Ext4Error::ArithmeticOverflow)?;
    if block.len() != expected {
        return Err(Ext4Error::InvalidFilesystem("shadow block size mismatch"));
    }
    let shadow_bytes = u64::try_from(shadow.len()).map_err(|_| Ext4Error::ArithmeticOverflow)?;
    let logical = physical_block
        .checked_mul(sectors_per_block)
        .ok_or(Ext4Error::ArithmeticOverflow)?;
    replacements.push(ReplacementExtent {
        logical_start: Sector(logical),
        sector_count: SectorCount(sectors_per_block),
        shadow_start: Sector(shadow_bytes / SECTOR_SIZE),
    });
    shadow.extend_from_slice(block);
    Ok(())
}

fn low_u16(value: u64) -> u16 {
    let bytes = value.to_le_bytes();
    u16::from_le_bytes([bytes[0], bytes[1]])
}

fn low_u32(value: u64) -> u32 {
    let bytes = value.to_le_bytes();
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn write_u16(bytes: &mut [u8], offset: usize, value: u16) -> Result<(), Ext4Error> {
    let end = offset.checked_add(2).ok_or(Ext4Error::ArithmeticOverflow)?;
    bytes
        .get_mut(offset..end)
        .ok_or(Ext4Error::UnexpectedEndOfStructure)?
        .copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn write_u32(bytes: &mut [u8], offset: usize, value: u32) -> Result<(), Ext4Error> {
    let end = offset.checked_add(4).ok_or(Ext4Error::ArithmeticOverflow)?;
    bytes
        .get_mut(offset..end)
        .ok_or(Ext4Error::UnexpectedEndOfStructure)?
        .copy_from_slice(&value.to_le_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_record_length_is_four_byte_aligned() {
        assert_eq!(dir_rec_len(1).unwrap(), 12);
        assert_eq!(dir_rec_len(5).unwrap(), 16);
    }

    #[test]
    fn bitmap_allocation_uses_lsb_first_numbering() {
        let mut bitmap = [0b1111_0111_u8];
        assert!(!bitmap_is_set(&bitmap, 3).unwrap());
        bitmap_set(&mut bitmap, 3).unwrap();
        assert_eq!(bitmap[0], 0xff);
    }
}

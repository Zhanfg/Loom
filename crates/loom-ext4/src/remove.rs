#![forbid(unsafe_code)]

use super::checksum::{crc32c, inode_seed};
use super::{
    parse_absolute_path, read_exact_at, read_u16, read_u32, Ext4Error, Ext4Image, Inode,
    INODE_EXTENTS_FL, INODE_INLINE_DATA_FL, INODE_VERITY_FL, MODE_DIRECTORY, MODE_REGULAR,
    ROOT_INODE, SECTOR_SIZE, SUPERBLOCK_OFFSET, SUPERBLOCK_SIZE,
};
use loom_map::{LoomMap, ReplacementExtent};
use loom_types::{Sector, SectorCount};
use std::path::Path;

const RO_COMPAT_METADATA_CSUM: u32 = 0x0400;
const INCOMPAT_CSUM_SEED: u32 = 0x2000;
const EXT4_OS_LINUX: u32 = 0;
const EXT4_CRC32C_CHKSUM: u8 = 1;
const EXT4_BG_INODE_UNINIT: u16 = 0x0001;
const EXT4_BG_BLOCK_UNINIT: u16 = 0x0002;
const EXT4_INDEX_FL: u32 = 0x0000_1000;

const SB_FREE_BLOCKS_LO: usize = 0x0c;
const SB_FREE_INODES: usize = 0x10;
const SB_FIRST_DATA_BLOCK: usize = 0x14;
const SB_BLOCKS_PER_GROUP: usize = 0x20;
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
const GD_CHECKSUM: usize = 0x1e;
const GD_BLOCK_BITMAP_HI: usize = 0x20;
const GD_INODE_BITMAP_HI: usize = 0x24;
const GD_INODE_TABLE_HI: usize = 0x28;
const GD_FREE_BLOCKS_HI: usize = 0x2c;
const GD_FREE_INODES_HI: usize = 0x2e;
const GD_BLOCK_BITMAP_CSUM_HI: usize = 0x38;
const GD_INODE_BITMAP_CSUM_HI: usize = 0x3a;
const GD_BLOCK_BITMAP_CSUM_HI_END: usize = 0x3a;
const GD_INODE_BITMAP_CSUM_HI_END: usize = 0x3c;

const INODE_GENERATION: usize = 0x64;
const INODE_FILE_ACL_LO: usize = 0x68;
const INODE_FILE_ACL_HI: usize = 0x76;

const DIRENT_HEADER: usize = 8;
const DIRENT_TAIL_SIZE: usize = 12;
const DIRENT_TAIL_REC_LEN: usize = 4;
const DIRENT_TAIL_RESERVED2: usize = 6;
const DIRENT_TAIL_FILETYPE: usize = 7;
const DIRENT_TAIL_CHECKSUM: usize = 8;
const EXT4_FT_DIR_CSUM: u8 = 0xde;

#[derive(Debug)]
pub struct CompiledRemoveFile {
    pub map: LoomMap,
    pub shadow: Vec<u8>,
    pub block_size: u32,
    pub parent_inode: u32,
    pub inode: u32,
    pub freed_block: u64,
    pub allocation_group: u32,
    pub shadow_blocks: usize,
}

struct FsMetadata {
    raw_superblock: [u8; SUPERBLOCK_SIZE],
    checksum_seed: u32,
    first_data_block: u64,
    blocks_per_group: u32,
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
}

/// Removes one one-block, single-link regular file from the effective ext4 view.
///
/// This compiles the final clean post-eviction state: the parent dirent is removed,
/// the file inode and its one data block are released in the effective bitmaps, and
/// group/superblock accounting plus checksums are updated. The origin is never written.
///
/// # Errors
/// Returns [`Ext4Error`] when the target does not fit the deliberately narrow Stage 5
/// model, when allocator/checksum state is malformed, or when origin I/O fails.
pub fn compile_remove_file(
    origin_path: &Path,
    target_path: &str,
) -> Result<CompiledRemoveFile, Ext4Error> {
    let mut image = Ext4Image::open(origin_path)?;
    image.compile_remove_file(target_path)
}

impl Ext4Image {
    #[allow(clippy::too_many_lines)] // one explicit immutable ext4 metadata transaction
    fn compile_remove_file(
        &mut self,
        target_path: &str,
    ) -> Result<CompiledRemoveFile, Ext4Error> {
        let (parent_path, name) = split_parent(target_path)?;
        let inode_number = self.resolve_path(target_path)?;
        let target_inode = self.read_inode(inode_number)?;
        validate_target(inode_number, &target_inode)?;
        let raw_target_inode = self.read_raw_inode_for_remove(inode_number)?;
        reject_external_xattr_block(&raw_target_inode, self.superblock.has_64bit)?;

        let target_blocks = self.file_blocks(&target_inode)?;
        if target_blocks.len() != 1 || target_inode.size == 0 {
            return Err(Ext4Error::UnsupportedInodeFeature(
                "Stage 5 requires a non-empty regular file with exactly one data block",
            ));
        }
        let freed_block = target_blocks[0];

        let parent_inode_number = if parent_path == "/" {
            ROOT_INODE
        } else {
            self.resolve_path(&parent_path)?
        };
        let parent_inode = self.read_inode(parent_inode_number)?;
        validate_parent(parent_inode_number, &parent_inode)?;

        let metadata = self.read_remove_metadata()?;
        let inode_group = (inode_number - 1) / self.superblock.inodes_per_group;
        let data_group = block_group_for(
            freed_block,
            metadata.first_data_block,
            metadata.blocks_per_group,
        )?;
        if inode_group != data_group {
            return Err(Ext4Error::UnsupportedInodeFeature(
                "Stage 5 requires inode and data block in the same block group",
            ));
        }

        let mut group = self.read_remove_group(inode_group, &metadata)?;
        let inode_bit = (inode_number - 1) % self.superblock.inodes_per_group;
        let block_bit = block_bit_in_group(
            freed_block,
            inode_group,
            metadata.first_data_block,
            metadata.blocks_per_group,
        )?;
        require_bitmap_set(&group.inode_bitmap, inode_bit, "target inode bit is already free")?;
        require_bitmap_set(&group.block_bitmap, block_bit, "target data block is already free")?;
        bitmap_clear(&mut group.inode_bitmap, inode_bit)?;
        bitmap_clear(&mut group.block_bitmap, block_bit)?;

        let sectors_per_block = u64::from(self.superblock.block_size) / SECTOR_SIZE;
        let mut shadow = Vec::new();
        let mut replacements = Vec::new();

        let (dir_block, dir_shadow) = self.build_directory_remove(
            parent_inode_number,
            &parent_inode,
            &name,
            inode_number,
            metadata.checksum_seed,
        )?;
        append_shadow_block(
            &mut shadow,
            &mut replacements,
            dir_block,
            sectors_per_block,
            &dir_shadow,
        )?;

        let (inode_table_block, inode_table_shadow) =
            self.build_cleared_inode_block(inode_number)?;
        append_shadow_block(
            &mut shadow,
            &mut replacements,
            inode_table_block,
            sectors_per_block,
            &inode_table_shadow,
        )?;

        rewrite_group_for_remove(&mut group, &metadata, self.superblock.inodes_per_group)?;
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

        let (superblock_block, superblock_shadow) = self.build_superblock_for_remove(&metadata)?;
        append_shadow_block(
            &mut shadow,
            &mut replacements,
            superblock_block,
            sectors_per_block,
            &superblock_shadow,
        )?;

        let map = LoomMap::from_replacements(
            SectorCount(self.image_bytes / SECTOR_SIZE),
            &replacements,
        )
        .map_err(Ext4Error::Map)?;
        let block_size = usize::try_from(self.superblock.block_size)
            .map_err(|_| Ext4Error::ArithmeticOverflow)?;
        let shadow_blocks = shadow.len() / block_size;

        Ok(CompiledRemoveFile {
            map,
            shadow,
            block_size: self.superblock.block_size,
            parent_inode: parent_inode_number,
            inode: inode_number,
            freed_block,
            allocation_group: inode_group,
            shadow_blocks,
        })
    }

    fn read_remove_metadata(&mut self) -> Result<FsMetadata, Ext4Error> {
        let mut raw = [0_u8; SUPERBLOCK_SIZE];
        read_exact_at(&mut self.file, SUPERBLOCK_OFFSET, &mut raw)?;
        if read_u32(&raw, SB_FEATURE_RO_COMPAT)? & RO_COMPAT_METADATA_CSUM == 0 {
            return Err(Ext4Error::UnsupportedFilesystemFeature(
                "Stage 5 requires metadata_csum",
            ));
        }
        if read_u32(&raw, SB_CREATOR_OS)? != EXT4_OS_LINUX
            || raw[SB_CHECKSUM_TYPE] != EXT4_CRC32C_CHKSUM
        {
            return Err(Ext4Error::UnsupportedFilesystemFeature(
                "Stage 5 requires Linux CRC32C metadata checksums",
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
        })
    }

    fn read_remove_group(
        &mut self,
        group: u32,
        metadata: &FsMetadata,
    ) -> Result<GroupState, Ext4Error> {
        let descriptor_size = usize::from(self.superblock.descriptor_size);
        let descriptor_start_block: u64 = if self.superblock.block_size == 1024 { 2 } else { 1 };
        let descriptor_byte_offset = u64::from(group)
            .checked_mul(u64::from(self.superblock.descriptor_size))
            .ok_or(Ext4Error::ArithmeticOverflow)?;
        let descriptor_block = descriptor_start_block
            .checked_add(descriptor_byte_offset / u64::from(self.superblock.block_size))
            .ok_or(Ext4Error::ArithmeticOverflow)?;
        let descriptor_offset = usize::try_from(
            descriptor_byte_offset % u64::from(self.superblock.block_size),
        )
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
                "Stage 5 refuses uninitialized inode/block bitmaps",
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

        let free_blocks = descriptor_u32_count(
            &descriptor,
            GD_FREE_BLOCKS_LO,
            GD_FREE_BLOCKS_HI,
        )?;
        let free_inodes = descriptor_u32_count(
            &descriptor,
            GD_FREE_INODES_LO,
            GD_FREE_INODES_HI,
        )?;
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
        })
    }

    fn read_raw_inode_for_remove(&mut self, inode_number: u32) -> Result<Vec<u8>, Ext4Error> {
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

    fn build_cleared_inode_block(
        &mut self,
        inode_number: u32,
    ) -> Result<(u64, Vec<u8>), Ext4Error> {
        let zero_based = inode_number
            .checked_sub(1)
            .ok_or(Ext4Error::InvalidInode(inode_number))?;
        let group = zero_based / self.superblock.inodes_per_group;
        let index = zero_based % self.superblock.inodes_per_group;
        let table = self.inode_table_block(group)?;
        let byte_offset = u64::from(index)
            .checked_mul(u64::from(self.superblock.inode_size))
            .ok_or(Ext4Error::ArithmeticOverflow)?;
        let block_size = u64::from(self.superblock.block_size);
        let table_block = table
            .checked_add(byte_offset / block_size)
            .ok_or(Ext4Error::ArithmeticOverflow)?;
        let inode_offset = usize::try_from(byte_offset % block_size)
            .map_err(|_| Ext4Error::ArithmeticOverflow)?;
        let inode_size = usize::from(self.superblock.inode_size);
        let end = inode_offset
            .checked_add(inode_size)
            .ok_or(Ext4Error::ArithmeticOverflow)?;
        let mut table_shadow = self.read_block(table_block)?;
        table_shadow
            .get_mut(inode_offset..end)
            .ok_or(Ext4Error::InvalidFilesystem(
                "target inode crosses inode-table block",
            ))?
            .fill(0);
        Ok((table_block, table_shadow))
    }

    fn build_directory_remove(
        &mut self,
        parent_inode_number: u32,
        parent_inode: &Inode,
        name: &str,
        target_inode: u32,
        checksum_seed: u32,
    ) -> Result<(u64, Vec<u8>), Ext4Error> {
        let parent_raw = self.read_raw_inode_for_remove(parent_inode_number)?;
        let generation = read_u32(&parent_raw, INODE_GENERATION)?;
        let dir_seed = inode_seed(checksum_seed, parent_inode_number, generation);
        for physical_block in self.file_blocks(parent_inode)? {
            let mut block = self.read_block(physical_block)?;
            verify_dirblock_checksum(&block, dir_seed)?;
            if delete_dirent(&mut block, name.as_bytes(), target_inode)? {
                rewrite_dirblock_checksum(&mut block, dir_seed)?;
                return Ok((physical_block, block));
            }
        }
        Err(Ext4Error::PathNotFound(name.to_string()))
    }

    fn build_superblock_for_remove(
        &mut self,
        metadata: &FsMetadata,
    ) -> Result<(u64, Vec<u8>), Ext4Error> {
        let mut raw = metadata.raw_superblock;
        let free_blocks = superblock_free_blocks(&raw, self.superblock.has_64bit)?
            .checked_add(1)
            .ok_or(Ext4Error::ArithmeticOverflow)?;
        let free_inodes = read_u32(&raw, SB_FREE_INODES)?
            .checked_add(1)
            .ok_or(Ext4Error::ArithmeticOverflow)?;
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

fn validate_target(inode_number: u32, inode: &Inode) -> Result<(), Ext4Error> {
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
    if inode.flags & INODE_EXTENTS_FL == 0 {
        return Err(Ext4Error::UnsupportedInodeFeature(
            "legacy indirect block mapping",
        ));
    }
    Ok(())
}

fn validate_parent(inode_number: u32, inode: &Inode) -> Result<(), Ext4Error> {
    if inode.file_type() != MODE_DIRECTORY {
        return Err(Ext4Error::NotDirectory(inode_number));
    }
    if inode.flags & EXT4_INDEX_FL != 0 {
        return Err(Ext4Error::UnsupportedInodeFeature(
            "Stage 5 refuses indexed directories",
        ));
    }
    if inode.flags & INODE_INLINE_DATA_FL != 0 {
        return Err(Ext4Error::UnsupportedInodeFeature("inline directory data"));
    }
    Ok(())
}

fn reject_external_xattr_block(raw_inode: &[u8], has_64bit: bool) -> Result<(), Ext4Error> {
    let lo = u64::from(read_u32(raw_inode, INODE_FILE_ACL_LO)?);
    let hi = if has_64bit && raw_inode.len() > INODE_FILE_ACL_HI + 1 {
        u64::from(read_u16(raw_inode, INODE_FILE_ACL_HI)?)
    } else {
        0
    };
    if (hi << 32) | lo != 0 {
        return Err(Ext4Error::UnsupportedInodeFeature(
            "external xattr block on removal target",
        ));
    }
    Ok(())
}

fn block_group_for(
    physical_block: u64,
    first_data_block: u64,
    blocks_per_group: u32,
) -> Result<u32, Ext4Error> {
    let relative = physical_block
        .checked_sub(first_data_block)
        .ok_or(Ext4Error::InvalidFilesystem(
            "data block precedes first_data_block",
        ))?;
    u32::try_from(relative / u64::from(blocks_per_group))
        .map_err(|_| Ext4Error::ArithmeticOverflow)
}

fn block_bit_in_group(
    physical_block: u64,
    group: u32,
    first_data_block: u64,
    blocks_per_group: u32,
) -> Result<u32, Ext4Error> {
    let group_first = first_data_block
        .checked_add(
            u64::from(group)
                .checked_mul(u64::from(blocks_per_group))
                .ok_or(Ext4Error::ArithmeticOverflow)?,
        )
        .ok_or(Ext4Error::ArithmeticOverflow)?;
    let bit = physical_block
        .checked_sub(group_first)
        .ok_or(Ext4Error::InvalidFilesystem(
            "data block lies before its block group",
        ))?;
    if bit >= u64::from(blocks_per_group) {
        return Err(Ext4Error::InvalidFilesystem(
            "data block lies outside selected block group",
        ));
    }
    u32::try_from(bit).map_err(|_| Ext4Error::ArithmeticOverflow)
}

fn rewrite_group_for_remove(
    group: &mut GroupState,
    metadata: &FsMetadata,
    inodes_per_group: u32,
) -> Result<(), Ext4Error> {
    group.free_blocks = group
        .free_blocks
        .checked_add(1)
        .ok_or(Ext4Error::ArithmeticOverflow)?;
    group.free_inodes = group
        .free_inodes
        .checked_add(1)
        .ok_or(Ext4Error::ArithmeticOverflow)?;
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
    } else if provided == (calculated & 0xffff) {
        return Ok(());
    } else {
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

fn require_bitmap_set(bitmap: &[u8], bit: u32, reason: &'static str) -> Result<(), Ext4Error> {
    if bitmap_is_set(bitmap, bit)? {
        Ok(())
    } else {
        Err(Ext4Error::InvalidFilesystem(reason))
    }
}

fn bitmap_is_set(bitmap: &[u8], bit: u32) -> Result<bool, Ext4Error> {
    let byte = usize::try_from(bit / 8).map_err(|_| Ext4Error::ArithmeticOverflow)?;
    let shift = bit % 8;
    let value = *bitmap
        .get(byte)
        .ok_or(Ext4Error::UnexpectedEndOfStructure)?;
    Ok(value & (1_u8 << shift) != 0)
}

fn bitmap_clear(bitmap: &mut [u8], bit: u32) -> Result<(), Ext4Error> {
    let byte = usize::try_from(bit / 8).map_err(|_| Ext4Error::ArithmeticOverflow)?;
    let shift = bit % 8;
    let value = bitmap
        .get_mut(byte)
        .ok_or(Ext4Error::UnexpectedEndOfStructure)?;
    *value &= !(1_u8 << shift);
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
    if provided != crc32c(seed, &block[..tail]) {
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

fn delete_dirent(block: &mut [u8], name: &[u8], target_inode: u32) -> Result<bool, Ext4Error> {
    let limit = block
        .len()
        .checked_sub(DIRENT_TAIL_SIZE)
        .ok_or(Ext4Error::UnexpectedEndOfStructure)?;
    let mut offset = 0_usize;
    let mut previous: Option<(usize, usize)> = None;
    while offset < limit {
        let remaining = limit - offset;
        if remaining < DIRENT_HEADER {
            return Err(Ext4Error::InvalidFilesystem("corrupt directory entry"));
        }
        let inode = read_u32(block, offset)?;
        let rec_len = usize::from(read_u16(block, offset + 4)?);
        if rec_len < DIRENT_HEADER || rec_len % 4 != 0 || rec_len > remaining {
            return Err(Ext4Error::InvalidFilesystem("corrupt directory rec_len"));
        }
        let name_len = usize::from(block[offset + 6]);
        if name_len > rec_len.saturating_sub(DIRENT_HEADER) {
            return Err(Ext4Error::InvalidFilesystem("corrupt directory name_len"));
        }
        let entry_name = block
            .get(offset + DIRENT_HEADER..offset + DIRENT_HEADER + name_len)
            .ok_or(Ext4Error::UnexpectedEndOfStructure)?;
        if inode == target_inode && entry_name == name {
            if let Some((previous_offset, previous_len)) = previous {
                let merged = previous_len
                    .checked_add(rec_len)
                    .ok_or(Ext4Error::ArithmeticOverflow)?;
                write_u16(
                    block,
                    previous_offset + 4,
                    u16::try_from(merged).map_err(|_| Ext4Error::ArithmeticOverflow)?,
                )?;
                block
                    .get_mut(offset..offset + rec_len)
                    .ok_or(Ext4Error::UnexpectedEndOfStructure)?
                    .fill(0);
            } else {
                write_u32(block, offset, 0)?;
                block
                    .get_mut(offset + 6..offset + rec_len)
                    .ok_or(Ext4Error::UnexpectedEndOfStructure)?
                    .fill(0);
            }
            return Ok(true);
        }
        previous = Some((offset, rec_len));
        offset = offset
            .checked_add(rec_len)
            .ok_or(Ext4Error::ArithmeticOverflow)?;
    }
    Ok(false)
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
    replacements.push(ReplacementExtent {
        logical_start: Sector(
            physical_block
                .checked_mul(sectors_per_block)
                .ok_or(Ext4Error::ArithmeticOverflow)?,
        ),
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
    fn clearing_bitmap_bit_uses_lsb_numbering() {
        let mut bitmap = [0xff_u8];
        bitmap_clear(&mut bitmap, 3).unwrap();
        assert_eq!(bitmap[0], 0xf7);
    }

    #[test]
    fn parent_split_preserves_filename() {
        let (parent, name) = split_parent("/system/etc/remove.me").unwrap();
        assert_eq!(parent, "/system/etc");
        assert_eq!(name, "remove.me");
    }
}

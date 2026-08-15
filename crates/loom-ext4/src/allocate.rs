#![forbid(unsafe_code)]

use super::checksum::{crc32c, rewrite_inode_checksum};
use super::{
    read_exact_at, read_u16, read_u32, Ext4Error, Ext4Image, Inode, INODE_EXTENTS_FL,
    INODE_INLINE_DATA_FL, INODE_VERITY_FL, MODE_REGULAR, SECTOR_SIZE, SUPERBLOCK_OFFSET,
    SUPERBLOCK_SIZE,
};
use loom_map::{LoomMap, ReplacementExtent};
use loom_types::{Sector, SectorCount};
use std::fs;
use std::path::Path;

const RO_COMPAT_METADATA_CSUM: u32 = 0x0400;
const INCOMPAT_CSUM_SEED: u32 = 0x2000;
const EXT4_OS_LINUX: u32 = 0;
const EXT4_CRC32C_CHKSUM: u8 = 1;
const EXT4_BG_BLOCK_UNINIT: u16 = 0x0002;
const EXT4_HUGE_FILE_FL: u32 = 0x0004_0000;

const SB_FREE_BLOCKS_LO: usize = 0x0c;
const SB_FIRST_DATA_BLOCK: usize = 0x14;
const SB_BLOCKS_PER_GROUP: usize = 0x20;
const SB_CREATOR_OS: usize = 0x48;
const SB_FEATURE_INCOMPAT: usize = 0x60;
const SB_FEATURE_RO_COMPAT: usize = 0x64;
const SB_UUID: usize = 0x68;
const SB_UUID_SIZE: usize = 16;
const SB_CHECKSUM_TYPE: usize = 0x175;
const SB_FREE_BLOCKS_HI: usize = 0x158;
const SB_CHECKSUM_SEED: usize = 0x270;
const SB_CHECKSUM: usize = 0x3fc;

const GD_BLOCK_BITMAP_LO: usize = 0x00;
const GD_FREE_BLOCKS_LO: usize = 0x0c;
const GD_FLAGS: usize = 0x12;
const GD_BLOCK_BITMAP_CSUM_LO: usize = 0x18;
const GD_CHECKSUM: usize = 0x1e;
const GD_BLOCK_BITMAP_HI: usize = 0x20;
const GD_FREE_BLOCKS_HI: usize = 0x2c;
const GD_BLOCK_BITMAP_CSUM_HI: usize = 0x38;
const GD_BITMAP_CSUM_HI_END: usize = 0x3a;

const INODE_SIZE_LO: usize = 0x04;
const INODE_BLOCKS_LO: usize = 0x1c;
const INODE_BLOCK: usize = 0x28;
const INODE_BLOCK_LEN: usize = 60;
const INODE_SIZE_HI: usize = 0x6c;
const INODE_BLOCKS_HI: usize = 0x74;

const EXTENT_HEADER_SIZE: usize = 12;
const EXTENT_ENTRY_SIZE: usize = 12;
const EXTENT_MAGIC: u16 = 0xf30a;

#[derive(Debug)]
pub struct CompiledAllocationGrow {
    pub map: LoomMap,
    pub shadow: Vec<u8>,
    pub block_size: u32,
    pub inode: u32,
    pub original_size: u64,
    pub effective_size: u64,
    pub original_data_blocks: usize,
    pub allocated_block: u64,
    pub allocation_group: u32,
    pub data_shadow_blocks: usize,
    pub metadata_blocks: usize,
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
    first_block: u64,
    descriptor_block: u64,
    descriptor_offset: usize,
    descriptor_block_bytes: Vec<u8>,
    descriptor: Vec<u8>,
    bitmap_block: u64,
    bitmap: Vec<u8>,
    free_blocks: u32,
}

/// Grows one dense regular file across exactly one ext4 data-block allocation boundary.
///
/// Stage 3 deliberately supports only an inline (inode-root) extent tree with depth 0.
/// It allocates one already-free data block in the effective view and shadows the complete
/// metadata closure: block bitmap, group descriptor, primary superblock, target inode block,
/// and all changed/new data blocks. The authoritative origin is opened read-only.
///
/// # Errors
/// Returns [`Ext4Error`] for unsupported filesystem/inode features, malformed metadata,
/// insufficient safe allocator state, mapping failures, or filesystem I/O errors.
pub fn compile_grow_with_block_allocation(
    origin_path: &Path,
    target_path: &str,
    replacement_path: &Path,
) -> Result<CompiledAllocationGrow, Ext4Error> {
    let replacement = fs::read(replacement_path).map_err(Ext4Error::Io)?;
    let mut image = Ext4Image::open(origin_path)?;
    let inode_number = image.resolve_path(target_path)?;
    image.compile_one_block_growth(inode_number, &replacement)
}

impl Ext4Image {
    #[allow(clippy::too_many_lines)] // transaction orchestration; helpers own the individual mutations
    pub(crate) fn compile_one_block_growth(
        &mut self,
        inode_number: u32,
        replacement: &[u8],
    ) -> Result<CompiledAllocationGrow, Ext4Error> {
        let inode = self.read_inode(inode_number)?;
        validate_target_inode(inode_number, &inode)?;

        let block_size_u64 = u64::from(self.superblock.block_size);
        let block_size =
            usize::try_from(block_size_u64).map_err(|_| Ext4Error::ArithmeticOverflow)?;
        let sectors_per_block = block_size_u64 / SECTOR_SIZE;
        let original_blocks = self.file_blocks(&inode)?;
        if original_blocks.is_empty() {
            return Err(Ext4Error::UnsupportedInodeFeature(
                "Stage 3 requires at least one existing data block",
            ));
        }

        let effective_size =
            u64::try_from(replacement.len()).map_err(|_| Ext4Error::ArithmeticOverflow)?;
        if effective_size <= inode.size {
            return Err(Ext4Error::UnsupportedInodeFeature(
                "Stage 3 only implements file growth",
            ));
        }
        let required_blocks = blocks_for_size(effective_size, block_size_u64)?;
        let original_block_count =
            u64::try_from(original_blocks.len()).map_err(|_| Ext4Error::ArithmeticOverflow)?;
        if required_blocks != original_block_count.saturating_add(1) {
            return Err(Ext4Error::UnsupportedInodeFeature(
                "Stage 3 growth must require exactly one additional data block",
            ));
        }

        let metadata = self.read_allocator_metadata()?;
        let preferred_group = block_group_for(
            *original_blocks
                .last()
                .ok_or(Ext4Error::ArithmeticOverflow)?,
            metadata.first_data_block,
            metadata.blocks_per_group,
        )?;
        let mut group = self.find_allocatable_group(preferred_group, &metadata)?;
        let allocated_block =
            allocate_bitmap_bit(&mut group, &metadata, self.superblock.blocks_count)?;

        let mut shadow = Vec::new();
        let mut replacements = Vec::new();
        let mut data_shadow_blocks = 0_usize;

        for (file_block_index, physical_block) in original_blocks.iter().copied().enumerate() {
            let origin_block = self.read_block(physical_block)?;
            let effective_block = materialize_existing_block(
                &origin_block,
                replacement,
                file_block_index,
                block_size,
            )?;
            if effective_block == origin_block {
                continue;
            }
            append_shadow_block(
                &mut shadow,
                &mut replacements,
                physical_block,
                sectors_per_block,
                &effective_block,
            )?;
            data_shadow_blocks = data_shadow_blocks
                .checked_add(1)
                .ok_or(Ext4Error::ArithmeticOverflow)?;
        }

        let new_file_block_index = original_blocks.len();
        let new_data = materialize_new_block(replacement, new_file_block_index, block_size)?;
        append_shadow_block(
            &mut shadow,
            &mut replacements,
            allocated_block,
            sectors_per_block,
            &new_data,
        )?;
        data_shadow_blocks = data_shadow_blocks
            .checked_add(1)
            .ok_or(Ext4Error::ArithmeticOverflow)?;

        let inode_shadow = self.build_grown_inode_block(
            inode_number,
            &inode,
            original_block_count,
            allocated_block,
            effective_size,
            sectors_per_block,
            metadata.checksum_seed,
        )?;
        append_shadow_block(
            &mut shadow,
            &mut replacements,
            inode_shadow.0,
            sectors_per_block,
            &inode_shadow.1,
        )?;

        rewrite_group_metadata(&mut group, &metadata)?;
        append_shadow_block(
            &mut shadow,
            &mut replacements,
            group.bitmap_block,
            sectors_per_block,
            &group.bitmap,
        )?;
        append_shadow_block(
            &mut shadow,
            &mut replacements,
            group.descriptor_block,
            sectors_per_block,
            &group.descriptor_block_bytes,
        )?;

        let (superblock_block, superblock_bytes) =
            self.build_decremented_superblock(&metadata, sectors_per_block)?;
        append_shadow_block(
            &mut shadow,
            &mut replacements,
            superblock_block,
            sectors_per_block,
            &superblock_bytes,
        )?;

        let total_sectors = self.image_bytes / SECTOR_SIZE;
        let map = LoomMap::from_replacements(SectorCount(total_sectors), &replacements)
            .map_err(Ext4Error::Map)?;
        let metadata_blocks = 4_usize;
        let shadow_blocks = data_shadow_blocks
            .checked_add(metadata_blocks)
            .ok_or(Ext4Error::ArithmeticOverflow)?;

        Ok(CompiledAllocationGrow {
            map,
            shadow,
            block_size: self.superblock.block_size,
            inode: inode_number,
            original_size: inode.size,
            effective_size,
            original_data_blocks: original_blocks.len(),
            allocated_block,
            allocation_group: group.group,
            data_shadow_blocks,
            metadata_blocks,
            shadow_blocks,
        })
    }

    fn read_allocator_metadata(&mut self) -> Result<FsMetadata, Ext4Error> {
        let mut raw = [0_u8; SUPERBLOCK_SIZE];
        read_exact_at(&mut self.file, SUPERBLOCK_OFFSET, &mut raw)?;
        let ro_compat = read_u32(&raw, SB_FEATURE_RO_COMPAT)?;
        if ro_compat & RO_COMPAT_METADATA_CSUM == 0 {
            return Err(Ext4Error::UnsupportedFilesystemFeature(
                "Stage 3 allocator requires metadata_csum",
            ));
        }
        if read_u32(&raw, SB_CREATOR_OS)? != EXT4_OS_LINUX {
            return Err(Ext4Error::UnsupportedFilesystemFeature(
                "metadata_csum on non-Linux creator OS",
            ));
        }
        if raw[SB_CHECKSUM_TYPE] != EXT4_CRC32C_CHKSUM {
            return Err(Ext4Error::UnsupportedFilesystemFeature(
                "non-CRC32C metadata checksum algorithm",
            ));
        }

        let incompat = read_u32(&raw, SB_FEATURE_INCOMPAT)?;
        let checksum_seed = if incompat & INCOMPAT_CSUM_SEED != 0 {
            read_u32(&raw, SB_CHECKSUM_SEED)?
        } else {
            let uuid_end = SB_UUID
                .checked_add(SB_UUID_SIZE)
                .ok_or(Ext4Error::ArithmeticOverflow)?;
            let uuid = raw
                .get(SB_UUID..uuid_end)
                .ok_or(Ext4Error::UnexpectedEndOfStructure)?;
            crc32c(u32::MAX, uuid)
        };

        let blocks_per_group = read_u32(&raw, SB_BLOCKS_PER_GROUP)?;
        if blocks_per_group == 0 || blocks_per_group % 8 != 0 {
            return Err(Ext4Error::InvalidFilesystem(
                "invalid ext4 blocks_per_group for Stage 3",
            ));
        }
        let bitmap_bytes =
            usize::try_from(blocks_per_group / 8).map_err(|_| Ext4Error::ArithmeticOverflow)?;
        if bitmap_bytes
            > usize::try_from(self.superblock.block_size)
                .map_err(|_| Ext4Error::ArithmeticOverflow)?
        {
            return Err(Ext4Error::InvalidFilesystem(
                "block bitmap exceeds filesystem block size",
            ));
        }

        Ok(FsMetadata {
            raw_superblock: raw,
            checksum_seed,
            first_data_block: u64::from(read_u32(&raw, SB_FIRST_DATA_BLOCK)?),
            blocks_per_group,
        })
    }

    fn find_allocatable_group(
        &mut self,
        preferred_group: u32,
        metadata: &FsMetadata,
    ) -> Result<GroupState, Ext4Error> {
        let data_blocks = self
            .superblock
            .blocks_count
            .checked_sub(metadata.first_data_block)
            .ok_or(Ext4Error::InvalidFilesystem(
                "first_data_block lies beyond filesystem",
            ))?;
        let groups_u64 = data_blocks
            .checked_add(u64::from(metadata.blocks_per_group).saturating_sub(1))
            .ok_or(Ext4Error::ArithmeticOverflow)?
            / u64::from(metadata.blocks_per_group);
        let groups = u32::try_from(groups_u64).map_err(|_| Ext4Error::ArithmeticOverflow)?;
        if preferred_group >= groups {
            return Err(Ext4Error::InvalidFilesystem(
                "preferred allocation group lies outside filesystem",
            ));
        }

        let mut order = Vec::with_capacity(usize::try_from(groups).unwrap_or(0));
        order.push(preferred_group);
        for group in 0..groups {
            if group != preferred_group {
                order.push(group);
            }
        }

        for group in order {
            let state = self.read_group_state(group, metadata)?;
            if state.free_blocks == 0 {
                continue;
            }
            if find_free_bit(
                &state.bitmap,
                valid_blocks_in_group(group, metadata, self.superblock.blocks_count)?,
            )
            .is_some()
            {
                return Ok(state);
            }
        }

        Err(Ext4Error::InvalidFilesystem(
            "no initialized free data block is available for Stage 3",
        ))
    }

    fn read_group_state(
        &mut self,
        group: u32,
        metadata: &FsMetadata,
    ) -> Result<GroupState, Ext4Error> {
        let descriptor_size = usize::from(self.superblock.descriptor_size);
        let block_size = usize::try_from(self.superblock.block_size)
            .map_err(|_| Ext4Error::ArithmeticOverflow)?;
        let descriptor_start_block = if self.superblock.block_size == 1024 {
            2_u64
        } else {
            1_u64
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
        if descriptor_end > block_size {
            return Err(Ext4Error::InvalidFilesystem(
                "group descriptor crosses a filesystem block",
            ));
        }

        let descriptor_block_bytes = self.read_block(descriptor_block)?;
        let descriptor = descriptor_block_bytes
            .get(descriptor_offset..descriptor_end)
            .ok_or(Ext4Error::UnexpectedEndOfStructure)?
            .to_vec();
        verify_group_descriptor_checksum(&descriptor, group, metadata.checksum_seed)?;

        let flags = read_u16(&descriptor, GD_FLAGS)?;
        if flags & EXT4_BG_BLOCK_UNINIT != 0 {
            return Err(Ext4Error::UnsupportedFilesystemFeature(
                "Stage 3 refuses EXT4_BG_BLOCK_UNINIT allocator state",
            ));
        }

        let bitmap_lo = u64::from(read_u32(&descriptor, GD_BLOCK_BITMAP_LO)?);
        let bitmap_hi = if self.superblock.has_64bit {
            u64::from(read_u32(&descriptor, GD_BLOCK_BITMAP_HI)?)
        } else {
            0
        };
        let bitmap_block = (bitmap_hi << 32) | bitmap_lo;
        if bitmap_block >= self.superblock.blocks_count {
            return Err(Ext4Error::InvalidFilesystem(
                "group block bitmap lies outside filesystem",
            ));
        }
        let bitmap = self.read_block(bitmap_block)?;
        verify_block_bitmap_checksum(
            &bitmap,
            &descriptor,
            metadata.checksum_seed,
            metadata.blocks_per_group,
        )?;

        let free_lo = u32::from(read_u16(&descriptor, GD_FREE_BLOCKS_LO)?);
        let free_hi = if self.superblock.has_64bit {
            u32::from(read_u16(&descriptor, GD_FREE_BLOCKS_HI)?)
        } else {
            0
        };
        let free_blocks = (free_hi << 16) | free_lo;
        let first_block = metadata
            .first_data_block
            .checked_add(
                u64::from(group)
                    .checked_mul(u64::from(metadata.blocks_per_group))
                    .ok_or(Ext4Error::ArithmeticOverflow)?,
            )
            .ok_or(Ext4Error::ArithmeticOverflow)?;

        Ok(GroupState {
            group,
            first_block,
            descriptor_block,
            descriptor_offset,
            descriptor_block_bytes,
            descriptor,
            bitmap_block,
            bitmap,
            free_blocks,
        })
    }

    #[allow(clippy::too_many_arguments)] // all arguments are explicit ext4 accounting inputs
    fn build_grown_inode_block(
        &mut self,
        inode_number: u32,
        inode: &Inode,
        original_blocks: u64,
        allocated_block: u64,
        effective_size: u64,
        sectors_per_block: u64,
        checksum_seed: u32,
    ) -> Result<(u64, Vec<u8>), Ext4Error> {
        let (inode_table_block, inode_offset) = inode_record_location(self, inode_number)?;
        let mut inode_table_shadow = self.read_block(inode_table_block)?;
        let inode_size = usize::from(self.superblock.inode_size);
        let inode_end = inode_offset
            .checked_add(inode_size)
            .ok_or(Ext4Error::ArithmeticOverflow)?;
        let raw_inode = inode_table_shadow.get_mut(inode_offset..inode_end).ok_or(
            Ext4Error::InvalidFilesystem("inode record crosses inode-table filesystem block"),
        )?;

        if inode.flags & EXT4_HUGE_FILE_FL != 0 {
            return Err(Ext4Error::UnsupportedInodeFeature(
                "EXT4_HUGE_FILE_FL i_blocks accounting",
            ));
        }
        if read_u16(raw_inode, INODE_BLOCKS_HI)? != 0 {
            return Err(Ext4Error::UnsupportedInodeFeature(
                "non-zero i_blocks_high accounting",
            ));
        }
        let blocks_lo = read_u32(raw_inode, INODE_BLOCKS_LO)?;
        let sectors_u32 =
            u32::try_from(sectors_per_block).map_err(|_| Ext4Error::ArithmeticOverflow)?;
        let new_blocks_lo = blocks_lo
            .checked_add(sectors_u32)
            .ok_or(Ext4Error::UnsupportedInodeFeature("i_blocks_lo overflow"))?;
        write_u32(raw_inode, INODE_BLOCKS_LO, new_blocks_lo)?;

        write_u64_split(raw_inode, INODE_SIZE_LO, INODE_SIZE_HI, effective_size)?;
        append_inline_extent(raw_inode, original_blocks, allocated_block)?;
        rewrite_inode_checksum(raw_inode, checksum_seed, inode_number)
            .map_err(Ext4Error::Checksum)?;

        Ok((inode_table_block, inode_table_shadow))
    }

    fn build_decremented_superblock(
        &mut self,
        metadata: &FsMetadata,
        _sectors_per_block: u64,
    ) -> Result<(u64, Vec<u8>), Ext4Error> {
        let mut raw = metadata.raw_superblock;
        let free_lo = u64::from(read_u32(&raw, SB_FREE_BLOCKS_LO)?);
        let free_hi = if self.superblock.has_64bit {
            u64::from(read_u32(&raw, SB_FREE_BLOCKS_HI)?)
        } else {
            0
        };
        let free_blocks = (free_hi << 32) | free_lo;
        let new_free = free_blocks
            .checked_sub(1)
            .ok_or(Ext4Error::InvalidFilesystem(
                "superblock free-block count is already zero",
            ))?;
        write_u32(&mut raw, SB_FREE_BLOCKS_LO, low_u32(new_free))?;
        if self.superblock.has_64bit {
            write_u32(
                &mut raw,
                SB_FREE_BLOCKS_HI,
                u32::try_from(new_free >> 32).map_err(|_| Ext4Error::ArithmeticOverflow)?,
            )?;
        } else if new_free > u64::from(u32::MAX) {
            return Err(Ext4Error::InvalidFilesystem(
                "32-bit filesystem has oversized free-block count",
            ));
        }

        write_u32(&mut raw, SB_CHECKSUM, 0)?;
        let checksum = crc32c(u32::MAX, &raw[..SB_CHECKSUM]);
        write_u32(&mut raw, SB_CHECKSUM, checksum)?;

        let block_size = u64::from(self.superblock.block_size);
        let superblock_block = SUPERBLOCK_OFFSET / block_size;
        let offset = usize::try_from(SUPERBLOCK_OFFSET % block_size)
            .map_err(|_| Ext4Error::ArithmeticOverflow)?;
        let mut block = self.read_block(superblock_block)?;
        let end = offset
            .checked_add(SUPERBLOCK_SIZE)
            .ok_or(Ext4Error::ArithmeticOverflow)?;
        let target = block
            .get_mut(offset..end)
            .ok_or(Ext4Error::InvalidFilesystem(
                "primary superblock does not fit in filesystem block",
            ))?;
        target.copy_from_slice(&raw);
        Ok((superblock_block, block))
    }
}

fn validate_target_inode(inode_number: u32, inode: &Inode) -> Result<(), Ext4Error> {
    if inode.file_type() != MODE_REGULAR {
        return Err(Ext4Error::NotRegularFile(inode_number));
    }
    if inode.links_count != 1 {
        return Err(Ext4Error::HardLinkedTarget {
            inode: inode_number,
            links: inode.links_count,
        });
    }
    if inode.flags & INODE_EXTENTS_FL == 0 {
        return Err(Ext4Error::UnsupportedInodeFeature(
            "legacy indirect block mapping",
        ));
    }
    if inode.flags & INODE_INLINE_DATA_FL != 0 {
        return Err(Ext4Error::UnsupportedInodeFeature("inline data"));
    }
    if inode.flags & INODE_VERITY_FL != 0 {
        return Err(Ext4Error::UnsupportedInodeFeature("fs-verity"));
    }
    Ok(())
}

fn block_group_for(
    physical_block: u64,
    first_data_block: u64,
    blocks_per_group: u32,
) -> Result<u32, Ext4Error> {
    let relative =
        physical_block
            .checked_sub(first_data_block)
            .ok_or(Ext4Error::InvalidFilesystem(
                "file data block precedes first_data_block",
            ))?;
    u32::try_from(relative / u64::from(blocks_per_group)).map_err(|_| Ext4Error::ArithmeticOverflow)
}

fn valid_blocks_in_group(
    group: u32,
    metadata: &FsMetadata,
    blocks_count: u64,
) -> Result<u32, Ext4Error> {
    let first = metadata
        .first_data_block
        .checked_add(
            u64::from(group)
                .checked_mul(u64::from(metadata.blocks_per_group))
                .ok_or(Ext4Error::ArithmeticOverflow)?,
        )
        .ok_or(Ext4Error::ArithmeticOverflow)?;
    if first >= blocks_count {
        return Err(Ext4Error::InvalidFilesystem(
            "block group starts beyond filesystem",
        ));
    }
    let remaining = blocks_count - first;
    u32::try_from(remaining.min(u64::from(metadata.blocks_per_group)))
        .map_err(|_| Ext4Error::ArithmeticOverflow)
}

fn find_free_bit(bitmap: &[u8], valid_blocks: u32) -> Option<u32> {
    (0..valid_blocks).find(|bit| {
        let byte = usize::try_from(*bit / 8).ok();
        let shift = (*bit % 8) as u8;
        byte.and_then(|index| bitmap.get(index))
            .is_some_and(|value| value & (1_u8 << shift) == 0)
    })
}

fn allocate_bitmap_bit(
    group: &mut GroupState,
    metadata: &FsMetadata,
    blocks_count: u64,
) -> Result<u64, Ext4Error> {
    let valid = valid_blocks_in_group(group.group, metadata, blocks_count)?;
    let bit = find_free_bit(&group.bitmap, valid).ok_or(Ext4Error::InvalidFilesystem(
        "group free-block count disagrees with block bitmap",
    ))?;
    let byte_index = usize::try_from(bit / 8).map_err(|_| Ext4Error::ArithmeticOverflow)?;
    let shift = (bit % 8) as u8;
    let byte = group
        .bitmap
        .get_mut(byte_index)
        .ok_or(Ext4Error::UnexpectedEndOfStructure)?;
    *byte |= 1_u8 << shift;
    group
        .first_block
        .checked_add(u64::from(bit))
        .ok_or(Ext4Error::ArithmeticOverflow)
}

fn rewrite_group_metadata(group: &mut GroupState, metadata: &FsMetadata) -> Result<(), Ext4Error> {
    let new_free = group
        .free_blocks
        .checked_sub(1)
        .ok_or(Ext4Error::InvalidFilesystem(
            "group free-block count is already zero",
        ))?;
    write_u16(
        &mut group.descriptor,
        GD_FREE_BLOCKS_LO,
        low_u16(u64::from(new_free)),
    )?;
    if group.descriptor.len() >= 64 {
        write_u16(
            &mut group.descriptor,
            GD_FREE_BLOCKS_HI,
            u16::try_from(new_free >> 16).map_err(|_| Ext4Error::ArithmeticOverflow)?,
        )?;
    } else if new_free > u32::from(u16::MAX) {
        return Err(Ext4Error::InvalidFilesystem(
            "32-byte group descriptor cannot represent free-block count",
        ));
    }

    let bitmap_len = usize::try_from(metadata.blocks_per_group / 8)
        .map_err(|_| Ext4Error::ArithmeticOverflow)?;
    let bitmap_region = group
        .bitmap
        .get(..bitmap_len)
        .ok_or(Ext4Error::UnexpectedEndOfStructure)?;
    let bitmap_csum = crc32c(metadata.checksum_seed, bitmap_region);
    write_u16(
        &mut group.descriptor,
        GD_BLOCK_BITMAP_CSUM_LO,
        low_u16(u64::from(bitmap_csum)),
    )?;
    if group.descriptor.len() >= GD_BITMAP_CSUM_HI_END {
        write_u16(
            &mut group.descriptor,
            GD_BLOCK_BITMAP_CSUM_HI,
            u16::try_from(bitmap_csum >> 16).map_err(|_| Ext4Error::ArithmeticOverflow)?,
        )?;
    }

    write_u16(&mut group.descriptor, GD_CHECKSUM, 0)?;
    let mut descriptor_csum = crc32c(metadata.checksum_seed, &group.group.to_le_bytes());
    descriptor_csum = crc32c(descriptor_csum, &group.descriptor);
    write_u16(
        &mut group.descriptor,
        GD_CHECKSUM,
        low_u16(u64::from(descriptor_csum)),
    )?;

    let descriptor_end = group
        .descriptor_offset
        .checked_add(group.descriptor.len())
        .ok_or(Ext4Error::ArithmeticOverflow)?;
    let target = group
        .descriptor_block_bytes
        .get_mut(group.descriptor_offset..descriptor_end)
        .ok_or(Ext4Error::UnexpectedEndOfStructure)?;
    target.copy_from_slice(&group.descriptor);
    group.free_blocks = new_free;
    Ok(())
}

fn verify_group_descriptor_checksum(
    descriptor: &[u8],
    group: u32,
    checksum_seed: u32,
) -> Result<(), Ext4Error> {
    let provided = read_u16(descriptor, GD_CHECKSUM)?;
    let mut copy = descriptor.to_vec();
    write_u16(&mut copy, GD_CHECKSUM, 0)?;
    let mut checksum = crc32c(checksum_seed, &group.to_le_bytes());
    checksum = crc32c(checksum, &copy);
    if provided != low_u16(u64::from(checksum)) {
        return Err(Ext4Error::InvalidFilesystem(
            "group descriptor checksum mismatch",
        ));
    }
    Ok(())
}

fn verify_block_bitmap_checksum(
    bitmap: &[u8],
    descriptor: &[u8],
    checksum_seed: u32,
    blocks_per_group: u32,
) -> Result<(), Ext4Error> {
    let bitmap_len =
        usize::try_from(blocks_per_group / 8).map_err(|_| Ext4Error::ArithmeticOverflow)?;
    let region = bitmap
        .get(..bitmap_len)
        .ok_or(Ext4Error::UnexpectedEndOfStructure)?;
    let calculated = crc32c(checksum_seed, region);
    let mut provided = u32::from(read_u16(descriptor, GD_BLOCK_BITMAP_CSUM_LO)?);
    if descriptor.len() >= GD_BITMAP_CSUM_HI_END {
        provided |= u32::from(read_u16(descriptor, GD_BLOCK_BITMAP_CSUM_HI)?) << 16;
    } else if calculated > u32::from(u16::MAX) {
        if provided != (calculated & 0xffff) {
            return Err(Ext4Error::InvalidFilesystem(
                "block bitmap checksum mismatch",
            ));
        }
        return Ok(());
    }
    if provided != calculated {
        return Err(Ext4Error::InvalidFilesystem(
            "block bitmap checksum mismatch",
        ));
    }
    Ok(())
}

fn inode_record_location(
    image: &mut Ext4Image,
    inode_number: u32,
) -> Result<(u64, usize), Ext4Error> {
    if inode_number == 0 || inode_number > image.superblock.inodes_count {
        return Err(Ext4Error::InvalidInode(inode_number));
    }
    let zero_based = inode_number - 1;
    let group = zero_based / image.superblock.inodes_per_group;
    let index = zero_based % image.superblock.inodes_per_group;
    let table_start = image.inode_table_block(group)?;
    let byte_offset = u64::from(index)
        .checked_mul(u64::from(image.superblock.inode_size))
        .ok_or(Ext4Error::ArithmeticOverflow)?;
    let block_size = u64::from(image.superblock.block_size);
    let table_block = table_start
        .checked_add(byte_offset / block_size)
        .ok_or(Ext4Error::ArithmeticOverflow)?;
    let offset =
        usize::try_from(byte_offset % block_size).map_err(|_| Ext4Error::ArithmeticOverflow)?;
    let end = offset
        .checked_add(usize::from(image.superblock.inode_size))
        .ok_or(Ext4Error::ArithmeticOverflow)?;
    if end > usize::try_from(block_size).map_err(|_| Ext4Error::ArithmeticOverflow)? {
        return Err(Ext4Error::InvalidFilesystem(
            "inode record crosses inode-table filesystem block",
        ));
    }
    Ok((table_block, offset))
}

fn append_inline_extent(
    raw_inode: &mut [u8],
    original_blocks: u64,
    allocated_block: u64,
) -> Result<(), Ext4Error> {
    let block_end = INODE_BLOCK
        .checked_add(INODE_BLOCK_LEN)
        .ok_or(Ext4Error::ArithmeticOverflow)?;
    let root = raw_inode
        .get_mut(INODE_BLOCK..block_end)
        .ok_or(Ext4Error::UnexpectedEndOfStructure)?;
    if read_u16(root, 0)? != EXTENT_MAGIC {
        return Err(Ext4Error::CorruptExtentTree);
    }
    let entries = usize::from(read_u16(root, 2)?);
    let maximum = usize::from(read_u16(root, 4)?);
    let depth = read_u16(root, 6)?;
    if depth != 0 {
        return Err(Ext4Error::UnsupportedInodeFeature(
            "Stage 3 requires an inline depth-0 extent root",
        ));
    }
    let physical_capacity = (INODE_BLOCK_LEN - EXTENT_HEADER_SIZE) / EXTENT_ENTRY_SIZE;
    if entries == 0 || entries > maximum || maximum > physical_capacity {
        return Err(Ext4Error::CorruptExtentTree);
    }

    let last_offset = EXTENT_HEADER_SIZE
        .checked_add(
            (entries - 1)
                .checked_mul(EXTENT_ENTRY_SIZE)
                .ok_or(Ext4Error::ArithmeticOverflow)?,
        )
        .ok_or(Ext4Error::ArithmeticOverflow)?;
    let last_logical = u64::from(read_u32(root, last_offset)?);
    let last_len = u64::from(read_u16(root, last_offset + 4)?);
    if last_len == 0 || last_len > 32_768 {
        return Err(Ext4Error::UnsupportedInodeFeature("unwritten extent"));
    }
    if last_logical
        .checked_add(last_len)
        .ok_or(Ext4Error::ArithmeticOverflow)?
        != original_blocks
    {
        return Err(Ext4Error::UnsupportedInodeFeature(
            "extent root contains allocation beyond logical EOF",
        ));
    }
    let last_physical = (u64::from(read_u16(root, last_offset + 6)?) << 32)
        | u64::from(read_u32(root, last_offset + 8)?);

    if last_physical
        .checked_add(last_len)
        .ok_or(Ext4Error::ArithmeticOverflow)?
        == allocated_block
        && last_len < 32_768
    {
        write_u16(
            root,
            last_offset + 4,
            u16::try_from(last_len + 1).map_err(|_| Ext4Error::ArithmeticOverflow)?,
        )?;
        return Ok(());
    }

    if entries >= maximum {
        return Err(Ext4Error::UnsupportedInodeFeature(
            "inline extent root has no free entry",
        ));
    }
    let new_offset = EXTENT_HEADER_SIZE
        .checked_add(
            entries
                .checked_mul(EXTENT_ENTRY_SIZE)
                .ok_or(Ext4Error::ArithmeticOverflow)?,
        )
        .ok_or(Ext4Error::ArithmeticOverflow)?;
    let logical = u32::try_from(original_blocks).map_err(|_| Ext4Error::ArithmeticOverflow)?;
    write_u32(root, new_offset, logical)?;
    write_u16(root, new_offset + 4, 1)?;
    write_u16(
        root,
        new_offset + 6,
        u16::try_from(allocated_block >> 32).map_err(|_| Ext4Error::ArithmeticOverflow)?,
    )?;
    write_u32(root, new_offset + 8, low_u32(allocated_block))?;
    write_u16(
        root,
        2,
        u16::try_from(entries + 1).map_err(|_| Ext4Error::ArithmeticOverflow)?,
    )?;
    Ok(())
}

fn materialize_existing_block(
    origin_block: &[u8],
    replacement: &[u8],
    file_block_index: usize,
    block_size: usize,
) -> Result<Vec<u8>, Ext4Error> {
    let mut block = origin_block.to_vec();
    let start = file_block_index
        .checked_mul(block_size)
        .ok_or(Ext4Error::ArithmeticOverflow)?;
    let remaining = replacement.len().saturating_sub(start);
    let copy_len = remaining.min(block_size);
    if copy_len != 0 {
        let end = start
            .checked_add(copy_len)
            .ok_or(Ext4Error::ArithmeticOverflow)?;
        block[..copy_len].copy_from_slice(&replacement[start..end]);
    }
    Ok(block)
}

fn materialize_new_block(
    replacement: &[u8],
    file_block_index: usize,
    block_size: usize,
) -> Result<Vec<u8>, Ext4Error> {
    let mut block = vec![0_u8; block_size];
    let start = file_block_index
        .checked_mul(block_size)
        .ok_or(Ext4Error::ArithmeticOverflow)?;
    if start >= replacement.len() {
        return Err(Ext4Error::InvalidFilesystem(
            "newly allocated file block has no replacement bytes",
        ));
    }
    let remaining = replacement.len() - start;
    let copy_len = remaining.min(block_size);
    let end = start
        .checked_add(copy_len)
        .ok_or(Ext4Error::ArithmeticOverflow)?;
    block[..copy_len].copy_from_slice(&replacement[start..end]);
    Ok(block)
}

fn blocks_for_size(size: u64, block_size: u64) -> Result<u64, Ext4Error> {
    size.checked_add(block_size.saturating_sub(1))
        .ok_or(Ext4Error::ArithmeticOverflow)
        .map(|rounded| rounded / block_size)
}

fn append_shadow_block(
    shadow: &mut Vec<u8>,
    replacements: &mut Vec<ReplacementExtent>,
    physical_block: u64,
    sectors_per_block: u64,
    block: &[u8],
) -> Result<(), Ext4Error> {
    let expected_bytes = usize::try_from(
        sectors_per_block
            .checked_mul(SECTOR_SIZE)
            .ok_or(Ext4Error::ArithmeticOverflow)?,
    )
    .map_err(|_| Ext4Error::ArithmeticOverflow)?;
    if block.len() != expected_bytes {
        return Err(Ext4Error::InvalidFilesystem(
            "shadow block does not match filesystem block size",
        ));
    }
    let shadow_bytes = u64::try_from(shadow.len()).map_err(|_| Ext4Error::ArithmeticOverflow)?;
    if shadow_bytes % SECTOR_SIZE != 0 {
        return Err(Ext4Error::InvalidFilesystem(
            "shadow pack lost sector alignment",
        ));
    }
    let logical_start = physical_block
        .checked_mul(sectors_per_block)
        .ok_or(Ext4Error::ArithmeticOverflow)?;
    replacements.push(ReplacementExtent {
        logical_start: Sector(logical_start),
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
    let target = bytes
        .get_mut(offset..end)
        .ok_or(Ext4Error::UnexpectedEndOfStructure)?;
    target.copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn write_u32(bytes: &mut [u8], offset: usize, value: u32) -> Result<(), Ext4Error> {
    let end = offset.checked_add(4).ok_or(Ext4Error::ArithmeticOverflow)?;
    let target = bytes
        .get_mut(offset..end)
        .ok_or(Ext4Error::UnexpectedEndOfStructure)?;
    target.copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn write_u64_split(
    bytes: &mut [u8],
    low_offset: usize,
    high_offset: usize,
    value: u64,
) -> Result<(), Ext4Error> {
    write_u32(bytes, low_offset, low_u32(value))?;
    write_u32(
        bytes,
        high_offset,
        u32::try_from(value >> 32).map_err(|_| Ext4Error::ArithmeticOverflow)?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_bit_scan_uses_ext4_lsb_bit_numbering() {
        let bitmap = [0b1111_0111_u8];
        assert_eq!(find_free_bit(&bitmap, 8), Some(3));
    }

    #[test]
    fn block_group_math_respects_first_data_block() {
        assert_eq!(block_group_for(8193, 1, 8192).unwrap(), 1);
    }

    #[test]
    fn size_rounding_crosses_one_block_boundary() {
        assert_eq!(blocks_for_size(4097, 4096).unwrap(), 2);
    }
}

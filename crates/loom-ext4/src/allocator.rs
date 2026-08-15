#![forbid(unsafe_code)]

use super::checksum::{crc32c, rewrite_inode_checksum};
use super::{
    read_exact_at, read_u16, read_u32, Ext4Error, Ext4Image, Inode, INODE_INLINE_DATA_FL,
    INODE_VERITY_FL, MODE_REGULAR, SECTOR_SIZE, SUPERBLOCK_OFFSET, SUPERBLOCK_SIZE,
};
use loom_map::{LoomMap, ReplacementExtent};
use loom_types::{Sector, SectorCount};
use std::fs;
use std::path::Path;

const RO_COMPAT_METADATA_CSUM: u32 = 0x0400;
const INCOMPAT_CSUM_SEED: u32 = 0x2000;
const EXT4_OS_LINUX: u32 = 0;
const EXT4_CRC32C_CHKSUM: u8 = 1;
const EXT4_EXTENT_MAGIC: u16 = 0xF30A;
const EXT4_BG_BLOCK_UNINIT: u16 = 0x0002;
const INODE_HUGE_FILE_FL: u32 = 0x0004_0000;

const SUPER_FREE_BLOCKS_LO: usize = 0x0C;
const SUPER_FIRST_DATA_BLOCK: usize = 0x14;
const SUPER_BLOCKS_PER_GROUP: usize = 0x20;
const SUPER_CLUSTERS_PER_GROUP: usize = 0x24;
const SUPER_FEATURE_INCOMPAT: usize = 0x60;
const SUPER_FEATURE_RO_COMPAT: usize = 0x64;
const SUPER_UUID: usize = 0x68;
const SUPER_DESC_SIZE: usize = 0xFE;
const SUPER_FREE_BLOCKS_HI: usize = 0x158;
const SUPER_CHECKSUM_TYPE: usize = 0x175;
const SUPER_CHECKSUM_SEED: usize = 0x270;
const SUPER_CHECKSUM: usize = 0x3FC;

const GD_BLOCK_BITMAP_LO: usize = 0x00;
const GD_FREE_BLOCKS_LO: usize = 0x0C;
const GD_FLAGS: usize = 0x12;
const GD_BLOCK_BITMAP_CSUM_LO: usize = 0x18;
const GD_CHECKSUM: usize = 0x1E;
const GD_BLOCK_BITMAP_HI: usize = 0x20;
const GD_FREE_BLOCKS_HI: usize = 0x2C;
const GD_BLOCK_BITMAP_CSUM_HI: usize = 0x38;
const GD_BLOCK_BITMAP_CSUM_HI_END: usize = 0x3A;

const INODE_BLOCKS_LO: usize = 0x1C;
const INODE_BLOCK_ROOT: usize = 0x28;
const INODE_BLOCK_ROOT_LEN: usize = 60;
const INODE_SIZE_LO: usize = 0x04;
const INODE_SIZE_HIGH: usize = 0x6C;
const INODE_BLOCKS_HIGH: usize = 0x74;

#[derive(Debug)]
pub struct CompiledGrowth {
    pub map: LoomMap,
    pub shadow: Vec<u8>,
    pub block_size: u32,
    pub inode: u32,
    pub original_data_blocks: usize,
    pub effective_data_blocks: usize,
    pub new_data_blocks: usize,
    pub existing_data_shadow_blocks: usize,
    pub inode_metadata_blocks: usize,
    pub allocator_metadata_blocks: usize,
    pub shadow_blocks: usize,
    pub allocated_block: u64,
}

#[derive(Debug, Clone, Copy)]
struct AllocationGeometry {
    first_data_block: u64,
    blocks_per_group: u32,
    clusters_per_group: u32,
    descriptor_size: u16,
    has_64bit: bool,
    checksum_seed: u32,
}

#[derive(Debug, Clone, Copy)]
struct GroupDescriptorLocation {
    group: u32,
    descriptor_block: u64,
    descriptor_offset: usize,
    block_bitmap: u64,
    free_blocks: u32,
    flags: u16,
}

struct InodeGrowthPlan<'a> {
    inode_number: u32,
    inode: &'a Inode,
    old_block: u64,
    new_block: u64,
    checksum_seed: u32,
    sectors_per_block: u64,
    effective_size: u64,
}

/// Grows a one-block ext4 regular file by exactly one newly allocated data block.
///
/// Stage 3 keeps the pre-existing data block byte-for-byte identical and mutates
/// only the minimal allocator closure required for one additional block.
///
/// # Errors
/// Returns [`Ext4Error`] when the target or filesystem falls outside the narrow
/// Stage 3 allocator subset, metadata is malformed, no suitable free block is
/// available in the existing data block's group, or checksum/mapping I/O fails.
pub fn compile_one_block_growth(
    origin_path: &Path,
    target_path: &str,
    replacement_path: &Path,
) -> Result<CompiledGrowth, Ext4Error> {
    let replacement = fs::read(replacement_path).map_err(Ext4Error::Io)?;
    let mut image = Ext4Image::open(origin_path)?;
    let inode_number = image.resolve_path(target_path)?;
    image.compile_one_block_growth(inode_number, &replacement)
}

impl Ext4Image {
    fn compile_one_block_growth(
        &mut self,
        inode_number: u32,
        replacement: &[u8],
    ) -> Result<CompiledGrowth, Ext4Error> {
        let inode = self.read_inode(inode_number)?;
        validate_allocator_inode(inode_number, &inode)?;
        let blocks = self.file_blocks(&inode)?;
        if blocks.len() != 1 || inode.size != u64::from(self.superblock.block_size) {
            return Err(Ext4Error::UnsupportedFilesystemFeature(
                "Stage 3 allocator requires a one-block source file",
            ));
        }

        let block_size = usize::try_from(self.superblock.block_size)
            .map_err(|_| Ext4Error::ArithmeticOverflow)?;
        if replacement.len() != block_size.saturating_mul(2) {
            return Err(Ext4Error::UnsupportedFilesystemFeature(
                "Stage 3 allocator requires exactly one new data block",
            ));
        }

        let origin_data = self.read_block(blocks[0])?;
        if replacement.get(..block_size) != Some(origin_data.as_slice()) {
            return Err(Ext4Error::UnsupportedFilesystemFeature(
                "Stage 3 allocator requires existing data blocks to remain unchanged",
            ));
        }

        let geometry = self.allocation_geometry()?;
        let group = block_group_for(
            blocks[0],
            geometry.first_data_block,
            geometry.blocks_per_group,
        )?;
        let descriptor = self.group_descriptor_location(group, geometry)?;
        if descriptor.flags & EXT4_BG_BLOCK_UNINIT != 0 {
            return Err(Ext4Error::UnsupportedFilesystemFeature(
                "Stage 3 refuses an uninitialized block bitmap",
            ));
        }
        if descriptor.free_blocks == 0 {
            return Err(Ext4Error::UnsupportedFilesystemFeature(
                "target block group has no free blocks",
            ));
        }

        let mut bitmap_shadow = self.read_block(descriptor.block_bitmap)?;
        let allocated_block = find_and_set_free_block(
            &mut bitmap_shadow,
            group,
            geometry.first_data_block,
            geometry.blocks_per_group,
            self.superblock.blocks_count,
        )?;

        let sectors_per_block = u64::from(self.superblock.block_size) / SECTOR_SIZE;
        let mut shadow = Vec::new();
        let mut replacements = Vec::with_capacity(5);

        let new_data = replacement
            .get(block_size..)
            .ok_or(Ext4Error::UnexpectedEndOfStructure)?;
        append_shadow_block(
            &mut shadow,
            &mut replacements,
            allocated_block,
            sectors_per_block,
            new_data,
        )?;

        let inode_plan = InodeGrowthPlan {
            inode_number,
            inode: &inode,
            old_block: blocks[0],
            new_block: allocated_block,
            checksum_seed: geometry.checksum_seed,
            sectors_per_block,
            effective_size: u64::from(self.superblock.block_size)
                .checked_mul(2)
                .ok_or(Ext4Error::ArithmeticOverflow)?,
        };
        self.append_growth_inode_shadow(&inode_plan, &mut shadow, &mut replacements)?;

        self.update_block_bitmap_checksum_and_descriptor(
            descriptor,
            geometry,
            &bitmap_shadow,
            sectors_per_block,
            &mut shadow,
            &mut replacements,
        )?;

        self.append_superblock_allocator_shadow(
            geometry,
            sectors_per_block,
            &mut shadow,
            &mut replacements,
        )?;

        let total_sectors = self.image_bytes / SECTOR_SIZE;
        let map = LoomMap::from_replacements(SectorCount(total_sectors), &replacements)
            .map_err(Ext4Error::Map)?;

        Ok(build_compiled_growth(
            map,
            shadow,
            self.superblock.block_size,
            inode_number,
            allocated_block,
        ))
    }

    fn allocation_geometry(&mut self) -> Result<AllocationGeometry, Ext4Error> {
        let mut bytes = [0_u8; SUPERBLOCK_SIZE];
        read_exact_at(&mut self.file, SUPERBLOCK_OFFSET, &mut bytes)?;
        let feature_incompat = read_u32(&bytes, SUPER_FEATURE_INCOMPAT)?;
        let feature_ro_compat = read_u32(&bytes, SUPER_FEATURE_RO_COMPAT)?;
        if feature_ro_compat & RO_COMPAT_METADATA_CSUM == 0 {
            return Err(Ext4Error::UnsupportedFilesystemFeature(
                "Stage 3 requires metadata_csum",
            ));
        }
        if bytes[SUPER_CHECKSUM_TYPE] != EXT4_CRC32C_CHKSUM {
            return Err(Ext4Error::UnsupportedFilesystemFeature(
                "Stage 3 requires CRC32C metadata checksums",
            ));
        }
        if read_u32(&bytes, 0x48)? != EXT4_OS_LINUX {
            return Err(Ext4Error::UnsupportedFilesystemFeature(
                "Stage 3 requires a Linux-created ext4 filesystem",
            ));
        }

        let blocks_per_group = read_u32(&bytes, SUPER_BLOCKS_PER_GROUP)?;
        let clusters_per_group = read_u32(&bytes, SUPER_CLUSTERS_PER_GROUP)?;
        if blocks_per_group == 0 || clusters_per_group != blocks_per_group {
            return Err(Ext4Error::UnsupportedFilesystemFeature(
                "Stage 3 requires non-bigalloc block groups",
            ));
        }

        let has_64bit = self.superblock.has_64bit;
        let descriptor_size = if has_64bit {
            read_u16(&bytes, SUPER_DESC_SIZE)?
        } else {
            32
        };
        let checksum_seed = if feature_incompat & INCOMPAT_CSUM_SEED != 0 {
            read_u32(&bytes, SUPER_CHECKSUM_SEED)?
        } else {
            let uuid = bytes
                .get(SUPER_UUID..SUPER_UUID + 16)
                .ok_or(Ext4Error::UnexpectedEndOfStructure)?;
            crc32c(u32::MAX, uuid)
        };

        Ok(AllocationGeometry {
            first_data_block: u64::from(read_u32(&bytes, SUPER_FIRST_DATA_BLOCK)?),
            blocks_per_group,
            clusters_per_group,
            descriptor_size,
            has_64bit,
            checksum_seed,
        })
    }

    fn group_descriptor_location(
        &mut self,
        group: u32,
        geometry: AllocationGeometry,
    ) -> Result<GroupDescriptorLocation, Ext4Error> {
        let descriptor_start_block = if self.superblock.block_size == 1024 {
            2_u64
        } else {
            1_u64
        };
        let descriptor_size = usize::from(geometry.descriptor_size);
        let byte_offset = usize::try_from(group)
            .map_err(|_| Ext4Error::ArithmeticOverflow)?
            .checked_mul(descriptor_size)
            .ok_or(Ext4Error::ArithmeticOverflow)?;
        let block_size = usize::try_from(self.superblock.block_size)
            .map_err(|_| Ext4Error::ArithmeticOverflow)?;
        let block_delta = byte_offset / block_size;
        let descriptor_offset = byte_offset % block_size;
        if descriptor_offset
            .checked_add(descriptor_size)
            .ok_or(Ext4Error::ArithmeticOverflow)?
            > block_size
        {
            return Err(Ext4Error::UnsupportedFilesystemFeature(
                "group descriptor crosses a filesystem-block boundary",
            ));
        }

        let descriptor_block = descriptor_start_block
            .checked_add(u64::try_from(block_delta).map_err(|_| Ext4Error::ArithmeticOverflow)?)
            .ok_or(Ext4Error::ArithmeticOverflow)?;
        let block = self.read_block(descriptor_block)?;
        let descriptor = block
            .get(descriptor_offset..descriptor_offset + descriptor_size)
            .ok_or(Ext4Error::UnexpectedEndOfStructure)?;

        let bitmap_lo = u64::from(read_u32(descriptor, GD_BLOCK_BITMAP_LO)?);
        let bitmap_hi = if geometry.has_64bit {
            u64::from(read_u32(descriptor, GD_BLOCK_BITMAP_HI)?)
        } else {
            0
        };
        let free_lo = u32::from(read_u16(descriptor, GD_FREE_BLOCKS_LO)?);
        let free_hi = if geometry.has_64bit {
            u32::from(read_u16(descriptor, GD_FREE_BLOCKS_HI)?)
        } else {
            0
        };

        Ok(GroupDescriptorLocation {
            group,
            descriptor_block,
            descriptor_offset,
            block_bitmap: (bitmap_hi << 32) | bitmap_lo,
            free_blocks: (free_hi << 16) | free_lo,
            flags: read_u16(descriptor, GD_FLAGS)?,
        })
    }

    fn append_growth_inode_shadow(
        &mut self,
        plan: &InodeGrowthPlan<'_>,
        shadow: &mut Vec<u8>,
        replacements: &mut Vec<ReplacementExtent>,
    ) -> Result<(), Ext4Error> {
        let (inode_table_block, inode_offset) =
            self.inode_record_location_stage3(plan.inode_number)?;
        let mut inode_table_shadow = self.read_block(inode_table_block)?;
        let inode_size = usize::from(self.superblock.inode_size);
        let inode_end = inode_offset
            .checked_add(inode_size)
            .ok_or(Ext4Error::ArithmeticOverflow)?;
        let raw_inode = inode_table_shadow
            .get_mut(inode_offset..inode_end)
            .ok_or(Ext4Error::UnexpectedEndOfStructure)?;

        update_extent_root_for_one_block(
            raw_inode,
            &plan.inode.block,
            plan.old_block,
            plan.new_block,
        )?;
        write_u64_split(
            raw_inode,
            INODE_SIZE_LO,
            INODE_SIZE_HIGH,
            plan.effective_size,
        )?;
        increment_inode_blocks(raw_inode, plan.sectors_per_block)?;
        rewrite_inode_checksum(raw_inode, plan.checksum_seed, plan.inode_number)
            .map_err(Ext4Error::Checksum)?;

        append_shadow_block(
            shadow,
            replacements,
            inode_table_block,
            plan.sectors_per_block,
            &inode_table_shadow,
        )
    }

    fn inode_record_location_stage3(
        &mut self,
        inode_number: u32,
    ) -> Result<(u64, usize), Ext4Error> {
        if inode_number == 0 || inode_number > self.superblock.inodes_count {
            return Err(Ext4Error::InvalidInode(inode_number));
        }
        let zero_based = inode_number - 1;
        let group = zero_based / self.superblock.inodes_per_group;
        let index = zero_based % self.superblock.inodes_per_group;
        let inode_table_start = self.inode_table_block(group)?;
        let inode_byte_offset = u64::from(index)
            .checked_mul(u64::from(self.superblock.inode_size))
            .ok_or(Ext4Error::ArithmeticOverflow)?;
        let block_size = u64::from(self.superblock.block_size);
        let table_block = inode_table_start
            .checked_add(inode_byte_offset / block_size)
            .ok_or(Ext4Error::ArithmeticOverflow)?;
        let offset = usize::try_from(inode_byte_offset % block_size)
            .map_err(|_| Ext4Error::ArithmeticOverflow)?;
        let end = offset
            .checked_add(usize::from(self.superblock.inode_size))
            .ok_or(Ext4Error::ArithmeticOverflow)?;
        if end > usize::try_from(block_size).map_err(|_| Ext4Error::ArithmeticOverflow)? {
            return Err(Ext4Error::InvalidFilesystem(
                "inode record crosses inode-table filesystem block",
            ));
        }
        Ok((table_block, offset))
    }

    fn update_block_bitmap_checksum_and_descriptor(
        &mut self,
        location: GroupDescriptorLocation,
        geometry: AllocationGeometry,
        bitmap_shadow: &[u8],
        sectors_per_block: u64,
        shadow: &mut Vec<u8>,
        replacements: &mut Vec<ReplacementExtent>,
    ) -> Result<(), Ext4Error> {
        append_shadow_block(
            shadow,
            replacements,
            location.block_bitmap,
            sectors_per_block,
            bitmap_shadow,
        )?;

        let mut descriptor_block = self.read_block(location.descriptor_block)?;
        let descriptor_size = usize::from(geometry.descriptor_size);
        let descriptor_end = location
            .descriptor_offset
            .checked_add(descriptor_size)
            .ok_or(Ext4Error::ArithmeticOverflow)?;
        let descriptor = descriptor_block
            .get_mut(location.descriptor_offset..descriptor_end)
            .ok_or(Ext4Error::UnexpectedEndOfStructure)?;

        decrement_group_free_blocks(descriptor, geometry.has_64bit)?;
        let bitmap_bytes = usize::try_from(geometry.clusters_per_group / 8)
            .map_err(|_| Ext4Error::ArithmeticOverflow)?;
        let bitmap_for_checksum = bitmap_shadow
            .get(..bitmap_bytes)
            .ok_or(Ext4Error::UnexpectedEndOfStructure)?;
        let bitmap_checksum = crc32c(geometry.checksum_seed, bitmap_for_checksum);
        write_checksum_low_high(
            descriptor,
            GD_BLOCK_BITMAP_CSUM_LO,
            GD_BLOCK_BITMAP_CSUM_HI,
            usize::from(geometry.descriptor_size) >= GD_BLOCK_BITMAP_CSUM_HI_END,
            bitmap_checksum,
        )?;
        rewrite_group_descriptor_checksum(descriptor, location.group, geometry.checksum_seed)?;

        append_shadow_block(
            shadow,
            replacements,
            location.descriptor_block,
            sectors_per_block,
            &descriptor_block,
        )
    }

    fn append_superblock_allocator_shadow(
        &mut self,
        geometry: AllocationGeometry,
        sectors_per_block: u64,
        shadow: &mut Vec<u8>,
        replacements: &mut Vec<ReplacementExtent>,
    ) -> Result<(), Ext4Error> {
        let block_size = u64::from(self.superblock.block_size);
        let super_block = SUPERBLOCK_OFFSET / block_size;
        let super_offset = usize::try_from(SUPERBLOCK_OFFSET % block_size)
            .map_err(|_| Ext4Error::ArithmeticOverflow)?;
        let mut block = self.read_block(super_block)?;
        let super_end = super_offset
            .checked_add(SUPERBLOCK_SIZE)
            .ok_or(Ext4Error::ArithmeticOverflow)?;
        let raw_super = block
            .get_mut(super_offset..super_end)
            .ok_or(Ext4Error::UnexpectedEndOfStructure)?;

        decrement_super_free_blocks(raw_super, geometry.has_64bit)?;
        let checksum = crc32c(u32::MAX, &raw_super[..SUPER_CHECKSUM]);
        write_u32(raw_super, SUPER_CHECKSUM, checksum)?;

        append_shadow_block(shadow, replacements, super_block, sectors_per_block, &block)
    }
}

fn build_compiled_growth(
    map: LoomMap,
    shadow: Vec<u8>,
    block_size: u32,
    inode: u32,
    allocated_block: u64,
) -> CompiledGrowth {
    CompiledGrowth {
        map,
        shadow,
        block_size,
        inode,
        original_data_blocks: 1,
        effective_data_blocks: 2,
        new_data_blocks: 1,
        existing_data_shadow_blocks: 0,
        inode_metadata_blocks: 1,
        allocator_metadata_blocks: 3,
        shadow_blocks: 5,
        allocated_block,
    }
}

fn validate_allocator_inode(inode_number: u32, inode: &Inode) -> Result<(), Ext4Error> {
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
    if inode.flags & INODE_HUGE_FILE_FL != 0 {
        return Err(Ext4Error::UnsupportedInodeFeature(
            "huge-file i_blocks encoding",
        ));
    }
    Ok(())
}

fn block_group_for(
    block: u64,
    first_data_block: u64,
    blocks_per_group: u32,
) -> Result<u32, Ext4Error> {
    let relative = block
        .checked_sub(first_data_block)
        .ok_or(Ext4Error::InvalidFilesystem(
            "data block precedes first_data_block",
        ))?;
    u32::try_from(relative / u64::from(blocks_per_group)).map_err(|_| Ext4Error::ArithmeticOverflow)
}

fn find_and_set_free_block(
    bitmap: &mut [u8],
    group: u32,
    first_data_block: u64,
    blocks_per_group: u32,
    blocks_count: u64,
) -> Result<u64, Ext4Error> {
    let group_start = first_data_block
        .checked_add(
            u64::from(group)
                .checked_mul(u64::from(blocks_per_group))
                .ok_or(Ext4Error::ArithmeticOverflow)?,
        )
        .ok_or(Ext4Error::ArithmeticOverflow)?;
    let remaining = blocks_count.saturating_sub(group_start);
    let valid_blocks = remaining.min(u64::from(blocks_per_group));

    for bit in 0..valid_blocks {
        let byte_index = usize::try_from(bit / 8).map_err(|_| Ext4Error::ArithmeticOverflow)?;
        let bit_index = u8::try_from(bit % 8).map_err(|_| Ext4Error::ArithmeticOverflow)?;
        let byte = bitmap
            .get_mut(byte_index)
            .ok_or(Ext4Error::UnexpectedEndOfStructure)?;
        let mask = 1_u8 << bit_index;
        if *byte & mask == 0 {
            *byte |= mask;
            return group_start
                .checked_add(bit)
                .ok_or(Ext4Error::ArithmeticOverflow);
        }
    }

    Err(Ext4Error::UnsupportedFilesystemFeature(
        "target block group has no discoverable free block",
    ))
}

fn update_extent_root_for_one_block(
    raw_inode: &mut [u8],
    parsed_root: &[u8; INODE_BLOCK_ROOT_LEN],
    old_block: u64,
    new_block: u64,
) -> Result<(), Ext4Error> {
    let root_end = INODE_BLOCK_ROOT
        .checked_add(INODE_BLOCK_ROOT_LEN)
        .ok_or(Ext4Error::ArithmeticOverflow)?;
    let root = raw_inode
        .get_mut(INODE_BLOCK_ROOT..root_end)
        .ok_or(Ext4Error::UnexpectedEndOfStructure)?;
    if root != parsed_root {
        return Err(Ext4Error::InvalidFilesystem(
            "inode extent root changed during compilation",
        ));
    }
    if read_u16(root, 0)? != EXT4_EXTENT_MAGIC || read_u16(root, 6)? != 0 || read_u16(root, 2)? != 1
    {
        return Err(Ext4Error::UnsupportedInodeFeature(
            "Stage 3 requires one depth-0 extent",
        ));
    }
    let maximum = read_u16(root, 4)?;
    if maximum < 2 {
        return Err(Ext4Error::UnsupportedInodeFeature(
            "inode extent root has no room for one-block growth",
        ));
    }

    let first = root
        .get(12..24)
        .ok_or(Ext4Error::UnexpectedEndOfStructure)?;
    let logical = read_u32(first, 0)?;
    let length = read_u16(first, 4)?;
    let physical = (u64::from(read_u16(first, 6)?) << 32) | u64::from(read_u32(first, 8)?);
    if logical != 0 || length != 1 || physical != old_block {
        return Err(Ext4Error::UnsupportedInodeFeature(
            "Stage 3 source extent is not a single logical block",
        ));
    }

    if new_block == old_block.saturating_add(1) {
        write_u16(root, 16, 2)?;
        return Ok(());
    }

    write_u16(root, 2, 2)?;
    let second = root
        .get_mut(24..36)
        .ok_or(Ext4Error::UnexpectedEndOfStructure)?;
    second.fill(0);
    write_u32(second, 0, 1)?;
    write_u16(second, 4, 1)?;
    let physical_bytes = new_block.to_le_bytes();
    write_u16(
        second,
        6,
        u16::from_le_bytes([physical_bytes[4], physical_bytes[5]]),
    )?;
    write_u32(
        second,
        8,
        u32::from_le_bytes([
            physical_bytes[0],
            physical_bytes[1],
            physical_bytes[2],
            physical_bytes[3],
        ]),
    )
}

fn increment_inode_blocks(raw_inode: &mut [u8], sectors: u64) -> Result<(), Ext4Error> {
    let low = u64::from(read_u32(raw_inode, INODE_BLOCKS_LO)?);
    let high = u64::from(read_u16(raw_inode, INODE_BLOCKS_HIGH)?);
    let current = (high << 32) | low;
    let updated = current
        .checked_add(sectors)
        .ok_or(Ext4Error::ArithmeticOverflow)?;
    if updated >> 48 != 0 {
        return Err(Ext4Error::UnsupportedInodeFeature(
            "i_blocks exceeds 48-bit Stage 3 encoding",
        ));
    }
    let bytes = updated.to_le_bytes();
    write_u32(
        raw_inode,
        INODE_BLOCKS_LO,
        u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
    )?;
    write_u16(
        raw_inode,
        INODE_BLOCKS_HIGH,
        u16::from_le_bytes([bytes[4], bytes[5]]),
    )
}

fn decrement_group_free_blocks(descriptor: &mut [u8], has_64bit: bool) -> Result<(), Ext4Error> {
    let low = u32::from(read_u16(descriptor, GD_FREE_BLOCKS_LO)?);
    let high = if has_64bit {
        u32::from(read_u16(descriptor, GD_FREE_BLOCKS_HI)?)
    } else {
        0
    };
    let current = (high << 16) | low;
    let updated = current.checked_sub(1).ok_or(Ext4Error::InvalidFilesystem(
        "group free-block count underflow",
    ))?;
    let bytes = updated.to_le_bytes();
    write_u16(
        descriptor,
        GD_FREE_BLOCKS_LO,
        u16::from_le_bytes([bytes[0], bytes[1]]),
    )?;
    if has_64bit {
        write_u16(
            descriptor,
            GD_FREE_BLOCKS_HI,
            u16::from_le_bytes([bytes[2], bytes[3]]),
        )?;
    }
    Ok(())
}

fn decrement_super_free_blocks(raw_super: &mut [u8], has_64bit: bool) -> Result<(), Ext4Error> {
    let low = u64::from(read_u32(raw_super, SUPER_FREE_BLOCKS_LO)?);
    let high = if has_64bit {
        u64::from(read_u32(raw_super, SUPER_FREE_BLOCKS_HI)?)
    } else {
        0
    };
    let current = (high << 32) | low;
    let updated = current.checked_sub(1).ok_or(Ext4Error::InvalidFilesystem(
        "superblock free-block count underflow",
    ))?;
    let bytes = updated.to_le_bytes();
    write_u32(
        raw_super,
        SUPER_FREE_BLOCKS_LO,
        u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
    )?;
    if has_64bit {
        write_u32(
            raw_super,
            SUPER_FREE_BLOCKS_HI,
            u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
        )?;
    }
    Ok(())
}

fn rewrite_group_descriptor_checksum(
    descriptor: &mut [u8],
    group: u32,
    seed: u32,
) -> Result<(), Ext4Error> {
    write_u16(descriptor, GD_CHECKSUM, 0)?;
    let crc = crc32c(seed, &group.to_le_bytes());
    let crc = crc32c(crc, descriptor);
    let bytes = crc.to_le_bytes();
    write_u16(
        descriptor,
        GD_CHECKSUM,
        u16::from_le_bytes([bytes[0], bytes[1]]),
    )
}

fn write_checksum_low_high(
    target: &mut [u8],
    low_offset: usize,
    high_offset: usize,
    has_high: bool,
    checksum: u32,
) -> Result<(), Ext4Error> {
    let bytes = checksum.to_le_bytes();
    write_u16(target, low_offset, u16::from_le_bytes([bytes[0], bytes[1]]))?;
    if has_high {
        write_u16(
            target,
            high_offset,
            u16::from_le_bytes([bytes[2], bytes[3]]),
        )?;
    }
    Ok(())
}

fn write_u64_split(
    target: &mut [u8],
    low_offset: usize,
    high_offset: usize,
    value: u64,
) -> Result<(), Ext4Error> {
    let bytes = value.to_le_bytes();
    write_u32(
        target,
        low_offset,
        u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
    )?;
    write_u32(
        target,
        high_offset,
        u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
    )
}

fn append_shadow_block(
    shadow: &mut Vec<u8>,
    replacements: &mut Vec<ReplacementExtent>,
    physical_block: u64,
    sectors_per_block: u64,
    block: &[u8],
) -> Result<(), Ext4Error> {
    if block.len()
        != usize::try_from(sectors_per_block * SECTOR_SIZE)
            .map_err(|_| Ext4Error::ArithmeticOverflow)?
    {
        return Err(Ext4Error::InvalidFilesystem(
            "shadow block length differs from filesystem block size",
        ));
    }
    let shadow_bytes = u64::try_from(shadow.len()).map_err(|_| Ext4Error::ArithmeticOverflow)?;
    if shadow_bytes % SECTOR_SIZE != 0 {
        return Err(Ext4Error::InvalidFilesystem(
            "shadow pack lost sector alignment",
        ));
    }
    let shadow_start = shadow_bytes / SECTOR_SIZE;
    let logical_start = physical_block
        .checked_mul(sectors_per_block)
        .ok_or(Ext4Error::ArithmeticOverflow)?;
    replacements.push(ReplacementExtent {
        logical_start: Sector(logical_start),
        sector_count: SectorCount(sectors_per_block),
        shadow_start: Sector(shadow_start),
    });
    shadow.extend_from_slice(block);
    Ok(())
}

fn write_u16(target: &mut [u8], offset: usize, value: u16) -> Result<(), Ext4Error> {
    let end = offset.checked_add(2).ok_or(Ext4Error::ArithmeticOverflow)?;
    target
        .get_mut(offset..end)
        .ok_or(Ext4Error::UnexpectedEndOfStructure)?
        .copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn write_u32(target: &mut [u8], offset: usize, value: u32) -> Result<(), Ext4Error> {
    let end = offset.checked_add(4).ok_or(Ext4Error::ArithmeticOverflow)?;
    target
        .get_mut(offset..end)
        .ok_or(Ext4Error::UnexpectedEndOfStructure)?
        .copy_from_slice(&value.to_le_bytes());
    Ok(())
}

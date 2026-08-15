#![forbid(unsafe_code)]

use super::checksum::{crc32c, rewrite_inode_checksum};
use super::{
    read_exact_at, read_u32, Ext4Error, Ext4Image, Inode, INODE_INLINE_DATA_FL, INODE_VERITY_FL,
    MODE_REGULAR, SECTOR_SIZE, SUPERBLOCK_OFFSET, SUPERBLOCK_SIZE,
};
use loom_map::{LoomMap, ReplacementExtent};
use loom_types::{Sector, SectorCount};
use std::fs;
use std::path::Path;

const RO_COMPAT_METADATA_CSUM: u32 = 0x0400;
const INCOMPAT_CSUM_SEED: u32 = 0x2000;
const EXT4_OS_LINUX: u32 = 0;
const EXT4_CRC32C_CHKSUM: u8 = 1;
const INODE_SIZE_LO_OFFSET: usize = 0x04;
const INODE_SIZE_HIGH_OFFSET: usize = 0x6C;
const UUID_OFFSET: usize = 0x68;
const UUID_SIZE: usize = 16;
const CHECKSUM_TYPE_OFFSET: usize = 0x175;
const CHECKSUM_SEED_OFFSET: usize = 0x270;

#[derive(Debug)]
pub struct CompiledResize {
    pub map: LoomMap,
    pub shadow: Vec<u8>,
    pub block_size: u32,
    pub inode: u32,
    pub data_blocks: usize,
    pub data_shadow_blocks: usize,
    pub metadata_blocks: usize,
    pub shadow_blocks: usize,
    pub original_size: u64,
    pub effective_size: u64,
}

struct ResizeTarget {
    inode: Inode,
    blocks: Vec<u64>,
    effective_size: u64,
    block_size: usize,
    sectors_per_block: u64,
}

/// Compiles an ext4 regular-file resize without changing allocator state.
///
/// The effective size must remain inside the exact same number of already
/// allocated, dense logical data blocks. Stage 2 therefore changes only file
/// data blocks that differ and the inode-table block containing the inode.
///
/// # Errors
/// Returns [`Ext4Error`] for malformed/unsupported ext4 structures, unsafe
/// inode semantics, allocation-boundary changes, checksum failures, mapping
/// failures, or filesystem I/O errors.
pub fn compile_resize_within_allocation(
    origin_path: &Path,
    target_path: &str,
    replacement_path: &Path,
) -> Result<CompiledResize, Ext4Error> {
    let replacement = fs::read(replacement_path).map_err(Ext4Error::Io)?;
    let mut image = Ext4Image::open(origin_path)?;
    let inode_number = image.resolve_path(target_path)?;
    image.compile_resize(inode_number, &replacement)
}

impl Ext4Image {
    pub(crate) fn compile_resize(
        &mut self,
        inode_number: u32,
        replacement: &[u8],
    ) -> Result<CompiledResize, Ext4Error> {
        let target = self.prepare_resize_target(inode_number, replacement.len())?;
        let (mut shadow, mut replacements, data_shadow_blocks) = self.compile_resize_data(
            &target.blocks,
            replacement,
            target.block_size,
            target.sectors_per_block,
        )?;

        self.append_inode_metadata_shadow(
            inode_number,
            target.effective_size,
            target.sectors_per_block,
            &mut shadow,
            &mut replacements,
        )?;

        let total_sectors = self.image_bytes / SECTOR_SIZE;
        let map = LoomMap::from_replacements(SectorCount(total_sectors), &replacements)
            .map_err(Ext4Error::Map)?;
        let shadow_blocks = data_shadow_blocks
            .checked_add(1)
            .ok_or(Ext4Error::ArithmeticOverflow)?;

        Ok(CompiledResize {
            map,
            shadow,
            block_size: self.superblock.block_size,
            inode: inode_number,
            data_blocks: target.blocks.len(),
            data_shadow_blocks,
            metadata_blocks: 1,
            shadow_blocks,
            original_size: target.inode.size,
            effective_size: target.effective_size,
        })
    }

    fn prepare_resize_target(
        &mut self,
        inode_number: u32,
        replacement_len: usize,
    ) -> Result<ResizeTarget, Ext4Error> {
        let inode = self.read_inode(inode_number)?;
        validate_resize_inode(inode_number, &inode)?;

        let effective_size =
            u64::try_from(replacement_len).map_err(|_| Ext4Error::ArithmeticOverflow)?;
        if effective_size == inode.size {
            return Err(Ext4Error::ResizeSizeUnchanged(inode.size));
        }

        let blocks = self.file_blocks(&inode)?;
        let block_size_u64 = u64::from(self.superblock.block_size);
        let effective_blocks = blocks_for_size(effective_size, block_size_u64)?;
        let existing_blocks =
            u64::try_from(blocks.len()).map_err(|_| Ext4Error::ArithmeticOverflow)?;
        if effective_blocks != existing_blocks || effective_blocks == 0 {
            return Err(Ext4Error::ResizeCrossesAllocationBoundary {
                original_size: inode.size,
                effective_size,
                allocated_blocks: existing_blocks,
                required_blocks: effective_blocks,
            });
        }

        Ok(ResizeTarget {
            inode,
            blocks,
            effective_size,
            block_size: usize::try_from(self.superblock.block_size)
                .map_err(|_| Ext4Error::ArithmeticOverflow)?,
            sectors_per_block: block_size_u64 / SECTOR_SIZE,
        })
    }

    fn compile_resize_data(
        &mut self,
        blocks: &[u64],
        replacement: &[u8],
        block_size: usize,
        sectors_per_block: u64,
    ) -> Result<(Vec<u8>, Vec<ReplacementExtent>, usize), Ext4Error> {
        let mut shadow = Vec::new();
        let mut replacements = Vec::with_capacity(blocks.len().saturating_add(1));
        let mut changed_blocks = 0_usize;

        for (file_block_index, physical_block) in blocks.iter().copied().enumerate() {
            let origin_block = self.read_block(physical_block)?;
            let effective_block =
                materialize_file_block(&origin_block, replacement, file_block_index, block_size)?;
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
            changed_blocks = changed_blocks
                .checked_add(1)
                .ok_or(Ext4Error::ArithmeticOverflow)?;
        }

        Ok((shadow, replacements, changed_blocks))
    }

    fn append_inode_metadata_shadow(
        &mut self,
        inode_number: u32,
        effective_size: u64,
        sectors_per_block: u64,
        shadow: &mut Vec<u8>,
        replacements: &mut Vec<ReplacementExtent>,
    ) -> Result<(), Ext4Error> {
        let checksum_seed = self.filesystem_checksum_seed()?;
        let (inode_table_block, inode_offset_in_block) =
            self.inode_record_location(inode_number)?;
        let mut inode_table_shadow = self.read_block(inode_table_block)?;
        let inode_size = usize::from(self.superblock.inode_size);
        let inode_end = inode_offset_in_block
            .checked_add(inode_size)
            .ok_or(Ext4Error::ArithmeticOverflow)?;
        let raw_inode = inode_table_shadow
            .get_mut(inode_offset_in_block..inode_end)
            .ok_or(Ext4Error::InvalidFilesystem(
                "inode record crosses inode-table filesystem block",
            ))?;

        let size_bytes = effective_size.to_le_bytes();
        write_u32(
            raw_inode,
            INODE_SIZE_LO_OFFSET,
            u32::from_le_bytes([size_bytes[0], size_bytes[1], size_bytes[2], size_bytes[3]]),
        )?;
        write_u32(
            raw_inode,
            INODE_SIZE_HIGH_OFFSET,
            u32::from_le_bytes([size_bytes[4], size_bytes[5], size_bytes[6], size_bytes[7]]),
        )?;

        if let Some(fs_seed) = checksum_seed {
            rewrite_inode_checksum(raw_inode, fs_seed, inode_number)
                .map_err(Ext4Error::Checksum)?;
        }

        append_shadow_block(
            shadow,
            replacements,
            inode_table_block,
            sectors_per_block,
            &inode_table_shadow,
        )
    }

    fn inode_record_location(&mut self, inode_number: u32) -> Result<(u64, usize), Ext4Error> {
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
        let block_delta = inode_byte_offset / block_size;
        let offset_in_block_u64 = inode_byte_offset % block_size;
        let table_block = inode_table_start
            .checked_add(block_delta)
            .ok_or(Ext4Error::ArithmeticOverflow)?;
        if table_block >= self.superblock.blocks_count {
            return Err(Ext4Error::InvalidFilesystem(
                "inode record lies outside filesystem",
            ));
        }

        let offset_in_block =
            usize::try_from(offset_in_block_u64).map_err(|_| Ext4Error::ArithmeticOverflow)?;
        let inode_end = offset_in_block
            .checked_add(usize::from(self.superblock.inode_size))
            .ok_or(Ext4Error::ArithmeticOverflow)?;
        let block_size_usize =
            usize::try_from(block_size).map_err(|_| Ext4Error::ArithmeticOverflow)?;
        if inode_end > block_size_usize {
            return Err(Ext4Error::InvalidFilesystem(
                "inode record crosses inode-table filesystem block",
            ));
        }
        Ok((table_block, offset_in_block))
    }

    fn filesystem_checksum_seed(&mut self) -> Result<Option<u32>, Ext4Error> {
        let mut bytes = [0_u8; SUPERBLOCK_SIZE];
        read_exact_at(&mut self.file, SUPERBLOCK_OFFSET, &mut bytes)?;
        let feature_incompat = read_u32(&bytes, 0x60)?;
        let feature_ro_compat = read_u32(&bytes, 0x64)?;
        if feature_ro_compat & RO_COMPAT_METADATA_CSUM == 0 {
            return Ok(None);
        }

        let creator_os = read_u32(&bytes, 0x48)?;
        if creator_os != EXT4_OS_LINUX {
            return Err(Ext4Error::UnsupportedFilesystemFeature(
                "metadata_csum on non-Linux creator OS",
            ));
        }
        if bytes[CHECKSUM_TYPE_OFFSET] != EXT4_CRC32C_CHKSUM {
            return Err(Ext4Error::UnsupportedFilesystemFeature(
                "non-CRC32C metadata checksum algorithm",
            ));
        }

        if feature_incompat & INCOMPAT_CSUM_SEED != 0 {
            return Ok(Some(read_u32(&bytes, CHECKSUM_SEED_OFFSET)?));
        }

        let uuid_end = UUID_OFFSET
            .checked_add(UUID_SIZE)
            .ok_or(Ext4Error::ArithmeticOverflow)?;
        let uuid = bytes
            .get(UUID_OFFSET..uuid_end)
            .ok_or(Ext4Error::UnexpectedEndOfStructure)?;
        Ok(Some(crc32c(u32::MAX, uuid)))
    }
}

fn validate_resize_inode(inode_number: u32, inode: &Inode) -> Result<(), Ext4Error> {
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
    Ok(())
}

fn materialize_file_block(
    origin_block: &[u8],
    replacement: &[u8],
    file_block_index: usize,
    block_size: usize,
) -> Result<Vec<u8>, Ext4Error> {
    let mut effective_block = origin_block.to_vec();
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
    Ok(effective_block)
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

fn write_u32(bytes: &mut [u8], offset: usize, value: u32) -> Result<(), Ext4Error> {
    let end = offset.checked_add(4).ok_or(Ext4Error::ArithmeticOverflow)?;
    let target = bytes
        .get_mut(offset..end)
        .ok_or(Ext4Error::UnexpectedEndOfStructure)?;
    target.copy_from_slice(&value.to_le_bytes());
    Ok(())
}

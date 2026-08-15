#![forbid(unsafe_code)]

use super::checksum::{crc32c, rewrite_inode_checksum, verify_inode_checksum};
use super::{
    read_exact_at, read_u16, read_u32, Ext4Error, Ext4Image, INODE_INLINE_DATA_FL, INODE_VERITY_FL,
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
const XATTR_MAGIC: u32 = 0xea02_0000;
const XATTR_SECURITY_INDEX: u8 = 6;
const XATTR_ENTRY_FIXED: usize = 16;
const XATTR_END_MARKER: usize = 4;
const GOOD_OLD_INODE_SIZE: usize = 128;
const SELINUX_DISK_NAME: &[u8] = b"selinux";

const SB_CREATOR_OS: usize = 0x48;
const SB_FEATURE_INCOMPAT: usize = 0x60;
const SB_FEATURE_RO_COMPAT: usize = 0x64;
const SB_UUID: usize = 0x68;
const SB_UUID_SIZE: usize = 16;
const SB_CHECKSUM_TYPE: usize = 0x175;
const SB_CHECKSUM_SEED: usize = 0x270;

const INODE_FILE_ACL_LO: usize = 0x68;
const INODE_FILE_ACL_HI: usize = 0x76;
const INODE_EXTRA_ISIZE: usize = 0x80;

#[derive(Debug)]
pub struct CompiledSelinuxXattr {
    pub map: LoomMap,
    pub shadow: Vec<u8>,
    pub block_size: u32,
    pub inode: u32,
    pub value_bytes: usize,
    pub shadow_blocks: usize,
}

/// Adds `security.selinux` as an in-inode ext4 extended attribute.
///
/// Stage 6 deliberately requires an inode with no existing in-inode or external xattrs.
/// The value is read as raw bytes so callers control whether a trailing NUL is present.
/// Only the inode-table filesystem block is shadowed; allocator state is untouched.
///
/// # Errors
/// Returns [`Ext4Error`] for unsupported inode/xattr layouts, insufficient inode EA space,
/// checksum failures, malformed filesystem metadata, or origin I/O errors.
pub fn compile_selinux_xattr(
    origin_path: &Path,
    target_path: &str,
    value_path: &Path,
) -> Result<CompiledSelinuxXattr, Ext4Error> {
    let value = fs::read(value_path).map_err(Ext4Error::Io)?;
    if value.is_empty() {
        return Err(Ext4Error::UnsupportedInodeFeature(
            "Stage 6 requires a non-empty security.selinux value",
        ));
    }
    let mut image = Ext4Image::open(origin_path)?;
    let inode_number = image.resolve_path(target_path)?;
    let inode = image.read_inode(inode_number)?;
    if inode.file_type() != MODE_REGULAR {
        return Err(Ext4Error::NotRegularFile(inode_number));
    }
    if inode.flags & INODE_INLINE_DATA_FL != 0 {
        return Err(Ext4Error::UnsupportedInodeFeature("inline data"));
    }
    if inode.flags & INODE_VERITY_FL != 0 {
        return Err(Ext4Error::UnsupportedInodeFeature("fs-verity"));
    }

    let checksum_seed = image.read_stage6_checksum_seed()?;
    let (table_block, inode_offset) = image.inode_record_location_stage6(inode_number)?;
    let mut table_shadow = image.read_block(table_block)?;
    let inode_size = usize::from(image.superblock.inode_size);
    let inode_end = inode_offset
        .checked_add(inode_size)
        .ok_or(Ext4Error::ArithmeticOverflow)?;
    let raw_inode =
        table_shadow
            .get_mut(inode_offset..inode_end)
            .ok_or(Ext4Error::InvalidFilesystem(
                "inode record crosses inode-table filesystem block",
            ))?;

    verify_inode_checksum(raw_inode, checksum_seed, inode_number).map_err(Ext4Error::Checksum)?;
    reject_external_xattr(raw_inode, image.superblock.has_64bit)?;
    write_empty_ibody_selinux_xattr(raw_inode, &value)?;
    rewrite_inode_checksum(raw_inode, checksum_seed, inode_number).map_err(Ext4Error::Checksum)?;

    let sectors_per_block = u64::from(image.superblock.block_size) / SECTOR_SIZE;
    let shadow = table_shadow;
    let logical_start = table_block
        .checked_mul(sectors_per_block)
        .ok_or(Ext4Error::ArithmeticOverflow)?;
    let replacement = ReplacementExtent {
        logical_start: Sector(logical_start),
        sector_count: SectorCount(sectors_per_block),
        shadow_start: Sector(0),
    };
    let map =
        LoomMap::from_replacements(SectorCount(image.image_bytes / SECTOR_SIZE), &[replacement])
            .map_err(Ext4Error::Map)?;

    Ok(CompiledSelinuxXattr {
        map,
        shadow,
        block_size: image.superblock.block_size,
        inode: inode_number,
        value_bytes: value.len(),
        shadow_blocks: 1,
    })
}

impl Ext4Image {
    fn read_stage6_checksum_seed(&mut self) -> Result<u32, Ext4Error> {
        let mut raw = [0_u8; SUPERBLOCK_SIZE];
        read_exact_at(&mut self.file, SUPERBLOCK_OFFSET, &mut raw)?;
        if read_u32(&raw, SB_FEATURE_RO_COMPAT)? & RO_COMPAT_METADATA_CSUM == 0 {
            return Err(Ext4Error::UnsupportedFilesystemFeature(
                "Stage 6 requires metadata_csum",
            ));
        }
        if read_u32(&raw, SB_CREATOR_OS)? != EXT4_OS_LINUX
            || raw[SB_CHECKSUM_TYPE] != EXT4_CRC32C_CHKSUM
        {
            return Err(Ext4Error::UnsupportedFilesystemFeature(
                "Stage 6 requires Linux CRC32C metadata checksums",
            ));
        }
        let incompat = read_u32(&raw, SB_FEATURE_INCOMPAT)?;
        if incompat & INCOMPAT_CSUM_SEED != 0 {
            return read_u32(&raw, SB_CHECKSUM_SEED);
        }
        let uuid = raw
            .get(SB_UUID..SB_UUID + SB_UUID_SIZE)
            .ok_or(Ext4Error::UnexpectedEndOfStructure)?;
        Ok(crc32c(u32::MAX, uuid))
    }

    fn inode_record_location_stage6(
        &mut self,
        inode_number: u32,
    ) -> Result<(u64, usize), Ext4Error> {
        if inode_number == 0 || inode_number > self.superblock.inodes_count {
            return Err(Ext4Error::InvalidInode(inode_number));
        }
        let zero_based = inode_number - 1;
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
        let offset =
            usize::try_from(byte_offset % block_size).map_err(|_| Ext4Error::ArithmeticOverflow)?;
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
}

fn reject_external_xattr(raw_inode: &[u8], has_64bit: bool) -> Result<(), Ext4Error> {
    let lo = u64::from(read_u32(raw_inode, INODE_FILE_ACL_LO)?);
    let hi = if has_64bit && raw_inode.len() > INODE_FILE_ACL_HI + 1 {
        u64::from(read_u16(raw_inode, INODE_FILE_ACL_HI)?)
    } else {
        0
    };
    if (hi << 32) | lo != 0 {
        return Err(Ext4Error::UnsupportedInodeFeature(
            "Stage 6 refuses existing external xattr blocks",
        ));
    }
    Ok(())
}

fn write_empty_ibody_selinux_xattr(raw_inode: &mut [u8], value: &[u8]) -> Result<(), Ext4Error> {
    if raw_inode.len() <= GOOD_OLD_INODE_SIZE {
        return Err(Ext4Error::UnsupportedInodeFeature(
            "inode has no extra space for in-inode xattrs",
        ));
    }
    let extra_isize = usize::from(read_u16(raw_inode, INODE_EXTRA_ISIZE)?);
    let xattr_start = GOOD_OLD_INODE_SIZE
        .checked_add(extra_isize)
        .ok_or(Ext4Error::ArithmeticOverflow)?;
    let first_entry = xattr_start
        .checked_add(4)
        .ok_or(Ext4Error::ArithmeticOverflow)?;
    if first_entry > raw_inode.len() {
        return Err(Ext4Error::InvalidFilesystem(
            "inode extra_isize leaves no xattr header space",
        ));
    }

    let region = raw_inode
        .get(xattr_start..)
        .ok_or(Ext4Error::UnexpectedEndOfStructure)?;
    if region.iter().any(|byte| *byte != 0) {
        return Err(Ext4Error::UnsupportedInodeFeature(
            "Stage 6 requires an empty in-inode xattr area",
        ));
    }

    let entry_len = align4(
        XATTR_ENTRY_FIXED
            .checked_add(SELINUX_DISK_NAME.len())
            .ok_or(Ext4Error::ArithmeticOverflow)?,
    )?;
    let entry_end = first_entry
        .checked_add(entry_len)
        .ok_or(Ext4Error::ArithmeticOverflow)?;
    let terminator_end = entry_end
        .checked_add(XATTR_END_MARKER)
        .ok_or(Ext4Error::ArithmeticOverflow)?;
    let value_unaligned =
        raw_inode
            .len()
            .checked_sub(value.len())
            .ok_or(Ext4Error::UnsupportedInodeFeature(
                "security.selinux value exceeds inode xattr space",
            ))?;
    let value_start = value_unaligned & !3_usize;
    if value_start < terminator_end || value_start < first_entry {
        return Err(Ext4Error::UnsupportedInodeFeature(
            "security.selinux value does not fit in-inode xattr space",
        ));
    }
    let value_offset = value_start - first_entry;
    let value_offset_u16 =
        u16::try_from(value_offset).map_err(|_| Ext4Error::ArithmeticOverflow)?;
    let value_size_u32 = u32::try_from(value.len()).map_err(|_| Ext4Error::ArithmeticOverflow)?;

    write_u32(raw_inode, xattr_start, XATTR_MAGIC)?;
    raw_inode[first_entry] =
        u8::try_from(SELINUX_DISK_NAME.len()).map_err(|_| Ext4Error::ArithmeticOverflow)?;
    raw_inode[first_entry + 1] = XATTR_SECURITY_INDEX;
    write_u16(raw_inode, first_entry + 2, value_offset_u16)?;
    write_u32(raw_inode, first_entry + 4, 0)?;
    write_u32(raw_inode, first_entry + 8, value_size_u32)?;
    write_u32(raw_inode, first_entry + 12, 0)?;
    let name_start = first_entry
        .checked_add(XATTR_ENTRY_FIXED)
        .ok_or(Ext4Error::ArithmeticOverflow)?;
    let name_end = name_start
        .checked_add(SELINUX_DISK_NAME.len())
        .ok_or(Ext4Error::ArithmeticOverflow)?;
    raw_inode
        .get_mut(name_start..name_end)
        .ok_or(Ext4Error::UnexpectedEndOfStructure)?
        .copy_from_slice(SELINUX_DISK_NAME);
    raw_inode
        .get_mut(value_start..value_start + value.len())
        .ok_or(Ext4Error::UnexpectedEndOfStructure)?
        .copy_from_slice(value);
    Ok(())
}

fn align4(value: usize) -> Result<usize, Ext4Error> {
    value
        .checked_add(3)
        .map(|rounded| rounded & !3_usize)
        .ok_or(Ext4Error::ArithmeticOverflow)
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
    fn selinux_entry_fits_standard_256_byte_inode() {
        let mut inode = vec![0_u8; 256];
        inode[INODE_EXTRA_ISIZE..INODE_EXTRA_ISIZE + 2].copy_from_slice(&32_u16.to_le_bytes());
        let value = b"u:object_r:system_file:s0\0";
        write_empty_ibody_selinux_xattr(&mut inode, value).unwrap();
        assert_eq!(read_u32(&inode, 160).unwrap(), XATTR_MAGIC);
        assert_eq!(inode[165], XATTR_SECURITY_INDEX);
    }

    #[test]
    fn existing_xattr_region_is_rejected() {
        let mut inode = vec![0_u8; 256];
        inode[INODE_EXTRA_ISIZE..INODE_EXTRA_ISIZE + 2].copy_from_slice(&32_u16.to_le_bytes());
        inode[200] = 1;
        assert!(write_empty_ibody_selinux_xattr(&mut inode, b"ctx\0").is_err());
    }
}

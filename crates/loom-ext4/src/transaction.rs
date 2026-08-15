#![forbid(unsafe_code)]

use super::checksum::{crc32c, rewrite_inode_checksum, verify_inode_checksum};
use super::{
    compile_create_file, read_exact_at, read_u16, read_u32, Ext4Error, Ext4Image, SECTOR_SIZE,
    SUPERBLOCK_OFFSET, SUPERBLOCK_SIZE,
};
use loom_map::LoomMap;
use loom_types::Source;
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
const INODE_EXTRA_ISIZE: usize = 0x80;

#[derive(Debug)]
pub struct CompiledCreateSelinuxTransaction {
    pub map: LoomMap,
    pub shadow: Vec<u8>,
    pub block_size: u32,
    pub inode: u32,
    pub value_bytes: usize,
    pub shadow_blocks: usize,
}

/// Mutable editor over a compiled Loom shadow image.
///
/// It never reads or writes the authoritative origin. It only resolves logical
/// filesystem blocks that are already shadow-backed by an earlier operation and
/// returns the corresponding bytes inside the existing shadow pack.
struct EffectiveShadowEditor {
    map: LoomMap,
    shadow: Vec<u8>,
    block_size: u32,
}

impl EffectiveShadowEditor {
    fn new(map: LoomMap, shadow: Vec<u8>, block_size: u32) -> Result<Self, Ext4Error> {
        if block_size == 0 || u64::from(block_size) % SECTOR_SIZE != 0 {
            return Err(Ext4Error::InvalidFilesystem(
                "transaction block size is not sector aligned",
            ));
        }
        Ok(Self {
            map,
            shadow,
            block_size,
        })
    }

    fn shadow_block_mut(&mut self, fs_block: u64) -> Result<&mut [u8], Ext4Error> {
        let sectors_per_block = u64::from(self.block_size) / SECTOR_SIZE;
        let logical_sector = fs_block
            .checked_mul(sectors_per_block)
            .ok_or(Ext4Error::ArithmeticOverflow)?;
        let logical_end = logical_sector
            .checked_add(sectors_per_block)
            .ok_or(Ext4Error::ArithmeticOverflow)?;

        let extent = self
            .map
            .extents()
            .iter()
            .find(|extent| {
                if extent.source != Source::Shadow {
                    return false;
                }
                let start = extent.logical_start.0;
                let Some(end) = start.checked_add(extent.sector_count.0) else {
                    return false;
                };
                logical_sector >= start && logical_end <= end
            })
            .ok_or(Ext4Error::InvalidFilesystem(
                "transaction target block is not shadow-backed",
            ))?;

        let sector_delta = logical_sector
            .checked_sub(extent.logical_start.0)
            .ok_or(Ext4Error::ArithmeticOverflow)?;
        let shadow_sector = extent
            .source_start
            .0
            .checked_add(sector_delta)
            .ok_or(Ext4Error::ArithmeticOverflow)?;
        let byte_start = shadow_sector
            .checked_mul(SECTOR_SIZE)
            .ok_or(Ext4Error::ArithmeticOverflow)?;
        let byte_end = byte_start
            .checked_add(u64::from(self.block_size))
            .ok_or(Ext4Error::ArithmeticOverflow)?;
        let start = usize::try_from(byte_start).map_err(|_| Ext4Error::ArithmeticOverflow)?;
        let end = usize::try_from(byte_end).map_err(|_| Ext4Error::ArithmeticOverflow)?;
        self.shadow
            .get_mut(start..end)
            .ok_or(Ext4Error::InvalidFilesystem(
                "shadow mapping points outside transaction pack",
            ))
    }
}

/// Compiles a two-operation effective-view transaction:
/// 1. create one regular file;
/// 2. attach `security.selinux` to the inode created by operation 1.
///
/// The second operation mutates the inode-table block already emitted by CREATE,
/// proving metadata-block collision coalescing without materializing a second image.
///
/// # Errors
/// Returns [`Ext4Error`] when either operation is unsupported, the CREATE result
/// does not expose its inode block through shadow storage, or checksum/xattr rules fail.
pub fn compile_create_with_selinux_transaction(
    origin_path: &Path,
    target_path: &str,
    payload_path: &Path,
    value_path: &Path,
) -> Result<CompiledCreateSelinuxTransaction, Ext4Error> {
    let value = fs::read(value_path).map_err(Ext4Error::Io)?;
    if value.is_empty() {
        return Err(Ext4Error::UnsupportedInodeFeature(
            "Stage 7 requires a non-empty security.selinux value",
        ));
    }

    let created = compile_create_file(origin_path, target_path, payload_path)?;
    let mut image = Ext4Image::open(origin_path)?;
    let checksum_seed = read_checksum_seed(&mut image)?;
    let (inode_table_block, inode_offset) = inode_record_location(&mut image, created.inode)?;
    let inode_size = usize::from(image.superblock.inode_size);

    let mut editor = EffectiveShadowEditor::new(created.map, created.shadow, created.block_size)?;
    let table_block = editor.shadow_block_mut(inode_table_block)?;
    let inode_end = inode_offset
        .checked_add(inode_size)
        .ok_or(Ext4Error::ArithmeticOverflow)?;
    let raw_inode = table_block
        .get_mut(inode_offset..inode_end)
        .ok_or(Ext4Error::InvalidFilesystem(
            "created inode crosses transaction inode-table block",
        ))?;

    verify_inode_checksum(raw_inode, checksum_seed, created.inode).map_err(Ext4Error::Checksum)?;
    write_empty_ibody_selinux_xattr(raw_inode, &value)?;
    rewrite_inode_checksum(raw_inode, checksum_seed, created.inode).map_err(Ext4Error::Checksum)?;

    let block_size = usize::try_from(editor.block_size).map_err(|_| Ext4Error::ArithmeticOverflow)?;
    let shadow_blocks = editor.shadow.len() / block_size;
    Ok(CompiledCreateSelinuxTransaction {
        map: editor.map,
        shadow: editor.shadow,
        block_size: created.block_size,
        inode: created.inode,
        value_bytes: value.len(),
        shadow_blocks,
    })
}

fn read_checksum_seed(image: &mut Ext4Image) -> Result<u32, Ext4Error> {
    let mut raw = [0_u8; SUPERBLOCK_SIZE];
    read_exact_at(&mut image.file, SUPERBLOCK_OFFSET, &mut raw)?;
    if read_u32(&raw, SB_FEATURE_RO_COMPAT)? & RO_COMPAT_METADATA_CSUM == 0 {
        return Err(Ext4Error::UnsupportedFilesystemFeature(
            "Stage 7 requires metadata_csum",
        ));
    }
    if read_u32(&raw, SB_CREATOR_OS)? != EXT4_OS_LINUX
        || raw[SB_CHECKSUM_TYPE] != EXT4_CRC32C_CHKSUM
    {
        return Err(Ext4Error::UnsupportedFilesystemFeature(
            "Stage 7 requires Linux CRC32C metadata checksums",
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
    let offset = usize::try_from(byte_offset % block_size)
        .map_err(|_| Ext4Error::ArithmeticOverflow)?;
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
            "transaction requires an empty in-inode xattr area",
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
    let value_unaligned = raw_inode
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

    write_u32(raw_inode, xattr_start, XATTR_MAGIC)?;
    raw_inode[first_entry] =
        u8::try_from(SELINUX_DISK_NAME.len()).map_err(|_| Ext4Error::ArithmeticOverflow)?;
    raw_inode[first_entry + 1] = XATTR_SECURITY_INDEX;
    write_u16(
        raw_inode,
        first_entry + 2,
        u16::try_from(value_start - first_entry).map_err(|_| Ext4Error::ArithmeticOverflow)?,
    )?;
    write_u32(raw_inode, first_entry + 4, 0)?;
    write_u32(
        raw_inode,
        first_entry + 8,
        u32::try_from(value.len()).map_err(|_| Ext4Error::ArithmeticOverflow)?,
    )?;
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
    let value_end = value_start
        .checked_add(value.len())
        .ok_or(Ext4Error::ArithmeticOverflow)?;
    raw_inode
        .get_mut(value_start..value_end)
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
    use loom_map::ReplacementExtent;
    use loom_types::{Sector, SectorCount};

    #[test]
    fn editor_locates_block_inside_merged_shadow_extent() {
        let map = LoomMap::from_replacements(
            SectorCount(64),
            &[
                ReplacementExtent {
                    logical_start: Sector(8),
                    sector_count: SectorCount(8),
                    shadow_start: Sector(0),
                },
                ReplacementExtent {
                    logical_start: Sector(16),
                    sector_count: SectorCount(8),
                    shadow_start: Sector(8),
                },
            ],
        )
        .unwrap();
        let mut editor = EffectiveShadowEditor::new(map, vec![0_u8; 8192], 4096).unwrap();
        editor.shadow_block_mut(2).unwrap()[0] = 0x5a;
        assert_eq!(editor.shadow[4096], 0x5a);
    }
}

#![forbid(unsafe_code)]

const CRC32C_POLY_REFLECTED: u32 = 0x82F6_3B78;
const INODE_GENERATION_OFFSET: usize = 0x64;
const INODE_CHECKSUM_LO_OFFSET: usize = 0x7C;
const INODE_EXTRA_ISIZE_OFFSET: usize = 0x80;
const INODE_CHECKSUM_HI_OFFSET: usize = 0x82;
const GOOD_OLD_INODE_SIZE: usize = 128;

/// Computes the raw Linux CRC32C state used by ext4 metadata checksums.
///
/// This intentionally performs no final XOR; Linux `crc32c(seed, data, len)`
/// consumes and returns the running CRC state directly.
#[must_use]
pub(crate) fn crc32c(mut crc: u32, bytes: &[u8]) -> u32 {
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = 0_u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (CRC32C_POLY_REFLECTED & mask);
        }
    }
    crc
}

/// Derives ext4's per-inode checksum seed from the filesystem checksum seed,
/// little-endian inode number, and little-endian inode generation.
#[must_use]
pub(crate) fn inode_seed(fs_seed: u32, inode_number: u32, generation: u32) -> u32 {
    let after_inode = crc32c(fs_seed, &inode_number.to_le_bytes());
    crc32c(after_inode, &generation.to_le_bytes())
}

/// Recomputes and writes the ext4 inode checksum fields in-place.
///
/// # Errors
/// Returns [`ChecksumError`] if the inode record is too small to contain the
/// mandatory checksum/generation fields or declares an impossible extra-inode
/// layout.
pub(crate) fn rewrite_inode_checksum(
    raw_inode: &mut [u8],
    fs_seed: u32,
    inode_number: u32,
) -> Result<u32, ChecksumError> {
    if raw_inode.len() < GOOD_OLD_INODE_SIZE {
        return Err(ChecksumError::InodeTooSmall(raw_inode.len()));
    }

    let generation = read_u32(raw_inode, INODE_GENERATION_OFFSET)?;
    write_u16(raw_inode, INODE_CHECKSUM_LO_OFFSET, 0)?;

    let high_checksum_fits = if raw_inode.len() > GOOD_OLD_INODE_SIZE {
        let extra_isize = usize::from(read_u16(raw_inode, INODE_EXTRA_ISIZE_OFFSET)?);
        let declared_end = GOOD_OLD_INODE_SIZE
            .checked_add(extra_isize)
            .ok_or(ChecksumError::InvalidExtraIsize(extra_isize))?;
        if declared_end > raw_inode.len() {
            return Err(ChecksumError::InvalidExtraIsize(extra_isize));
        }
        INODE_CHECKSUM_HI_OFFSET + 2 <= declared_end
    } else {
        false
    };

    if high_checksum_fits {
        write_u16(raw_inode, INODE_CHECKSUM_HI_OFFSET, 0)?;
    }

    let checksum = crc32c(inode_seed(fs_seed, inode_number, generation), raw_inode);
    write_u16(raw_inode, INODE_CHECKSUM_LO_OFFSET, checksum as u16)?;
    if high_checksum_fits {
        write_u16(raw_inode, INODE_CHECKSUM_HI_OFFSET, (checksum >> 16) as u16)?;
    }
    Ok(checksum)
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, ChecksumError> {
    let end = offset.checked_add(2).ok_or(ChecksumError::OutOfBounds)?;
    let raw: [u8; 2] = bytes
        .get(offset..end)
        .ok_or(ChecksumError::OutOfBounds)?
        .try_into()
        .map_err(|_| ChecksumError::OutOfBounds)?;
    Ok(u16::from_le_bytes(raw))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, ChecksumError> {
    let end = offset.checked_add(4).ok_or(ChecksumError::OutOfBounds)?;
    let raw: [u8; 4] = bytes
        .get(offset..end)
        .ok_or(ChecksumError::OutOfBounds)?
        .try_into()
        .map_err(|_| ChecksumError::OutOfBounds)?;
    Ok(u32::from_le_bytes(raw))
}

fn write_u16(bytes: &mut [u8], offset: usize, value: u16) -> Result<(), ChecksumError> {
    let end = offset.checked_add(2).ok_or(ChecksumError::OutOfBounds)?;
    let target = bytes
        .get_mut(offset..end)
        .ok_or(ChecksumError::OutOfBounds)?;
    target.copy_from_slice(&value.to_le_bytes());
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChecksumError {
    InodeTooSmall(usize),
    InvalidExtraIsize(usize),
    OutOfBounds,
}

impl core::fmt::Display for ChecksumError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::InodeTooSmall(size) => write!(f, "inode record is only {size} bytes"),
            Self::InvalidExtraIsize(size) => write!(f, "invalid ext4 inode extra_isize {size}"),
            Self::OutOfBounds => write!(f, "inode checksum field lies outside inode record"),
        }
    }
}

impl std::error::Error for ChecksumError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32c_matches_standard_raw_state() {
        // The standard finalized CRC32C for "123456789" is 0xe3069283;
        // ext4/Linux uses the raw state without the final complement.
        assert_eq!(crc32c(u32::MAX, b"123456789"), 0x1cf9_6d7c);
    }

    #[test]
    fn inode_seed_is_incremental() {
        let fs_seed = 0x1234_5678;
        let ino = 42_u32;
        let generation = 0xaabb_ccdd_u32;
        let mut combined = Vec::new();
        combined.extend_from_slice(&ino.to_le_bytes());
        combined.extend_from_slice(&generation.to_le_bytes());
        assert_eq!(inode_seed(fs_seed, ino, generation), crc32c(fs_seed, &combined));
    }
}

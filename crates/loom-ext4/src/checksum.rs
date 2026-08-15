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

/// Verifies the checksum stored in an ext4 inode record before Loom mutates it.
///
/// # Errors
/// Returns [`ChecksumError`] for malformed inode layouts or a checksum mismatch.
pub(crate) fn verify_inode_checksum(
    raw_inode: &[u8],
    fs_seed: u32,
    inode_number: u32,
) -> Result<(), ChecksumError> {
    let high_checksum_fits = inode_checksum_hi_fits(raw_inode)?;
    let provided_low = read_u16(raw_inode, INODE_CHECKSUM_LO_OFFSET)?;
    let provided_high = if high_checksum_fits {
        Some(read_u16(raw_inode, INODE_CHECKSUM_HI_OFFSET)?)
    } else {
        None
    };

    let mut copy = raw_inode.to_vec();
    write_u16(&mut copy, INODE_CHECKSUM_LO_OFFSET, 0)?;
    if high_checksum_fits {
        write_u16(&mut copy, INODE_CHECKSUM_HI_OFFSET, 0)?;
    }
    let generation = read_u32(&copy, INODE_GENERATION_OFFSET)?;
    let checksum = crc32c(inode_seed(fs_seed, inode_number, generation), &copy);
    let bytes = checksum.to_le_bytes();
    let expected_low = u16::from_le_bytes([bytes[0], bytes[1]]);
    let expected_high = u16::from_le_bytes([bytes[2], bytes[3]]);

    if provided_low != expected_low || provided_high.is_some_and(|value| value != expected_high) {
        return Err(ChecksumError::Mismatch {
            expected: checksum,
            stored_low: provided_low,
            stored_high: provided_high,
        });
    }
    Ok(())
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
    let high_checksum_fits = inode_checksum_hi_fits(raw_inode)?;
    let generation = read_u32(raw_inode, INODE_GENERATION_OFFSET)?;
    write_u16(raw_inode, INODE_CHECKSUM_LO_OFFSET, 0)?;
    if high_checksum_fits {
        write_u16(raw_inode, INODE_CHECKSUM_HI_OFFSET, 0)?;
    }

    let checksum = crc32c(inode_seed(fs_seed, inode_number, generation), raw_inode);
    let checksum_bytes = checksum.to_le_bytes();
    write_u16(
        raw_inode,
        INODE_CHECKSUM_LO_OFFSET,
        u16::from_le_bytes([checksum_bytes[0], checksum_bytes[1]]),
    )?;
    if high_checksum_fits {
        write_u16(
            raw_inode,
            INODE_CHECKSUM_HI_OFFSET,
            u16::from_le_bytes([checksum_bytes[2], checksum_bytes[3]]),
        )?;
    }
    Ok(checksum)
}

fn inode_checksum_hi_fits(raw_inode: &[u8]) -> Result<bool, ChecksumError> {
    if raw_inode.len() < GOOD_OLD_INODE_SIZE {
        return Err(ChecksumError::InodeTooSmall(raw_inode.len()));
    }
    if raw_inode.len() == GOOD_OLD_INODE_SIZE {
        return Ok(false);
    }
    let extra_isize = usize::from(read_u16(raw_inode, INODE_EXTRA_ISIZE_OFFSET)?);
    let declared_end = GOOD_OLD_INODE_SIZE
        .checked_add(extra_isize)
        .ok_or(ChecksumError::InvalidExtraIsize(extra_isize))?;
    if declared_end > raw_inode.len() {
        return Err(ChecksumError::InvalidExtraIsize(extra_isize));
    }
    Ok(INODE_CHECKSUM_HI_OFFSET + 2 <= declared_end)
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
pub enum ChecksumError {
    InodeTooSmall(usize),
    InvalidExtraIsize(usize),
    OutOfBounds,
    Mismatch {
        expected: u32,
        stored_low: u16,
        stored_high: Option<u16>,
    },
}

impl core::fmt::Display for ChecksumError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::InodeTooSmall(size) => write!(f, "inode record is only {size} bytes"),
            Self::InvalidExtraIsize(size) => write!(f, "invalid ext4 inode extra_isize {size}"),
            Self::OutOfBounds => write!(f, "inode checksum field lies outside inode record"),
            Self::Mismatch {
                expected,
                stored_low,
                stored_high,
            } => write!(
                f,
                "inode checksum mismatch: expected {expected:#010x}, stored low {stored_low:#06x}, high {stored_high:?}"
            ),
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
        assert_eq!(
            inode_seed(fs_seed, ino, generation),
            crc32c(fs_seed, &combined)
        );
    }

    #[test]
    fn rewritten_inode_checksum_verifies() {
        let mut inode = vec![0_u8; 256];
        inode[INODE_EXTRA_ISIZE_OFFSET..INODE_EXTRA_ISIZE_OFFSET + 2]
            .copy_from_slice(&32_u16.to_le_bytes());
        inode[INODE_GENERATION_OFFSET..INODE_GENERATION_OFFSET + 4]
            .copy_from_slice(&7_u32.to_le_bytes());
        let seed = 0x1234_5678;
        rewrite_inode_checksum(&mut inode, seed, 19).unwrap();
        verify_inode_checksum(&inode, seed, 19).unwrap();
        inode[0x20] ^= 1;
        assert!(matches!(
            verify_inode_checksum(&inode, seed, 19),
            Err(ChecksumError::Mismatch { .. })
        ));
    }
}

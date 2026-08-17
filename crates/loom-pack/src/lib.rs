#![forbid(unsafe_code)]

use core::fmt;
use std::fs;
use std::io;
use std::path::Path;

pub fn pack_block(input: &[u8], block_size: usize) -> Result<Vec<u8>, PackError> {
    if block_size == 0 {
        return Err(PackError::ZeroBlockSize);
    }
    if input.len() > block_size {
        return Err(PackError::InputTooLarge {
            input: input.len(),
            block_size,
        });
    }

    let mut block = vec![0_u8; block_size];
    block[..input.len()].copy_from_slice(input);
    Ok(block)
}

pub fn pack_file(
    input_path: &Path,
    output_path: &Path,
    block_size: usize,
) -> Result<(), PackError> {
    let input = fs::read(input_path).map_err(PackError::Io)?;
    let block = pack_block(&input, block_size)?;
    fs::write(output_path, block).map_err(PackError::Io)
}

#[derive(Debug)]
pub enum PackError {
    ZeroBlockSize,
    InputTooLarge { input: usize, block_size: usize },
    Io(io::Error),
}

impl fmt::Display for PackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroBlockSize => write!(f, "shadow block size must be non-zero"),
            Self::InputTooLarge { input, block_size } => write!(
                f,
                "input is {input} bytes but shadow block size is only {block_size} bytes"
            ),
            Self::Io(error) => write!(f, "I/O error: {error}"),
        }
    }
}

impl std::error::Error for PackError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::ZeroBlockSize | Self::InputTooLarge { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_payload_is_zero_padded_to_exact_block_size() {
        let packed = pack_block(b"loom", 8).unwrap();
        assert_eq!(packed, b"loom\0\0\0\0");
    }

    #[test]
    fn oversized_payload_is_rejected() {
        let error = pack_block(b"12345", 4).unwrap_err();
        assert!(matches!(error, PackError::InputTooLarge { .. }));
    }
}

use std::io;

pub struct PageBuffer {
    bytes: Vec<u8>,
}

impl PageBuffer {
    /// Allocates a host-side fallback buffer.
    ///
    /// # Errors
    /// Returns an invalid-input error for a zero-length buffer.
    pub fn new(len: usize) -> io::Result<Self> {
        if len == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "page buffer length must be non-zero",
            ));
        }
        Ok(Self {
            bytes: vec![0_u8; len],
        })
    }

    #[must_use]
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        self.bytes.as_mut_slice()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

/// NoMount keyring transport is only available on Linux and Android.
///
/// # Errors
/// Always returns `Unsupported` on other targets.
pub fn add_key_nomount(_page: &mut PageBuffer) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "NoMount keyring transport is Linux/Android only",
    ))
}

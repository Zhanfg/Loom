use core::ffi::{c_char, c_long, c_void};
use std::io;
use std::ptr::NonNull;

const PROT_READ: i32 = 0x1;
const PROT_WRITE: i32 = 0x2;
const MAP_PRIVATE: i32 = 0x02;
const MAP_ANONYMOUS: i32 = 0x20;
const ECANCELED: i32 = 125;
const KEY_SPEC_THREAD_KEYRING: i32 = -1;
const NOMOUNT_KEY_TYPE: &[u8] = b"nomount\0";
const KEY_DESCRIPTION: &[u8] = b"trigger\0";

#[cfg(target_arch = "aarch64")]
const SYS_ADD_KEY: c_long = 217;
#[cfg(target_arch = "arm")]
const SYS_ADD_KEY: c_long = 309;
#[cfg(target_arch = "x86_64")]
const SYS_ADD_KEY: c_long = 248;

unsafe extern "C" {
    fn mmap(
        addr: *mut c_void,
        length: usize,
        prot: i32,
        flags: i32,
        fd: i32,
        offset: isize,
    ) -> *mut c_void;
    fn munmap(addr: *mut c_void, length: usize) -> i32;
    fn syscall(number: c_long, ...) -> c_long;
}

pub struct PageBuffer {
    ptr: NonNull<u8>,
    len: usize,
}

impl PageBuffer {
    /// Allocates one writable anonymous mapping of exactly `len` bytes.
    ///
    /// # Errors
    /// Returns the host OS error when `mmap` fails.
    pub fn new(len: usize) -> io::Result<Self> {
        if len == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "page buffer length must be non-zero",
            ));
        }

        let addr = unsafe {
            mmap(
                std::ptr::null_mut(),
                len,
                PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if addr as isize == -1 {
            return Err(io::Error::last_os_error());
        }
        let ptr = NonNull::new(addr.cast::<u8>())
            .ok_or_else(|| io::Error::other("mmap returned a null pointer"))?;
        Ok(Self { ptr, len })
    }

    #[must_use]
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Drop for PageBuffer {
    fn drop(&mut self) {
        let _ = unsafe { munmap(self.ptr.as_ptr().cast::<c_void>(), self.len) };
    }
}

/// Sends a NoMount payload pointer through Linux/Android `add_key(2)`.
///
/// NoMount intentionally returns `-ECANCELED` after consuming the pointer so that
/// no key object is instantiated. That errno is therefore treated as successful delivery.
///
/// # Errors
/// Returns the OS error when the syscall fails with anything other than `ECANCELED`.
pub fn add_key_nomount(page: &mut PageBuffer) -> io::Result<()> {
    let payload_ptr = page.ptr.as_ptr() as usize;
    let ret = unsafe {
        syscall(
            SYS_ADD_KEY,
            NOMOUNT_KEY_TYPE.as_ptr().cast::<c_char>(),
            KEY_DESCRIPTION.as_ptr().cast::<c_char>(),
            std::ptr::addr_of!(payload_ptr),
            std::mem::size_of::<usize>(),
            KEY_SPEC_THREAD_KEYRING,
        )
    };

    if ret < 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(ECANCELED) {
            return Err(error);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::PageBuffer;

    #[test]
    fn page_buffer_has_requested_length() {
        let mut page = PageBuffer::new(4096).unwrap();
        assert_eq!(page.len(), 4096);
        assert_eq!(page.as_mut_slice().len(), 4096);
    }
}

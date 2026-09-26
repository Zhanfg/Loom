#![allow(unsafe_code)]

#[cfg(any(target_os = "linux", target_os = "android"))]
mod platform;
#[cfg(not(any(target_os = "linux", target_os = "android")))]
mod platform_stub;

#[cfg(any(target_os = "linux", target_os = "android"))]
pub use platform::{add_key_nomount, PageBuffer};
#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub use platform_stub::{add_key_nomount, PageBuffer};

extern crate libc;

#[cfg(target_os = "windows")]
extern crate winapi;

#[cfg(target_os = "windows")]
mod win32;
#[cfg(target_os = "windows")]
pub use win32::*;

#[cfg(target_os = "macos")]
extern crate core_text;
#[cfg(target_os = "macos")]
extern crate core_foundation;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::*;

#[cfg(all(unix, not(target_os = "macos")))]
extern crate fontconfig as servo_fontconfig;
#[cfg(all(unix, not(target_os = "macos")))]
mod fontconfig;
#[cfg(all(unix, not(target_os = "macos")))]
pub use fontconfig::*;

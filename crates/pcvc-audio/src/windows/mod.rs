//! Windows Core Audio backend.
//!
//! Only [`logic`] is platform independent; it is also compiled for tests on
//! other platforms.

mod logic;

#[cfg(target_os = "windows")]
mod backend;
#[cfg(target_os = "windows")]
mod callbacks;
#[cfg(target_os = "windows")]
mod com;
#[cfg(target_os = "windows")]
mod policy_config;
#[cfg(target_os = "windows")]
mod worker;

#[cfg(target_os = "windows")]
pub use backend::WindowsBackend;

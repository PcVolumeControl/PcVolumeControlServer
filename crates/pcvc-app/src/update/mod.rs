//! Update checks against GitHub Releases and, on Windows, installing an
//! update with the release's installer.
//!
//! Only GitHub is contacted ([`http::Policy`]); downloads are verified
//! against the release's `SHA256SUMS` and, when a key is built in, its
//! minisign signature before anything runs them.

mod download;
mod http;
mod release;
#[cfg(test)]
mod tests;
mod verify;
mod version;

//! Downloading a release's installer into a private directory and
//! verifying it before anything runs it.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use minisign_verify::PublicKey;
use sha2::{Digest, Sha256};

use super::http::{Client, HttpError, Limits};
use super::release::Release;
use super::verify::{self, VerifyError};

const SUMS_LIMITS: Limits = Limits {
    accept: "application/octet-stream",
    max_bytes: 64 * 1024,
    total: Duration::from_secs(60),
};
const SIGNATURE_LIMITS: Limits = Limits {
    accept: "application/octet-stream",
    max_bytes: 4 * 1024,
    total: Duration::from_secs(60),
};
/// The installer is about 10 MB.
const INSTALLER_MAX: u64 = 100 * 1024 * 1024;
const INSTALLER_TOTAL: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DownloadError {
    #[error("the release has no installer")]
    NoInstaller,
    #[error("the release has no SHA256SUMS")]
    NoChecksums,
    #[error("the installer is larger than {INSTALLER_MAX} bytes")]
    TooLarge,
    #[error("the download is incomplete")]
    WrongSize,
    #[error("download failed: {0}")]
    Http(#[from] HttpError),
    #[error("{0}")]
    Verify(#[from] VerifyError),
    #[error("saving the download failed: {0}")]
    Io(String),
}

impl From<std::io::Error> for DownloadError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    Downloading { done: u64, total: u64 },
    Verifying,
}

/// A verified installer. Dropping it deletes its directory, which fails
/// harmlessly on Windows while the installer runs; the next start clears
/// what is left (see [`clear`]).
#[derive(Debug)]
pub struct Verified {
    dir: PathBuf,
    pub path: PathBuf,
    /// Whether a signature was checked as well as the checksum.
    pub signed: bool,
}

impl Drop for Verified {
    fn drop(&mut self) {
        remove_dir(&self.dir);
    }
}

/// Deletes a download directory; a guard so every early return cleans up.
struct DirGuard(Option<PathBuf>);

impl Drop for DirGuard {
    fn drop(&mut self) {
        if let Some(dir) = self.0.take() {
            remove_dir(&dir);
        }
    }
}

fn remove_dir(dir: &Path) {
    if let Err(e) = fs::remove_dir_all(dir)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::debug!("removing {} failed: {e}", dir.display());
    }
}

/// Removes leftover downloads in `updates_dir`.
pub fn clear(updates_dir: &Path) {
    remove_dir(updates_dir);
}

/// Downloads `release`'s installer into a new directory under
/// `updates_dir` and checks it against `SHA256SUMS` from the same release,
/// which must itself carry a valid signature when `key` is given. On any
/// failure nothing is left behind.
pub fn download_and_verify(
    client: &Client,
    release: &Release,
    key: Option<&PublicKey>,
    updates_dir: &Path,
    progress: &mut dyn FnMut(Progress),
) -> Result<Verified, DownloadError> {
    let asset = release.asset.as_ref().ok_or(DownloadError::NoInstaller)?;
    let checksums = release
        .checksums
        .as_ref()
        .ok_or(DownloadError::NoChecksums)?;
    if asset.size > INSTALLER_MAX {
        return Err(DownloadError::TooLarge);
    }

    let sums = client.get_bytes(&checksums.url, SUMS_LIMITS)?;
    if let Some(key) = key {
        let signature = release
            .signature
            .as_ref()
            .ok_or(VerifyError::MissingSignature)?;
        let signature = match client.get_bytes(&signature.url, SIGNATURE_LIMITS) {
            Err(HttpError::NotFound) => return Err(VerifyError::MissingSignature.into()),
            result => result?,
        };
        verify::verify_signature(key, &sums, &signature)?;
    }
    let sums = std::str::from_utf8(&sums).map_err(|_| VerifyError::MalformedSums(0))?;
    let expected = verify::digest_for(sums, &asset.name)?;

    let dir = create_private_dir(updates_dir)?;
    let mut guard = DirGuard(Some(dir.clone()));
    let path = dir.join(&asset.name);
    let mut file = File::options().write(true).create_new(true).open(&path)?;
    let mut hasher = Sha256::new();
    let mut done = 0u64;
    let mut reported = None;
    progress(Progress::Downloading {
        done: 0,
        total: asset.size,
    });
    let limits = Limits {
        accept: "application/octet-stream",
        max_bytes: asset.size,
        total: INSTALLER_TOTAL,
    };
    let length = client.get(&asset.url, limits, &mut |chunk| {
        hasher.update(chunk);
        file.write_all(chunk)?;
        done += chunk.len() as u64;
        let percent = done * 100 / asset.size;
        if reported != Some(percent) {
            reported = Some(percent);
            progress(Progress::Downloading {
                done,
                total: asset.size,
            });
        }
        Ok(())
    })?;
    file.sync_all()?;
    drop(file);
    progress(Progress::Verifying);
    if length != asset.size {
        return Err(DownloadError::WrongSize);
    }
    if hasher.finalize().as_slice() != expected {
        return Err(VerifyError::ChecksumMismatch.into());
    }
    // What is on disk, which is what will run, must match too.
    if hash_file(&path)? != expected {
        return Err(VerifyError::ChecksumMismatch.into());
    }
    guard.0 = None;
    Ok(Verified {
        dir,
        path,
        signed: key.is_some(),
    })
}

fn hash_file(path: &Path) -> std::io::Result<[u8; 32]> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 64 * 1024];
    loop {
        match file.read(&mut buffer)? {
            0 => return Ok(hasher.finalize().into()),
            n => hasher.update(&buffer[..n]),
        }
    }
}

/// A new directory of this process's own under `parent`, readable only by
/// the user on Unix (Windows directories under the profile already are).
fn create_private_dir(parent: &Path) -> std::io::Result<PathBuf> {
    fs::create_dir_all(parent)?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let dir = parent.join(format!("{}-{nanos}", std::process::id()));
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut fs::DirBuilder::new(), 0o700).create(&dir)?;
    #[cfg(not(unix))]
    fs::DirBuilder::new().create(&dir)?;
    Ok(dir)
}

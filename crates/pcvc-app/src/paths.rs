//! Per-user directories.

use std::path::PathBuf;

use directories::ProjectDirs;

fn project_dirs() -> Option<ProjectDirs> {
    ProjectDirs::from("", "PcVolumeControl", "PcVolumeControl")
}

/// The config file, or `None` without a home directory.
pub fn config_file() -> Option<PathBuf> {
    project_dirs().map(|d| d.config_dir().join("config.toml"))
}

/// Where the rolling log files are written.
pub fn log_dir() -> Option<PathBuf> {
    project_dirs().map(|d| d.data_local_dir().join("logs"))
}

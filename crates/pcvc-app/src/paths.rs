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

/// What the update feature keeps between runs, next to the config.
pub fn update_state_file() -> Option<PathBuf> {
    project_dirs().map(|d| d.config_dir().join("update-state.toml"))
}

/// Where update installers are downloaded, private to the user.
pub fn updates_dir() -> Option<PathBuf> {
    project_dirs().map(|d| d.data_local_dir().join("updates"))
}

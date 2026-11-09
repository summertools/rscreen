//! Filesystem locations and socket helpers.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// Override for tests and unusual setups: `RSCREEN_RUNTIME_DIR`.
pub fn runtime_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("RSCREEN_RUNTIME_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir).join("rscreen");
        }
    }
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/rscreen-{}", uid))
}

pub fn config_path() -> PathBuf {
    if let Ok(path) = std::env::var("RSCREEN_CONFIG") {
        if !path.is_empty() {
            return PathBuf::from(path);
        }
    }
    if let Ok(dir) = std::env::var("XDG_CONFIG_HOME") {
        if !dir.is_empty() {
            return PathBuf::from(dir).join("rscreen").join("config.toml");
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            return PathBuf::from(home)
                .join(".config")
                .join("rscreen")
                .join("config.toml");
        }
    }
    // Never fall back to a predictable file in a world-writable directory:
    // it could be created by another local user and inject shell commands.
    runtime_dir().join("config.toml")
}

pub fn ensure_runtime_dir() -> Result<PathBuf> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let dir = runtime_dir();
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating runtime directory {}", dir.display()))?;

    // Refuse to use a shared or attacker-controlled directory. Session
    // sockets carry screen contents and clipboard traffic, so they must live
    // in a private per-user directory.
    let metadata = std::fs::symlink_metadata(&dir)
        .with_context(|| format!("inspecting runtime directory {}", dir.display()))?;
    if !metadata.file_type().is_dir() {
        bail!("runtime path {} is not a directory", dir.display());
    }
    if metadata.uid() != unsafe { libc::geteuid() } {
        bail!(
            "runtime directory {} is not owned by the current user",
            dir.display()
        );
    }
    if metadata.mode() & 0o077 != 0 {
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("making runtime directory {} private", dir.display()))?;
        let metadata = std::fs::metadata(&dir)?;
        if metadata.mode() & 0o077 != 0 {
            bail!(
                "runtime directory {} is not private (mode {:o}); set RSCREEN_RUNTIME_DIR to a private directory",
                dir.display(),
                metadata.mode() & 0o777
            );
        }
    }
    Ok(dir)
}

pub fn socket_path(name: &str) -> PathBuf {
    runtime_dir().join(format!("{}.sock", name))
}

pub fn lock_path(name: &str) -> PathBuf {
    runtime_dir().join(format!("{}.lock", name))
}

pub fn log_path(name: &str) -> PathBuf {
    runtime_dir().join("logs").join(format!("{}.log", name))
}

/// Socket paths longer than this cannot be used with sockaddr_un.
const MAX_SOCKET_PATH: usize = 100;

pub fn socket_path_fits(path: &Path) -> bool {
    path.as_os_str().len() < MAX_SOCKET_PATH
}

pub fn session_names() -> Vec<String> {
    let dir = runtime_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("sock") {
            continue;
        }
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            if !stem.is_empty() {
                names.push(stem.to_string());
            }
        }
    }
    names.sort();
    names
}

/// Remove a stale socket file if nothing is listening on it.
pub fn remove_stale_socket(path: &Path) {
    if let Ok(stream) = std::os::unix::net::UnixStream::connect(path) {
        drop(stream);
        return;
    }
    let _ = std::fs::remove_file(path);
}

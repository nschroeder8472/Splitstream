//! Per-user `%APPDATA%\Splitstream\` paths (simple-launch.md L4) — via the
//! `directories` wrapper crate, never `windows-rs` directly (app-shell.md
//! constraint). Replaces the old CWD-relative `config_path()`: a
//! double-clicked install has no meaningful "current directory".

use std::path::PathBuf;

const APP_DIR_NAME: &str = "Splitstream";
const CONFIG_FILE_NAME: &str = "splitstream.toml";
const LOG_DIR_NAME: &str = "logs";

/// `%APPDATA%\Splitstream\splitstream.toml`. Falls back to a
/// current-directory file if the OS can't resolve a per-user data dir at all
/// (no `HOME`/`USERPROFILE`) — never a hard failure this early in startup.
pub fn config_path() -> PathBuf {
    app_dir()
        .map(|dir| dir.join(CONFIG_FILE_NAME))
        .unwrap_or_else(|| PathBuf::from(CONFIG_FILE_NAME))
}

/// `%APPDATA%\Splitstream\logs`. Same fallback as `config_path`.
pub fn log_dir() -> PathBuf {
    app_dir()
        .map(|dir| dir.join(LOG_DIR_NAME))
        .unwrap_or_else(|| PathBuf::from(LOG_DIR_NAME))
}

fn app_dir() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|dirs| dirs.data_dir().join(APP_DIR_NAME))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_path_ends_with_the_app_dir_and_file_name() {
        assert!(config_path().ends_with("Splitstream/splitstream.toml"));
    }

    #[test]
    fn log_dir_ends_with_the_app_dir_and_logs_name() {
        assert!(log_dir().ends_with("Splitstream/logs"));
    }
}

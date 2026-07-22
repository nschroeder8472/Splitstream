//! Same-directory temp-file + rename: atomic on the same filesystem, and
//! avoids a watcher ever observing a half-written file. Shared by `config`
//! (seed template) and `store` (live edits) — one write path, not two
//! independently-maintained copies.

use std::fs;
use std::path::Path;

pub(crate) fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    let tmp = path.with_extension("toml.tmp");
    fs::write(&tmp, text).map_err(|e| e.to_string())?;
    fs::rename(&tmp, path).map_err(|e| e.to_string())
}

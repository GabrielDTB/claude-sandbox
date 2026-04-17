//! Marker-cached `podman load`.
//!
//! Re-loading a Nix-built image tar into podman's store on every launch is
//! slow (up to seconds). The shell cached "did I already load this store
//! path?" via a marker file under `$XDG_CACHE_HOME/claude-sandbox/`. We
//! preserve the exact same layout — marker filenames, contents — so a mixed
//! shell/Rust deployment keeps a warm cache.

use std::fs::{self, File};
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// Load `image_path` into podman's store iff the marker file doesn't already
/// name that exact store path.
pub fn load_if_needed(image_path: &str, marker: &str) -> Result<(), crate::Error> {
    if image_path.is_empty() {
        return Err(format!(
            "image path for marker '{marker}' is empty — build with Nix (sandbox.nix)"
        )
        .into());
    }
    let cache_dir = cache_dir()?;
    fs::create_dir_all(&cache_dir)?;
    let marker_file = cache_dir.join(marker);

    if read_marker(&marker_file).as_deref() == Some(image_path) {
        return Ok(());
    }

    // `podman load < $image_path` — shell-form redirect translates to stdin
    // redirection here.
    let tar = File::open(image_path)
        .map_err(|e| -> crate::Error { format!("cannot open image tar {image_path}: {e}").into() })?;
    let status = Command::new("podman")
        .args(["load"])
        .stdin(Stdio::from(tar))
        .stdout(Stdio::null())
        .status()
        .map_err(|e| -> crate::Error { format!("failed to spawn `podman load`: {e}").into() })?;
    if !status.success() {
        return Err(format!("`podman load` failed (exit {status}) for {image_path}").into());
    }

    fs::write(&marker_file, image_path)?;
    Ok(())
}

fn cache_dir() -> Result<PathBuf, crate::Error> {
    if let Some(x) = std::env::var_os("XDG_CACHE_HOME") {
        return Ok(PathBuf::from(x).join("claude-sandbox"));
    }
    if let Some(h) = std::env::var_os("HOME") {
        return Ok(PathBuf::from(h).join(".cache").join("claude-sandbox"));
    }
    Err("neither XDG_CACHE_HOME nor HOME is set; cannot locate image marker cache".into())
}

fn read_marker(p: &std::path::Path) -> Option<String> {
    let mut f = File::open(p).ok()?;
    let mut s = String::new();
    f.read_to_string(&mut s).ok()?;
    Some(s.trim_end_matches('\n').to_string())
}

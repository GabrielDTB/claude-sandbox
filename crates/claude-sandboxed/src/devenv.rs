//! `--devenv` / `--flake` dev-environment capture. The flow:
//!   1. Compute a composite hash of the inputs (lockfiles, flake.nix, and
//!      — for devenv — the realpath of `.devenv/profile`, since that
//!      symlink rewrites when devenv rebuilds after a new package).
//!   2. Compare against `$SANDBOX_DIR/dev-env.hash`. If unchanged AND the
//!      two capture artifacts exist, skip the expensive nix calls.
//!   3. Otherwise, run `nix print-dev-env` (flake) or `devenv shell ...`
//!      (devenv) to serialize the environment to `dev-env.sh`, and
//!      `nix path-info -r` to capture the closure's store paths into
//!      `dev-closure-paths`. Write the new hash on success.
//!   4. Unconditionally write `dev-entrypoint.sh` — cheap, idempotent.

use sha2::Digest;
use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::cli::DevEnv;
use crate::state::State;

/// Variables stripped from the devenv-captured env before mounting it
/// into the sandbox — they're either host-specific or container-managed.
const DROP_VARS: &[&str] = &[
    "HOME", "USER", "TMPDIR", "SHELL", "SHLVL", "PWD", "OLDPWD", "_", "LOGNAME", "HOSTNAME",
];

/// Runtime entrypoint written into the container. Sources the dev env
/// before exec'ing the child. Kept here (not in a separate file) because
/// the content is short and tightly coupled to the capture logic above.
const DEV_ENTRYPOINT: &str = "\
#!/bin/bash
BASE_PATH=\"$PATH\"
source /dev-env.sh
export PATH=\"$PATH:$BASE_PATH\"
export HOME=/home/user
export USER=user
export TMPDIR=/tmp
exec \"$@\"
";

pub fn capture(kind: &DevEnv, state: &State) -> Result<(), crate::Error> {
    let src = match kind {
        DevEnv::Flake(p) => fs::canonicalize(p).map_err(|e| -> crate::Error {
            format!("flake source does not exist: {} ({e})", p.display()).into()
        })?,
        DevEnv::Devenv(p) => fs::canonicalize(p).map_err(|e| -> crate::Error {
            format!("devenv source does not exist: {} ({e})", p.display()).into()
        })?,
    };

    match kind {
        DevEnv::Flake(_) => require_file(&src.join("flake.nix"), "flake")?,
        DevEnv::Devenv(_) => {
            require_file(&src.join("devenv.yaml"), "devenv")?;
            if !which("devenv") {
                return Err("devenv CLI is required for --devenv but was not found on PATH".into());
            }
            if !src.join(".devenv/profile").is_symlink() {
                return Err(format!(
                    "no .devenv/profile found — run `devenv shell` in {} first",
                    src.display()
                )
                .into());
            }
        }
    }
    if !which("nix") {
        return Err("nix CLI is required for --devenv/--flake but was not found on PATH".into());
    }

    let current_hash = compute_hash(kind, &src)?;
    let cached_hash = fs::read_to_string(state.dev_env_hash()).ok();
    let needs_capture = cached_hash.as_deref() != Some(&current_hash)
        || !state.dev_env_sh().is_file()
        || !state.dev_closure_paths().is_file();

    if needs_capture {
        eprintln!("Capturing dev environment from {} ...", src.display());
        match kind {
            DevEnv::Flake(_) => capture_flake(&src, state)?,
            DevEnv::Devenv(_) => capture_devenv(&src, state)?,
        }
        fs::write(state.dev_env_hash(), &current_hash)?;
        let count = count_lines(&state.dev_closure_paths())?;
        eprintln!("Dev environment captured ({count} store paths).");
    }

    fs::write(state.dev_entrypoint_sh(), DEV_ENTRYPOINT)?;
    Ok(())
}

fn capture_flake(src: &Path, state: &State) -> Result<(), crate::Error> {
    // `nix print-dev-env path:<src>` → dev-env.sh
    let src_arg = format!("path:{}", src.display());
    let env_out = Command::new("nix")
        .args(["print-dev-env", &src_arg])
        .output()
        .map_err(|e| -> crate::Error { format!("`nix print-dev-env` failed to start: {e}").into() })?;
    if !env_out.status.success() {
        return Err(format!(
            "`nix print-dev-env` exited {}: {}",
            env_out.status,
            String::from_utf8_lossy(&env_out.stderr)
        )
        .into());
    }
    fs::write(state.dev_env_sh(), &env_out.stdout)?;

    // `nix build path:<src>#devShells.<sys>.default --no-link --print-out-paths`
    let system = nix_system()?;
    let target = format!("path:{}#devShells.{system}.default", src.display());
    let build_out = Command::new("nix")
        .args(["build", &target, "--no-link", "--print-out-paths"])
        .stderr(Stdio::null())
        .output()
        .map_err(|e| -> crate::Error { format!("`nix build` failed to start: {e}").into() })?;
    if !build_out.status.success() {
        return Err(format!(
            "`nix build {target}` exited {}",
            build_out.status,
        )
        .into());
    }
    let shell_out = String::from_utf8_lossy(&build_out.stdout)
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    if shell_out.is_empty() {
        return Err(format!("`nix build {target}` returned no output path").into());
    }
    write_closure(&shell_out, &state.dev_closure_paths())?;
    Ok(())
}

fn capture_devenv(src: &Path, state: &State) -> Result<(), crate::Error> {
    let tmp = tempfile::NamedTempFile::new()?;
    let tmp_path = tmp.path().to_path_buf();

    let devenv_bin = which_path("devenv")
        .ok_or_else(|| -> crate::Error { "devenv not on PATH".into() })?;
    let nix_bin = which_path("nix")
        .ok_or_else(|| -> crate::Error { "nix not on PATH".into() })?;
    let path = format!(
        "{}:{}:/run/current-system/sw/bin",
        devenv_bin.parent().unwrap().display(),
        nix_bin.parent().unwrap().display(),
    );

    // Rebuild the shell command exactly as package.nix did, via positional
    // args so $1/$2 land unescaped. Run with a clean env; only re-populate
    // what devenv itself needs.
    let src_str = src.to_string_lossy().to_string();
    let tmp_str = tmp_path.to_string_lossy().to_string();

    let mut cmd = Command::new("bash");
    cmd.env_clear()
        .env(
            "HOME",
            std::env::var_os("HOME").unwrap_or_default(),
        )
        .env(
            "USER",
            std::env::var_os("USER").unwrap_or_default(),
        )
        .env("PATH", &path)
        .env(
            "NIX_SSL_CERT_FILE",
            std::env::var_os("NIX_SSL_CERT_FILE")
                .unwrap_or_else(|| std::ffi::OsString::from("/etc/ssl/certs/ca-certificates.crt")),
        )
        .env(
            "LOCALE_ARCHIVE",
            std::env::var_os("LOCALE_ARCHIVE")
                .unwrap_or_else(|| {
                    std::ffi::OsString::from("/run/current-system/sw/lib/locale/locale-archive")
                }),
        )
        .args([
            "-c",
            "cd \"$1\" && devenv shell bash --norc --noprofile -c \"export -p > \\\"$2\\\"\"",
            "_",
            &src_str,
            &tmp_str,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let status = cmd
        .status()
        .map_err(|e| -> crate::Error { format!("failed to launch devenv shell: {e}").into() })?;
    if !status.success() {
        return Err(format!("`devenv shell` exited {}", status).into());
    }

    // Filter out container-managed / host-specific vars.
    let raw = fs::read_to_string(&tmp_path)?;
    let mut out = fs::File::create(state.dev_env_sh())?;
    for line in raw.lines() {
        if is_dropped(line) {
            continue;
        }
        out.write_all(line.as_bytes())?;
        out.write_all(b"\n")?;
    }

    // Closure via the built devenv profile (already on disk from `devenv shell`).
    let profile = src.join(".devenv/profile");
    let target = fs::canonicalize(&profile).map_err(|e| -> crate::Error {
        format!(
            "cannot readlink {}: {e} — run `devenv shell` in {} first",
            profile.display(),
            src.display()
        )
        .into()
    })?;
    write_closure(&target.to_string_lossy(), &state.dev_closure_paths())?;

    Ok(())
}

fn is_dropped(line: &str) -> bool {
    // Shell regex: ^declare -x (HOME|USER|TMPDIR|SHELL|SHLVL|PWD|OLDPWD|_|LOGNAME|HOSTNAME)=
    let prefix = "declare -x ";
    if !line.starts_with(prefix) {
        return false;
    }
    let rest = &line[prefix.len()..];
    DROP_VARS
        .iter()
        .any(|v| rest.as_bytes().starts_with(v.as_bytes()) && rest[v.len()..].starts_with('='))
}

fn write_closure(root: &str, out_path: &Path) -> Result<(), crate::Error> {
    let out = Command::new("nix")
        .args(["path-info", "-r", root])
        .output()
        .map_err(|e| -> crate::Error { format!("`nix path-info` failed: {e}").into() })?;
    if !out.status.success() {
        return Err(format!(
            "`nix path-info -r {root}` exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        )
        .into());
    }
    // `sort -u`: BTreeSet sorts + dedups.
    let paths: BTreeSet<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    let mut f = fs::File::create(out_path)?;
    for p in &paths {
        f.write_all(p.as_bytes())?;
        f.write_all(b"\n")?;
    }
    Ok(())
}

fn compute_hash(kind: &DevEnv, src: &Path) -> Result<String, crate::Error> {
    let mut input = String::new();
    match kind {
        DevEnv::Flake(_) => {
            let lock = src.join("flake.lock");
            if lock.is_file() {
                input.push_str(&hash_file(&lock)?);
            }
            let flake = src.join("flake.nix");
            if flake.is_file() {
                input.push_str(&hash_file(&flake)?);
            }
        }
        DevEnv::Devenv(_) => {
            let lock = src.join("devenv.lock");
            if lock.is_file() {
                input.push_str(&hash_file(&lock)?);
            }
            let profile = src.join(".devenv/profile");
            if profile.is_symlink() {
                let target = fs::canonicalize(&profile)?;
                input.push_str(&target.to_string_lossy());
            }
        }
    }
    Ok(hex::encode(sha2::Sha256::digest(input.as_bytes())))
}

fn hash_file(p: &Path) -> Result<String, crate::Error> {
    let bytes = fs::read(p)?;
    Ok(hex::encode(sha2::Sha256::digest(&bytes)))
}

fn require_file(p: &Path, kind: &str) -> Result<(), crate::Error> {
    if !p.is_file() {
        return Err(format!("no {kind} source found at {}", p.display()).into());
    }
    Ok(())
}

fn which(cmd: &str) -> bool {
    which_path(cmd).is_some()
}

fn which_path(cmd: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(cmd);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn nix_system() -> Result<String, crate::Error> {
    let out = Command::new("uname").arg("-m").output().map_err(|e| -> crate::Error {
        format!("`uname -m` failed: {e}").into()
    })?;
    if !out.status.success() {
        return Err(format!("`uname -m` exited {}", out.status).into());
    }
    let arch = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Ok(format!("{arch}-linux"))
}

fn count_lines(p: &Path) -> Result<usize, crate::Error> {
    Ok(fs::read_to_string(p)?.lines().count())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_only_fixed_vars() {
        assert!(is_dropped("declare -x HOME=/root"));
        assert!(is_dropped("declare -x SHLVL=1"));
        assert!(is_dropped("declare -x _=/usr/bin/env"));
        assert!(!is_dropped("declare -x PATH=/usr/bin"));
        assert!(!is_dropped("declare -x HOMED=/root")); // prefix-only match must not trip HOMED
        assert!(!is_dropped("declare -x HOME_BASE=/root"));
        assert!(!is_dropped("# comment"));
        assert!(!is_dropped(""));
    }

    #[test]
    fn write_closure_dedups_and_sorts() {
        // Can't invoke `nix path-info` in sandbox, so exercise the filter
        // with a synthetic helper: mirror write_closure's sort/dedup step.
        let inputs = ["/nix/store/b", "/nix/store/a", "/nix/store/b", ""];
        let set: BTreeSet<&str> = inputs.iter().copied().filter(|s| !s.is_empty()).collect();
        let got: Vec<&str> = set.into_iter().collect();
        assert_eq!(got, vec!["/nix/store/a", "/nix/store/b"]);
    }
}

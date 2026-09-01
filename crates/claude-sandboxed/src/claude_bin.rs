//! Provision the Claude Code binary the sandbox runs.
//!
//! The image bakes in nixpkgs' `claude-code` at `/bin/claude`, but nixpkgs
//! lags upstream. By default the launcher instead downloads Anthropic's
//! standalone release binary (the same artifact `claude.ai/install.sh`
//! installs), verifies it against the release manifest's sha256, caches it
//! under `$XDG_CACHE_HOME/claude-sandboxed/claude-bin/<version>/claude`, and
//! bind-mounts it read-only over `/usr/local/bin/claude` — which shadows
//! `/bin/claude` because the image's PATH puts `/usr/local/bin` first.
//!
//! Version selection is sticky per sandbox: the version resolved at first
//! launch is recorded in the state dir (`claude-version`) and reused until
//! `--update-claude` re-resolves the channel. `--pinned-claude` (or
//! `claude_bin = "nixpkgs"` in config) skips all of this and runs the baked
//! nixpkgs binary.
//!
//! Downloads shell out to host `curl` rather than pulling a TLS-capable HTTP
//! client into the crate — same trade as `subscription.rs` makes for plain
//! HTTP, except here TLS is mandatory so we lean on the host tool the way we
//! already do for `podman` and `systemctl`.
//!
//! Every network failure degrades, never aborts: recorded-and-cached version
//! first, then the newest cached version, then the nixpkgs binary — each
//! step with a stderr warning. A sandbox must always be able to launch
//! offline.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// Release endpoint used by Anthropic's official installer. `{BASE}/{channel}`
/// answers a bare version string; `{BASE}/{version}/manifest.json` carries
/// per-platform sha256 checksums; `{BASE}/{version}/{platform}/claude` is the
/// standalone binary.
const DOWNLOAD_BASE: &str = "https://downloads.claude.ai/claude-code-releases";

/// Timeout for the small metadata fetches (channel version, manifest).
const METADATA_TIMEOUT_SECS: u32 = 15;

/// Timeout for the binary download itself (tens of MB; generous but bounded
/// so a stalled CDN connection can't hang the launch forever).
const BINARY_TIMEOUT_SECS: u32 = 300;

/// Which binary the sandbox should run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Download/cache the upstream standalone binary (default).
    Upstream,
    /// Use the nixpkgs binary baked into the image at `/bin/claude`.
    Nixpkgs,
}

/// Upstream release channel to resolve versions from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    /// Newest release (default — matches npm's `latest`).
    Latest,
    /// Anthropic's slower-moving stable channel.
    Stable,
}

impl Channel {
    fn as_str(self) -> &'static str {
        match self {
            Channel::Latest => "latest",
            Channel::Stable => "stable",
        }
    }
}

/// Parse the config `claude_bin` value. `None` (key absent) means upstream.
pub fn parse_mode(s: Option<&str>) -> Result<Mode, crate::Error> {
    match s {
        None | Some("upstream") => Ok(Mode::Upstream),
        Some("nixpkgs") => Ok(Mode::Nixpkgs),
        Some(other) => Err(format!(
            "invalid claude_bin value `{other}` in config.toml (expected \"upstream\" or \"nixpkgs\")"
        )
        .into()),
    }
}

/// Parse the config `claude_channel` value. `None` (key absent) means latest.
pub fn parse_channel(s: Option<&str>) -> Result<Channel, crate::Error> {
    match s {
        None | Some("latest") => Ok(Channel::Latest),
        Some("stable") => Ok(Channel::Stable),
        Some(other) => Err(format!(
            "invalid claude_channel value `{other}` in config.toml (expected \"latest\" or \"stable\")"
        )
        .into()),
    }
}

/// The user-wide download cache. Versions are immutable once verified, so
/// this is shared across every sandbox: one download per version, not per
/// sandbox. `None` when no home directory is known (the caller degrades to
/// the nixpkgs binary).
pub fn cache_root() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(xdg).join("claude-sandboxed").join("claude-bin"));
    }
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(|h| PathBuf::from(h).join(".cache").join("claude-sandboxed").join("claude-bin"))
}

/// Resolve the host path of the binary to bind-mount into the sandbox, or
/// `None` for the baked nixpkgs binary. Performs network work (channel
/// resolution / download) only when needed; never fails the launch on
/// network errors — those degrade with a warning.
pub fn provision(
    mode: Mode,
    channel: Channel,
    update: bool,
    version_file: &Path,
    cache_root: &Path,
) -> Result<Option<PathBuf>, crate::Error> {
    provision_with(&Curl, mode, channel, update, version_file, cache_root)
}

/// Network operations, separated so the orchestration in [`provision_with`]
/// is testable without curl or a network.
trait Net {
    /// `{BASE}/{channel}` → version string (unvalidated).
    fn channel_version(&self, channel: Channel) -> Result<String, String>;
    /// `{BASE}/{version}/manifest.json` → raw JSON bytes.
    fn manifest(&self, version: &str) -> Result<Vec<u8>, String>;
    /// `{BASE}/{version}/{platform}/claude` → written to `dest`.
    fn download_binary(&self, version: &str, platform: &str, dest: &Path) -> Result<(), String>;
}

/// Shell out to host `curl`. `-f` turns HTTP errors into exit failures,
/// `--proto =https` refuses any downgrade, `--max-time` bounds the launch
/// path.
struct Curl;

impl Curl {
    fn get(&self, url: &str, output: Option<&Path>, max_time: u32) -> Result<Vec<u8>, String> {
        let mut cmd = std::process::Command::new("curl");
        cmd.args(["-fsSL", "--proto", "=https", "--max-time", &max_time.to_string()]);
        if let Some(dest) = output {
            cmd.arg("-o").arg(dest);
        }
        cmd.arg(url);
        let out = cmd
            .output()
            .map_err(|e| format!("running curl: {e} (is curl installed on the host?)"))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            return Err(format!("curl {url}: {}", err.trim()));
        }
        Ok(out.stdout)
    }
}

impl Net for Curl {
    fn channel_version(&self, channel: Channel) -> Result<String, String> {
        let raw = self.get(
            &format!("{DOWNLOAD_BASE}/{}", channel.as_str()),
            None,
            METADATA_TIMEOUT_SECS,
        )?;
        Ok(String::from_utf8_lossy(&raw).trim().to_string())
    }

    fn manifest(&self, version: &str) -> Result<Vec<u8>, String> {
        self.get(
            &format!("{DOWNLOAD_BASE}/{version}/manifest.json"),
            None,
            METADATA_TIMEOUT_SECS,
        )
    }

    fn download_binary(&self, version: &str, platform: &str, dest: &Path) -> Result<(), String> {
        self.get(
            &format!("{DOWNLOAD_BASE}/{version}/{platform}/claude"),
            Some(dest),
            BINARY_TIMEOUT_SECS,
        )?;
        Ok(())
    }
}

fn warn(msg: &str) {
    eprintln!("claude-sandboxed: {msg}");
}

fn provision_with(
    net: &dyn Net,
    mode: Mode,
    channel: Channel,
    update: bool,
    version_file: &Path,
    cache_root: &Path,
) -> Result<Option<PathBuf>, crate::Error> {
    if mode == Mode::Nixpkgs {
        return Ok(None);
    }
    let Some(platform) = platform() else {
        warn("unsupported architecture for upstream claude binary; using the nixpkgs binary");
        return Ok(None);
    };

    let recorded = read_recorded(version_file);

    // Decide the version we *want*: the recorded one, unless this launch
    // should (re-)resolve the channel — first init or --update-claude.
    let desired = if update || recorded.is_none() {
        match net.channel_version(channel) {
            Ok(v) if valid_version(&v) => Some(v),
            Ok(v) => {
                warn(&format!(
                    "release channel `{}` answered garbage (`{v}`); ignoring",
                    channel.as_str()
                ));
                recorded.clone()
            }
            Err(e) => {
                warn(&format!(
                    "could not resolve claude {} version: {e}",
                    channel.as_str()
                ));
                recorded.clone()
            }
        }
    } else {
        recorded.clone()
    };

    if let Some(version) = &desired {
        match ensure_cached(net, version, &platform, cache_root) {
            Ok(bin) => {
                if recorded.as_deref() != Some(version.as_str()) {
                    record(version_file, version);
                }
                return Ok(Some(bin));
            }
            Err(e) => warn(&format!("could not provision claude {version}: {e}")),
        }
    }

    // Desired version unreachable (or unresolvable): newest cached version.
    // Deliberately NOT recorded — leaving the version file as-is means the
    // next launch retries the real resolution instead of pinning to the
    // emergency fallback.
    if let Some((version, bin)) = newest_cached(cache_root) {
        warn(&format!("falling back to cached claude {version}"));
        return Ok(Some(bin));
    }

    warn("falling back to the nixpkgs claude binary baked into the image");
    Ok(None)
}

/// Upstream platform key for this host. The container runs the host's
/// architecture (rootless podman, no emulation), and the image is glibc-based
/// — never the musl variant.
fn platform() -> Option<String> {
    match std::env::consts::ARCH {
        "x86_64" => Some("linux-x64".into()),
        "aarch64" => Some("linux-arm64".into()),
        _ => None,
    }
}

/// A version string acceptable as both a semver and a cache directory name:
/// `digits.digits.digits` with an optional `[0-9A-Za-z.-]` pre-release
/// suffix. Anything else (HTML error pages, path traversal) is rejected.
fn valid_version(s: &str) -> bool {
    parse_version(s).is_some()
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
}

/// Extract the numeric `(major, minor, patch)` for ordering. `None` when the
/// string doesn't start with three dot-separated numbers.
fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let mut it = s.split(['.', '-']);
    let major = it.next()?.parse().ok()?;
    let minor = it.next()?.parse().ok()?;
    let patch = it.next()?.parse().ok()?;
    Some((major, minor, patch))
}

fn read_recorded(version_file: &Path) -> Option<String> {
    let raw = fs::read_to_string(version_file).ok()?;
    let v = raw.trim().to_string();
    if valid_version(&v) {
        Some(v)
    } else {
        None
    }
}

fn record(version_file: &Path, version: &str) {
    if let Err(e) = fs::write(version_file, format!("{version}\n")) {
        // Non-fatal: the sandbox still launches with the right binary this
        // time; it just re-resolves next launch.
        warn(&format!(
            "could not record claude version at {}: {e}",
            version_file.display()
        ));
    }
}

/// Return the cached binary for `version`, downloading and verifying it
/// first when absent.
fn ensure_cached(
    net: &dyn Net,
    version: &str,
    platform: &str,
    cache_root: &Path,
) -> Result<PathBuf, String> {
    let dir = cache_root.join(version);
    let bin = dir.join("claude");
    if bin.is_file() {
        return Ok(bin);
    }

    let manifest = net.manifest(version)?;
    let checksum = manifest_checksum(&manifest, platform)?;

    fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    // Per-process partial name + atomic rename: two concurrent launchers can
    // download the same version without trampling each other, and a killed
    // download never leaves a half-written `claude` behind.
    let partial = dir.join(format!(".claude.partial-{}", std::process::id()));
    eprintln!("claude-sandboxed: downloading claude {version} ({platform})...");
    let result = (|| {
        net.download_binary(version, platform, &partial)?;
        let actual = sha256_file(&partial)?;
        if actual != checksum {
            return Err(format!(
                "checksum mismatch for claude {version}: manifest says {checksum}, got {actual}"
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&partial, fs::Permissions::from_mode(0o755))
                .map_err(|e| format!("chmod {}: {e}", partial.display()))?;
        }
        fs::rename(&partial, &bin).map_err(|e| format!("installing {}: {e}", bin.display()))?;
        Ok(bin.clone())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&partial);
    }
    result
}

/// Pull `platforms[platform].checksum` out of the release manifest and
/// validate it looks like a sha256.
fn manifest_checksum(manifest: &[u8], platform: &str) -> Result<String, String> {
    let v: serde_json::Value =
        serde_json::from_slice(manifest).map_err(|e| format!("unparseable manifest: {e}"))?;
    let checksum = v["platforms"][platform]["checksum"]
        .as_str()
        .ok_or_else(|| format!("platform {platform} not in release manifest"))?;
    if checksum.len() != 64 || !checksum.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("malformed checksum in manifest: {checksum}"));
    }
    Ok(checksum.to_ascii_lowercase())
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut f = fs::File::open(path).map_err(|e| format!("opening {}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f
            .read(&mut buf)
            .map_err(|e| format!("reading {}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Newest complete `<version>/claude` under the cache, by semver ordering.
fn newest_cached(cache_root: &Path) -> Option<(String, PathBuf)> {
    let entries = fs::read_dir(cache_root).ok()?;
    let mut best: Option<((u64, u64, u64), String, PathBuf)> = None;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(key) = parse_version(name) else { continue };
        let bin = entry.path().join("claude");
        if !bin.is_file() {
            continue;
        }
        if best.as_ref().map_or(true, |(k, _, _)| key > *k) {
            best = Some((key, name.to_string(), bin));
        }
    }
    best.map(|(_, v, p)| (v, p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_and_channel_parse_and_reject() {
        assert_eq!(parse_mode(None).unwrap(), Mode::Upstream);
        assert_eq!(parse_mode(Some("upstream")).unwrap(), Mode::Upstream);
        assert_eq!(parse_mode(Some("nixpkgs")).unwrap(), Mode::Nixpkgs);
        assert!(parse_mode(Some("npm")).is_err());
        assert_eq!(parse_channel(None).unwrap(), Channel::Latest);
        assert_eq!(parse_channel(Some("stable")).unwrap(), Channel::Stable);
        assert!(parse_channel(Some("nightly")).is_err());
    }

    #[test]
    fn version_validation_rejects_garbage_and_traversal() {
        assert!(valid_version("2.1.257"));
        assert!(valid_version("2.1.257-beta.1"));
        assert!(!valid_version(""));
        assert!(!valid_version("2.1"));
        assert!(!valid_version("<html>region blocked</html>"));
        assert!(!valid_version("../../../etc/passwd"));
        assert!(!valid_version("2.1.257/x"));
    }

    #[test]
    fn version_ordering_is_numeric_not_lexicographic() {
        assert!(parse_version("2.1.257").unwrap() > parse_version("2.1.36").unwrap());
        assert!(parse_version("10.0.0").unwrap() > parse_version("9.9.9").unwrap());
    }

    #[test]
    fn manifest_checksum_extracts_and_validates() {
        let manifest = br#"{"platforms":{"linux-x64":{"checksum":"6c8818fa22187aa555c242be4abbacc44d6b71a32ac9631ee7b2b5d12f51f752","size":1}}}"#;
        assert_eq!(
            manifest_checksum(manifest, "linux-x64").unwrap(),
            "6c8818fa22187aa555c242be4abbacc44d6b71a32ac9631ee7b2b5d12f51f752"
        );
        assert!(manifest_checksum(manifest, "linux-arm64").is_err());
        assert!(manifest_checksum(br#"{"platforms":{"linux-x64":{"checksum":"deadbeef"}}}"#, "linux-x64").is_err());
        assert!(manifest_checksum(b"not json", "linux-x64").is_err());
    }

    #[test]
    fn newest_cached_picks_highest_complete_version() {
        let tmp = tempfile::tempdir().unwrap();
        for v in ["2.1.36", "2.1.257", "2.1.999"] {
            fs::create_dir_all(tmp.path().join(v)).unwrap();
        }
        // 2.1.999 has no binary (interrupted download) — must be skipped.
        fs::write(tmp.path().join("2.1.36/claude"), "old").unwrap();
        fs::write(tmp.path().join("2.1.257/claude"), "new").unwrap();
        let (v, p) = newest_cached(tmp.path()).unwrap();
        assert_eq!(v, "2.1.257");
        assert!(p.ends_with("2.1.257/claude"));
        assert!(newest_cached(&tmp.path().join("missing")).is_none());
    }

    /// Scripted fake network for the orchestration tests.
    struct FakeNet {
        version: Result<String, String>,
        body: Vec<u8>,
    }

    impl FakeNet {
        fn serving(version: &str, body: &[u8]) -> Self {
            FakeNet { version: Ok(version.into()), body: body.to_vec() }
        }
        fn offline() -> Self {
            FakeNet { version: Err("no route".into()), body: Vec::new() }
        }
        fn manifest_for(body: &[u8]) -> String {
            let mut hasher = Sha256::new();
            hasher.update(body);
            format!(
                r#"{{"platforms":{{"{}":{{"checksum":"{}"}}}}}}"#,
                platform().unwrap(),
                hex::encode(hasher.finalize())
            )
        }
    }

    impl Net for FakeNet {
        fn channel_version(&self, _: Channel) -> Result<String, String> {
            self.version.clone()
        }
        fn manifest(&self, _: &str) -> Result<Vec<u8>, String> {
            if self.body.is_empty() {
                return Err("no route".into());
            }
            Ok(Self::manifest_for(&self.body).into_bytes())
        }
        fn download_binary(&self, _: &str, _: &str, dest: &Path) -> Result<(), String> {
            if self.body.is_empty() {
                return Err("no route".into());
            }
            fs::write(dest, &self.body).map_err(|e| e.to_string())
        }
    }

    fn setup() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let vf = tmp.path().join("claude-version");
        let cache = tmp.path().join("cache");
        (tmp, vf, cache)
    }

    #[test]
    fn nixpkgs_mode_short_circuits() {
        let (_tmp, vf, cache) = setup();
        let got =
            provision_with(&FakeNet::offline(), Mode::Nixpkgs, Channel::Latest, false, &vf, &cache)
                .unwrap();
        assert_eq!(got, None);
        assert!(!vf.exists());
    }

    #[test]
    fn first_init_resolves_downloads_and_records() {
        let (_tmp, vf, cache) = setup();
        let net = FakeNet::serving("2.1.257", b"fake-binary");
        let got = provision_with(&net, Mode::Upstream, Channel::Latest, false, &vf, &cache)
            .unwrap()
            .unwrap();
        assert!(got.ends_with("2.1.257/claude"));
        assert_eq!(fs::read_to_string(&got).unwrap(), "fake-binary");
        assert_eq!(fs::read_to_string(&vf).unwrap(), "2.1.257\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&got).unwrap().permissions().mode() & 0o777, 0o755);
        }
    }

    #[test]
    fn recorded_version_is_sticky_without_update() {
        let (_tmp, vf, cache) = setup();
        // Sandbox recorded 2.1.100 and it's cached; the channel now serves
        // 2.1.257. Without --update-claude the recorded version wins and no
        // resolution happens (FakeNet would happily answer — the assertion is
        // on the returned path).
        fs::create_dir_all(cache.join("2.1.100")).unwrap();
        fs::write(cache.join("2.1.100/claude"), "pinned").unwrap();
        fs::write(&vf, "2.1.100\n").unwrap();
        let net = FakeNet::serving("2.1.257", b"newer");
        let got = provision_with(&net, Mode::Upstream, Channel::Latest, false, &vf, &cache)
            .unwrap()
            .unwrap();
        assert!(got.ends_with("2.1.100/claude"));
        assert_eq!(fs::read_to_string(&vf).unwrap(), "2.1.100\n");
    }

    #[test]
    fn update_flag_moves_to_the_channel_version() {
        let (_tmp, vf, cache) = setup();
        fs::create_dir_all(cache.join("2.1.100")).unwrap();
        fs::write(cache.join("2.1.100/claude"), "pinned").unwrap();
        fs::write(&vf, "2.1.100\n").unwrap();
        let net = FakeNet::serving("2.1.257", b"newer");
        let got = provision_with(&net, Mode::Upstream, Channel::Latest, true, &vf, &cache)
            .unwrap()
            .unwrap();
        assert!(got.ends_with("2.1.257/claude"));
        assert_eq!(fs::read_to_string(&vf).unwrap(), "2.1.257\n");
    }

    #[test]
    fn offline_with_recorded_cache_uses_it() {
        let (_tmp, vf, cache) = setup();
        fs::create_dir_all(cache.join("2.1.100")).unwrap();
        fs::write(cache.join("2.1.100/claude"), "pinned").unwrap();
        fs::write(&vf, "2.1.100\n").unwrap();
        let got =
            provision_with(&FakeNet::offline(), Mode::Upstream, Channel::Latest, true, &vf, &cache)
                .unwrap()
                .unwrap();
        assert!(got.ends_with("2.1.100/claude"));
    }

    #[test]
    fn offline_first_init_falls_back_to_newest_cached_without_recording() {
        let (_tmp, vf, cache) = setup();
        fs::create_dir_all(cache.join("2.1.90")).unwrap();
        fs::write(cache.join("2.1.90/claude"), "older").unwrap();
        let got =
            provision_with(&FakeNet::offline(), Mode::Upstream, Channel::Latest, false, &vf, &cache)
                .unwrap()
                .unwrap();
        assert!(got.ends_with("2.1.90/claude"));
        // Not recorded: next launch should retry the real resolution.
        assert!(!vf.exists());
    }

    #[test]
    fn offline_empty_cache_degrades_to_nixpkgs() {
        let (_tmp, vf, cache) = setup();
        let got =
            provision_with(&FakeNet::offline(), Mode::Upstream, Channel::Latest, false, &vf, &cache)
                .unwrap();
        assert_eq!(got, None);
    }

    #[test]
    fn checksum_mismatch_is_rejected_and_cleaned_up() {
        let (_tmp, vf, cache) = setup();
        // Manifest checksum computed over different bytes than the download.
        struct Lying;
        impl Net for Lying {
            fn channel_version(&self, _: Channel) -> Result<String, String> {
                Ok("2.1.257".into())
            }
            fn manifest(&self, _: &str) -> Result<Vec<u8>, String> {
                Ok(FakeNet::manifest_for(b"the-real-bytes").into_bytes())
            }
            fn download_binary(&self, _: &str, _: &str, dest: &Path) -> Result<(), String> {
                fs::write(dest, b"tampered-bytes").map_err(|e| e.to_string())
            }
        }
        let got = provision_with(&Lying, Mode::Upstream, Channel::Latest, false, &vf, &cache)
            .unwrap();
        // No cache to fall back to → nixpkgs, and nothing half-written left.
        assert_eq!(got, None);
        assert!(!cache.join("2.1.257/claude").exists());
        let leftovers: Vec<_> = fs::read_dir(cache.join("2.1.257"))
            .map(|d| d.flatten().collect())
            .unwrap_or_default();
        assert!(leftovers.is_empty(), "partial download left behind: {leftovers:?}");
    }

    #[test]
    fn garbage_channel_answer_is_ignored() {
        let (_tmp, vf, cache) = setup();
        let net = FakeNet::serving("<html>error</html>", b"");
        let got = provision_with(&net, Mode::Upstream, Channel::Latest, false, &vf, &cache)
            .unwrap();
        assert_eq!(got, None);
        assert!(!vf.exists());
    }
}

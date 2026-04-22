//! Token store: sandbox-facing bearer tokens, flock-coordinated.
//!
//! Stored at a service-user-owned JSON file with byte-identical schema to
//! the Python version so an existing `tokens.json` is a drop-in on upgrade:
//!
//! ```json
//! { "tokens": [
//!     { "id": "hex4", "name": "...", "hash": "<sha256hex>",
//!       "created_at": <unix>, "revoked_at": null | <unix> } ] }
//! ```
//!
//! Discipline:
//!   mint / revoke  → open O_RDWR, flock(LOCK_EX), read, mutate, write-to-
//!                    sibling-tmp + fsync + rename, unlock.
//!   list / reload  → open O_RDONLY, flock(LOCK_SH), read, unlock.
//! A running `serve` never holds the lock between requests; it mtime-polls
//! the file on each auth check and only reloads on change. `TokenAuth::check`
//! is the hot path and takes a parking_lot-style `std::sync::Mutex` just
//! long enough to swap the cache dict.

use std::{
    fs,
    io::{self, Read, Write},
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    path::{Path, PathBuf},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    cli::{ListArgs, MintArgs, RevokeArgs},
    config::SystemConfig,
};

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TokenEntry {
    pub id: String,
    #[serde(default)]
    pub name: String,
    pub hash: String,
    pub created_at: i64,
    #[serde(default)]
    pub revoked_at: Option<i64>,
    /// Forward-compat: any extra keys in the on-disk JSON survive a round-trip.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Default, Clone, Debug, Serialize, Deserialize)]
pub struct Store {
    #[serde(default)]
    pub tokens: Vec<TokenEntry>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

fn sha256_hex(token: &str) -> String {
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    hex::encode(h.finalize())
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn rand_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

// ---------------------------------------------------------------------------
// Locked I/O
// ---------------------------------------------------------------------------

/// RAII wrapper: holds an fd with an exclusive or shared flock, releases on drop.
struct LockedFile {
    file: fs::File,
}

impl LockedFile {
    fn open_for_mutate(path: &Path) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        // Race-safe init: O_CREAT without O_EXCL, then flock, then check size
        // under lock and init an empty store if the file is brand new. Two
        // concurrent `mint`s on a fresh install would otherwise race between
        // "file exists, size 0" and "file exists, size N".
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            // Never truncate — we read-modify-write under LOCK_EX, so we
            // need the current content on entry. Atomic rename handles the
            // actual "swap in new bytes" step.
            .truncate(false)
            .mode(0o600)
            .open(path)?;
        flock(&file, libc::LOCK_EX)?;
        let size = file.metadata()?.len();
        if size == 0 {
            // Write the bootstrap shape. No truncate needed (file is new / empty).
            let initial = b"{\"tokens\": []}\n";
            (&file).write_all(initial)?;
        }
        Ok(Self { file })
    }

    fn open_for_read(path: &Path) -> io::Result<Self> {
        let file = fs::OpenOptions::new().read(true).write(true).open(path)?;
        flock(&file, libc::LOCK_SH)?;
        Ok(Self { file })
    }

    fn read_all(&mut self) -> io::Result<String> {
        use std::io::Seek;
        self.file.seek(io::SeekFrom::Start(0))?;
        let mut buf = String::new();
        self.file.read_to_string(&mut buf)?;
        Ok(buf)
    }
}

impl Drop for LockedFile {
    fn drop(&mut self) {
        // Best-effort unlock; fd close also releases but explicit is clearer.
        let _ = flock(&self.file, libc::LOCK_UN);
    }
}

fn flock(f: &fs::File, op: libc::c_int) -> io::Result<()> {
    // SAFETY: flock(2) is a straightforward syscall. The fd is valid for
    // the lifetime of the borrow.
    let r = unsafe { libc::flock(f.as_raw_fd(), op) };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn parse_store(raw: &str) -> Result<Store, crate::Error> {
    if raw.trim().is_empty() {
        return Ok(Store::default());
    }
    serde_json::from_str(raw).map_err(|e| format!("token store parse error: {e}").into())
}

fn write_store_atomic(path: &Path, store: &Store) -> Result<(), crate::Error> {
    let tmp = path.with_extension(
        // append ".tmp" to the existing extension (or set it if none) — mirrors
        // the Python `f"{path}.tmp"` convention.
        match path.extension() {
            Some(e) => format!("{}.tmp", e.to_string_lossy()),
            None => "tmp".to_string(),
        },
    );
    {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        serde_json::to_writer_pretty(&mut f, store)?;
        f.flush()?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// CLI subcommands
// ---------------------------------------------------------------------------

fn store_path_or_die(flag: Option<PathBuf>, config: &SystemConfig) -> Result<PathBuf, crate::Error> {
    config.token_store_path(flag).ok_or_else(|| {
        "error: --token-store is required (no /etc/claude-proxy/config.json and \
         CLAUDE_PROXY_TOKEN_STORE is unset)"
            .into()
    })
}

pub fn mint(args: MintArgs, config: &SystemConfig) -> Result<u8, crate::Error> {
    let path = store_path_or_die(args.token_store, config)?;
    let mut lf = LockedFile::open_for_mutate(&path)?;
    let raw = lf.read_all()?;
    let mut store = parse_store(&raw)?;
    let token = rand_hex(32);
    let entry = TokenEntry {
        id: rand_hex(4),
        name: args.name.unwrap_or_default(),
        hash: sha256_hex(&token),
        created_at: now_unix(),
        revoked_at: None,
        extra: Default::default(),
    };
    let id = entry.id.clone();
    let label = if entry.name.is_empty() {
        "<none>".to_string()
    } else {
        entry.name.clone()
    };
    store.tokens.push(entry);
    write_store_atomic(&path, &store)?;
    drop(lf); // release lock before any I/O the caller might race on
    println!("{token}");
    eprintln!("(id: {id}, name: {label})");
    Ok(0)
}

pub fn list(args: ListArgs, config: &SystemConfig) -> Result<u8, crate::Error> {
    let path = store_path_or_die(args.token_store, config)?;
    let mut lf = LockedFile::open_for_read(&path)?;
    let raw = lf.read_all()?;
    drop(lf);
    let store = parse_store(&raw)?;
    if store.tokens.is_empty() {
        println!("(no tokens)");
        return Ok(0);
    }
    println!("{:<10} {:<20} {:<20} STATUS", "ID", "NAME", "CREATED");
    for t in &store.tokens {
        let created = format_local_time(t.created_at);
        let status = if t.revoked_at.is_some() { "revoked" } else { "active" };
        println!("{:<10} {:<20} {:<20} {}", t.id, t.name, created, status);
    }
    Ok(0)
}

pub fn revoke(args: RevokeArgs, config: &SystemConfig) -> Result<u8, crate::Error> {
    let path = store_path_or_die(args.token_store, config)?;
    let mut lf = LockedFile::open_for_mutate(&path)?;
    let raw = lf.read_all()?;
    let mut store = parse_store(&raw)?;
    let Some(entry) = store.tokens.iter_mut().find(|t| t.id == args.id) else {
        eprintln!("token {} not found", args.id);
        return Ok(1);
    };
    if entry.revoked_at.is_some() {
        eprintln!("token {} already revoked", args.id);
        return Ok(1);
    }
    entry.revoked_at = Some(now_unix());
    write_store_atomic(&path, &store)?;
    drop(lf);
    println!("revoked {}", args.id);
    Ok(0)
}

// ---------------------------------------------------------------------------
// TokenAuth — server-side cache, mtime-gated reload
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Cache {
    mtime_nanos: i128,
    by_hash: std::collections::HashMap<String, CachedEntry>,
}

struct CachedEntry {
    revoked: bool,
}

pub struct TokenAuth {
    store_path: Option<PathBuf>,
    inner: Mutex<Cache>,
}

impl TokenAuth {
    /// Ephemeral mode: a single accepted token, no backing file.
    pub fn ephemeral(token: &str) -> Self {
        let mut by_hash = std::collections::HashMap::new();
        by_hash.insert(sha256_hex(token), CachedEntry { revoked: false });
        Self {
            store_path: None,
            inner: Mutex::new(Cache { mtime_nanos: 0, by_hash }),
        }
    }

    /// Persistent mode: reload from disk when the store's mtime changes.
    pub fn from_store(path: PathBuf) -> Result<Self, crate::Error> {
        let auth = Self {
            store_path: Some(path),
            inner: Mutex::new(Cache::default()),
        };
        auth.reload()?;
        Ok(auth)
    }

    fn reload(&self) -> Result<(), crate::Error> {
        let Some(path) = self.store_path.as_ref() else {
            return Ok(());
        };
        let mut lf = LockedFile::open_for_read(path)?;
        let raw = lf.read_all()?;
        drop(lf);
        let store = parse_store(&raw)?;
        let mtime_nanos = mtime_nanos(path).unwrap_or(0);
        let mut by_hash = std::collections::HashMap::with_capacity(store.tokens.len());
        for t in store.tokens {
            by_hash.insert(t.hash, CachedEntry { revoked: t.revoked_at.is_some() });
        }
        let mut guard = self.inner.lock().unwrap();
        guard.mtime_nanos = mtime_nanos;
        guard.by_hash = by_hash;
        Ok(())
    }

    fn maybe_reload(&self) {
        let Some(path) = self.store_path.as_ref() else {
            return;
        };
        let current = mtime_nanos(path).unwrap_or(0);
        // Check-then-act with a short critical section: worst case multiple
        // threads see a change and all call reload(); reload() is idempotent.
        let stale = {
            let guard = self.inner.lock().unwrap();
            current != guard.mtime_nanos
        };
        if stale {
            if let Err(e) = self.reload() {
                eprintln!("[auth-proxy] token store reload failed: {e}");
            }
        }
    }

    pub fn check(&self, token: Option<&str>) -> bool {
        let Some(tok) = token else {
            return false;
        };
        if tok.is_empty() {
            return false;
        }
        self.maybe_reload();
        let guard = self.inner.lock().unwrap();
        match guard.by_hash.get(&sha256_hex(tok)) {
            Some(entry) => !entry.revoked,
            None => false,
        }
    }
}

fn mtime_nanos(path: &Path) -> Option<i128> {
    let md = fs::metadata(path).ok()?;
    let m = md.modified().ok()?;
    let d = m.duration_since(UNIX_EPOCH).ok()?;
    Some(d.as_nanos() as i128)
}

fn format_local_time(ts: i64) -> String {
    // `chrono::Local` handles tz lookup and strftime-style formatting without
    // any unsafe blocks. Fall back to the raw unix timestamp if the value is
    // outside chrono's representable range (impossible in practice for an
    // i64 coming out of `SystemTime::now()`, but we keep the guard for parity
    // with the previous implementation).
    match chrono::DateTime::from_timestamp(ts, 0) {
        Some(dt) => dt
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string(),
        None => ts.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_python() {
        // `hashlib.sha256(b"hello").hexdigest()` in Python — byte-identical.
        assert_eq!(
            sha256_hex("hello"),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn store_round_trip_preserves_unknown_keys() {
        let raw = r#"{
            "tokens": [
                {"id": "abcd1234", "name": "x", "hash": "deadbeef",
                 "created_at": 1700000000, "revoked_at": null, "future_field": "ok"}
            ],
            "schema_version": 2
        }"#;
        let store = parse_store(raw).unwrap();
        let out = serde_json::to_string(&store).unwrap();
        assert!(out.contains("\"future_field\":\"ok\""));
        assert!(out.contains("\"schema_version\":2"));
    }

    #[test]
    fn ephemeral_check() {
        let a = TokenAuth::ephemeral("secret");
        assert!(a.check(Some("secret")));
        assert!(!a.check(Some("wrong")));
        assert!(!a.check(None));
        assert!(!a.check(Some("")));
    }

    #[test]
    fn mint_list_revoke_roundtrip() {
        let dir = tempdir();
        let store = dir.join("tokens.json");
        // mint (directly, bypassing clap)
        let args = MintArgs {
            token_store: Some(store.clone()),
            name: Some("t1".into()),
        };
        let cfg = SystemConfig::default();
        // mint writes to stdout/stderr; we just care about exit code + file.
        let code = mint(args, &cfg).unwrap();
        assert_eq!(code, 0);
        let raw = std::fs::read_to_string(&store).unwrap();
        let parsed = parse_store(&raw).unwrap();
        assert_eq!(parsed.tokens.len(), 1);
        assert_eq!(parsed.tokens[0].name, "t1");
        assert!(parsed.tokens[0].revoked_at.is_none());

        let id = parsed.tokens[0].id.clone();
        let code = revoke(
            RevokeArgs {
                token_store: Some(store.clone()),
                id: id.clone(),
            },
            &cfg,
        )
        .unwrap();
        assert_eq!(code, 0);
        let parsed = parse_store(&std::fs::read_to_string(&store).unwrap()).unwrap();
        assert!(parsed.tokens[0].revoked_at.is_some());

        // Revoking an already-revoked token returns 1.
        let code = revoke(
            RevokeArgs {
                token_store: Some(store.clone()),
                id,
            },
            &cfg,
        )
        .unwrap();
        assert_eq!(code, 1);

        // Revoking a non-existent id returns 1.
        let code = revoke(
            RevokeArgs {
                token_store: Some(store),
                id: "00000000".into(),
            },
            &cfg,
        )
        .unwrap();
        assert_eq!(code, 1);
    }

    #[test]
    fn token_auth_reload_on_mtime_change() {
        let dir = tempdir();
        let store = dir.join("tokens.json");
        let cfg = SystemConfig::default();
        let m = MintArgs {
            token_store: Some(store.clone()),
            name: None,
        };
        mint(m, &cfg).unwrap();
        let parsed = parse_store(&std::fs::read_to_string(&store).unwrap()).unwrap();
        let hash = parsed.tokens[0].hash.clone();

        let auth = TokenAuth::from_store(store.clone()).unwrap();
        // We don't know the raw token (mint only prints it), so build by hash:
        // synthesize a token whose sha256 == hash is impossible, so this test
        // only exercises the negative + reload path.
        assert!(!auth.check(Some("nope")));

        // Revoke and ensure reload picks it up by checking against a known hash.
        let id = parsed.tokens[0].id.clone();
        // bump mtime guaranteed by write
        std::thread::sleep(std::time::Duration::from_millis(10));
        revoke(
            RevokeArgs {
                token_store: Some(store),
                id,
            },
            &cfg,
        )
        .unwrap();
        // Force a reload by calling check; confirm internal state sees revoked.
        auth.check(Some("nope"));
        let guard = auth.inner.lock().unwrap();
        assert_eq!(guard.by_hash.len(), 1);
        assert!(guard.by_hash.get(&hash).unwrap().revoked);
    }

    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir();
        let sub = base.join(format!("claude-proxy-test-{}", rand_hex(8)));
        std::fs::create_dir_all(&sub).unwrap();
        sub
    }
}

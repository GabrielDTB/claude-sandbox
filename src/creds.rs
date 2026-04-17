//! OAuth credentials, shared between `serve` (refresh on access) and
//! `login` (initial write). File schema is byte-identical to the Python
//! version so an existing `~/.claude/.credentials.json` is a drop-in:
//!
//! ```json
//! {
//!   "claudeAiOauth": {
//!     "accessToken": "...",
//!     "refreshToken": "...",
//!     "expiresAt": <unix_ms>,
//!     "scopes": ["..."]
//!   }
//!   // any other top-level keys (Claude Code writes some) are preserved.
//! }
//! ```
//!
//! Refresh discipline: a single `tokio::sync::Mutex<Inner>` guards the
//! in-memory state. `get_access_token` mtime-reloads before taking the
//! lock, then under the lock checks expiry and refreshes if within
//! `REFRESH_MARGIN_S` of expiry. This mirrors Python's `Credentials.lock`
//! pattern — if two callers race on refresh, the second one will see a
//! freshly-refreshed token when it acquires the lock.

use std::{
    fs,
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::constants::{CLIENT_ID, REFRESH_MARGIN_S, TOKEN_URL};

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct OauthBlock {
    #[serde(rename = "accessToken", default, skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,
    #[serde(rename = "refreshToken", default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Unix milliseconds, to match Claude Code's on-disk shape.
    #[serde(rename = "expiresAt", default)]
    pub expires_at: i64,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

#[derive(Debug, Default)]
struct Inner {
    block: OauthBlock,
    mtime_nanos: i128,
}

pub struct Credentials {
    path: PathBuf,
    inner: Mutex<Inner>,
}

impl Credentials {
    pub fn new(path: PathBuf) -> Self {
        let mut inner = Inner::default();
        let _ = load_into(&path, &mut inner); // silent on missing / bad file
        Self { path, inner: Mutex::new(inner) }
    }

    /// True iff we have a refresh token (access token may be stale).
    pub async fn has_credentials(&self) -> bool {
        self.maybe_reload().await;
        let g = self.inner.lock().await;
        g.block.refresh_token.as_deref().map(|s| !s.is_empty()).unwrap_or(false)
    }

    /// Seconds until the current access token expires (may be negative).
    /// Used only for the startup log line.
    pub async fn seconds_until_expiry(&self) -> i64 {
        let g = self.inner.lock().await;
        let now_ms = now_ms();
        (g.block.expires_at - now_ms) / 1000
    }

    /// Fetch (refreshing if needed) the current access token. Returns None if
    /// the proxy is unauthenticated (no refresh token) or refresh fails.
    pub async fn get_access_token(
        self: &Arc<Self>,
        http: &crate::server::UpstreamClient,
    ) -> Option<String> {
        self.maybe_reload().await;
        let mut g = self.inner.lock().await;
        if g.block.refresh_token.as_deref().unwrap_or("").is_empty() {
            return None;
        }
        let now_ms = now_ms();
        // Refresh `REFRESH_MARGIN_S` *before* actual expiry — absorbs clock skew.
        if now_ms > g.block.expires_at - (REFRESH_MARGIN_S as i64) * 1000 {
            match refresh(http, &mut g.block).await {
                Ok(()) => {
                    if let Err(e) = save(&self.path, &g.block) {
                        eprintln!("[auth-proxy] creds save after refresh failed: {e}");
                    } else {
                        g.mtime_nanos = mtime_nanos(&self.path).unwrap_or(0);
                    }
                    eprintln!("[auth-proxy] token refreshed");
                }
                Err(e) => {
                    eprintln!("[auth-proxy] token refresh failed: {e}");
                    return None;
                }
            }
        }
        g.block.access_token.clone()
    }

    async fn maybe_reload(&self) {
        let cur = mtime_nanos(&self.path).unwrap_or(0);
        let stale = {
            let g = self.inner.lock().await;
            cur != g.mtime_nanos
        };
        if stale {
            let mut g = self.inner.lock().await;
            if cur != g.mtime_nanos {
                if let Err(e) = load_into(&self.path, &mut g) {
                    eprintln!("[auth-proxy] creds load failed: {e}");
                }
            }
        }
    }
}

fn load_into(path: &Path, inner: &mut Inner) -> Result<(), crate::Error> {
    let Ok(raw) = fs::read_to_string(path) else {
        // Missing or unreadable: clear in-memory state so we stop serving stale creds.
        inner.block = OauthBlock::default();
        inner.mtime_nanos = 0;
        return Ok(());
    };
    if raw.trim().is_empty() {
        inner.block = OauthBlock::default();
        inner.mtime_nanos = mtime_nanos(path).unwrap_or(0);
        return Ok(());
    }
    // The top-level file may contain arbitrary other keys; we only care
    // about `claudeAiOauth`. Deserialize into a Value so we can preserve
    // unknown keys when we save.
    let root: Value = serde_json::from_str(&raw)?;
    let oauth = root
        .get("claudeAiOauth")
        .cloned()
        .unwrap_or(Value::Object(Default::default()));
    let block: OauthBlock = serde_json::from_value(oauth).unwrap_or_default();
    if block.refresh_token.as_deref().unwrap_or("").is_empty() {
        // File exists but no refreshToken — treat as unauthenticated, but
        // record the mtime so we don't hot-loop retrying.
        inner.block = OauthBlock::default();
    } else {
        inner.block = block;
    }
    inner.mtime_nanos = mtime_nanos(path).unwrap_or(0);
    Ok(())
}

pub fn save(path: &Path, block: &OauthBlock) -> Result<(), crate::Error> {
    // Preserve any other top-level keys that live alongside `claudeAiOauth`.
    let mut root: Value = match fs::read_to_string(path) {
        Ok(raw) if !raw.trim().is_empty() => serde_json::from_str(&raw).unwrap_or_else(|_| Value::Object(Default::default())),
        _ => Value::Object(Default::default()),
    };
    if !root.is_object() {
        root = Value::Object(Default::default());
    }
    root.as_object_mut()
        .unwrap()
        .insert("claudeAiOauth".into(), serde_json::to_value(block)?);
    write_atomic(path, &root)?;
    Ok(())
}

pub fn write_atomic(path: &Path, value: &Value) -> Result<(), crate::Error> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let tmp = path.with_extension(match path.extension() {
        Some(e) => format!("{}.tmp", e.to_string_lossy()),
        None => "tmp".to_string(),
    });
    {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        serde_json::to_writer_pretty(&mut f, value)?;
        f.flush()?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

async fn refresh(
    http: &crate::server::UpstreamClient,
    block: &mut OauthBlock,
) -> Result<(), crate::Error> {
    let refresh_token = block
        .refresh_token
        .as_deref()
        .ok_or("refresh called without a refresh_token")?;
    let scope = block.scopes.join(" ");
    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "client_id": CLIENT_ID,
        "scope": scope,
    });

    #[derive(Deserialize)]
    struct RefreshResp {
        access_token: String,
        #[serde(default)]
        refresh_token: Option<String>,
        expires_in: i64,
    }

    let resp: RefreshResp = http.post_json(TOKEN_URL, &body).await?;
    block.access_token = Some(resp.access_token);
    if let Some(r) = resp.refresh_token {
        block.refresh_token = Some(r);
    }
    // Same shape as Python: expires_at = (now + expires_in) in ms.
    block.expires_at = now_ms() + resp.expires_in * 1000;
    Ok(())
}

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn mtime_nanos(path: &Path) -> Option<i128> {
    let md = fs::metadata(path).ok()?;
    let m = md.modified().ok()?;
    let d = m.duration_since(UNIX_EPOCH).ok()?;
    Some(d.as_nanos() as i128)
}

//! `serve` subcommand + shared HTTPS client used by `login` / creds refresh.
//!
//! Architecture:
//!   * One hyper server listening on the --bind address.
//!   * Per-request `tokio::spawn`: check auth, forward to api.anthropic.com
//!     over an hyper-rustls connector, stream the response body back.
//!   * Upstream responses with `content-type: text/event-stream` are piped
//!     chunk-by-chunk; everything else is buffered.
//!   * Signal handling: SIGTERM/SIGINT trigger a graceful-shutdown handle.

use std::{
    convert::Infallible,
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use http_body_util::{BodyExt, Full};
use hyper::{
    body::{Bytes, Incoming},
    header::{HeaderName, HeaderValue, AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, HOST},
    service::service_fn,
    Method, Request, Response, StatusCode, Uri,
};
use hyper_rustls::HttpsConnector;
use hyper_util::{
    client::legacy::{connect::HttpConnector, Client as LegacyClient},
    rt::{TokioExecutor, TokioIo},
    server::conn::auto::Builder as HttpServerBuilder,
};
use serde::de::DeserializeOwned;
use tokio::net::TcpListener;

use crate::{
    cli::ServeArgs,
    config::SystemConfig,
    constants::{
        API_HOST, ALLOWED_PREFIXES, ORG_TYPE_TO_SUBSCRIPTION, PROFILE_URL, REQUEST_READ_TIMEOUT_S,
        SUBSCRIPTION_PATH, SUBSCRIPTION_TTL_S, UPSTREAM_TIMEOUT_S,
    },
    creds::Credentials,
    token_store::TokenAuth,
};

// ---------------------------------------------------------------------------
// Upstream HTTPS client: shared by creds refresh + login + forwarded requests.
// ---------------------------------------------------------------------------

type BoxBody = http_body_util::combinators::BoxBody<Bytes, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Clone)]
pub struct UpstreamClient {
    inner: LegacyClient<HttpsConnector<HttpConnector>, BoxBody>,
}

impl UpstreamClient {
    pub fn new() -> Result<Self, crate::Error> {
        // Rustls crypto provider is set once per process; if another caller
        // already installed one (shouldn't happen in this binary) we fall back.
        let _ = rustls::crypto::ring::default_provider().install_default();

        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_native_roots()?
            .https_only()
            .enable_http1()
            .build();
        let inner = LegacyClient::builder(TokioExecutor::new()).build(https);
        Ok(Self { inner })
    }

    /// POST a JSON body and decode a JSON response. Used by login + creds refresh.
    pub async fn post_json<T: DeserializeOwned>(
        &self,
        url: &str,
        body: &serde_json::Value,
    ) -> Result<T, crate::Error> {
        let body_bytes = serde_json::to_vec(body)?;
        let req = Request::builder()
            .method(Method::POST)
            .uri(url)
            .header(CONTENT_TYPE, "application/json")
            .body(box_body(Full::new(Bytes::from(body_bytes))))?;
        let resp = tokio::time::timeout(Duration::from_secs(30), self.inner.request(req))
            .await
            .map_err(|_| "upstream request timed out")??;
        let status = resp.status();
        let bytes = resp.into_body().collect().await.map_err(|e| format!("read upstream body: {e}"))?.to_bytes();
        if !status.is_success() {
            let text = String::from_utf8_lossy(&bytes);
            return Err(format!("HTTP {}: {}", status.as_u16(), text).into());
        }
        let parsed: T = serde_json::from_slice(&bytes)?;
        Ok(parsed)
    }

    /// GET a bearer-authenticated JSON resource. Used by the subscription
    /// lookup; same timeout and error shape as [`Self::post_json`].
    pub async fn get_json<T: DeserializeOwned>(
        &self,
        url: &str,
        bearer: &str,
    ) -> Result<T, crate::Error> {
        let req = Request::builder()
            .method(Method::GET)
            .uri(url)
            .header(AUTHORIZATION, format!("Bearer {bearer}"))
            .header(CONTENT_TYPE, "application/json")
            .body(box_body(Full::new(Bytes::new())))?;
        let resp = tokio::time::timeout(Duration::from_secs(30), self.inner.request(req))
            .await
            .map_err(|_| "upstream request timed out")??;
        let status = resp.status();
        let bytes = resp.into_body().collect().await.map_err(|e| format!("read upstream body: {e}"))?.to_bytes();
        if !status.is_success() {
            let text = String::from_utf8_lossy(&bytes);
            return Err(format!("HTTP {}: {}", status.as_u16(), text).into());
        }
        let parsed: T = serde_json::from_slice(&bytes)?;
        Ok(parsed)
    }
}

fn box_body<B>(b: B) -> BoxBody
where
    B: hyper::body::Body<Data = Bytes> + Send + Sync + 'static,
    B::Error: std::error::Error + Send + Sync + 'static,
{
    b.map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>).boxed()
}

// ---------------------------------------------------------------------------
// serve entrypoint
// ---------------------------------------------------------------------------

struct ServerState {
    auth: TokenAuth,
    creds: Arc<Credentials>,
    http: UpstreamClient,
    /// Plan tier + when we fetched it, for `SUBSCRIPTION_PATH`. Cached so a
    /// sandbox launch costs a loopback round-trip rather than an upstream one.
    subscription: tokio::sync::Mutex<Option<(Subscription, Instant)>>,
}

pub async fn run(args: ServeArgs, config: &SystemConfig) -> Result<u8, crate::Error> {
    // --- auth source: ephemeral token vs persistent store (mutually exclusive) ---
    let auth = if let Some(env_var) = args.initial_token_env.as_deref() {
        let tok = std::env::var(env_var).map_err(|_| {
            format!("env var {env_var} is empty or unset (required by --initial-token-env)")
        })?;
        if tok.is_empty() {
            return Err(format!("env var {env_var} is empty or unset").into());
        }
        TokenAuth::ephemeral(&tok)
    } else {
        let path = config
            .token_store_path(args.token_store)
            .ok_or("serve needs --token-store or --initial-token-env")?;
        // Bootstrap an empty store on first boot so the systemd service can
        // start before anyone has minted a token.
        if !path.exists() {
            let parent = path.parent();
            if let Some(p) = parent {
                if !p.as_os_str().is_empty() {
                    std::fs::create_dir_all(p)?;
                }
            }
            std::fs::write(&path, b"{\"tokens\": []}\n")?;
            // chmod after write in case umask is looser than 077.
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            eprintln!(
                "[auth-proxy] initialised empty token store at {} — run `claude-proxy mint` \
                 before any client can authenticate",
                path.display()
            );
        }
        TokenAuth::from_store(path)?
    };

    // --- creds ---
    let creds_path: PathBuf = config
        .creds_path(args.creds, true)
        .ok_or("serve needs a creds path (--creds, $CLAUDE_PROXY_CREDS, or config file)")?;
    let creds = Arc::new(Credentials::new(creds_path.clone()));
    if creds.has_credentials().await {
        let secs = creds.seconds_until_expiry().await;
        eprintln!(
            "[auth-proxy] loaded credentials from {}, access token expires in {}s",
            creds_path.display(),
            secs
        );
    } else {
        warn_unauth(Some(&creds_path));
    }

    // --- shared state ---
    let state = Arc::new(ServerState {
        auth,
        creds,
        http: UpstreamClient::new()?,
        subscription: tokio::sync::Mutex::new(None),
    });

    // --- bind + serve ---
    let addr: SocketAddr = parse_bind(&args.bind)?;
    let listener = TcpListener::bind(addr).await?;
    eprintln!("[auth-proxy] listening on {addr}");

    let mut shutdown = shutdown_signal();
    loop {
        tokio::select! {
            _ = &mut shutdown => {
                eprintln!("[auth-proxy] shutdown signal received");
                return Ok(0);
            }
            accept = listener.accept() => {
                let (stream, peer) = match accept {
                    Ok(x) => x,
                    Err(e) => {
                        eprintln!("[auth-proxy] accept error: {e}");
                        continue;
                    }
                };
                let state = state.clone();
                tokio::spawn(async move {
                    let io = TokioIo::new(stream);
                    let svc = service_fn(move |req| {
                        let state = state.clone();
                        async move { Ok::<_, Infallible>(handle(state, req).await) }
                    });
                    let _ = HttpServerBuilder::new(TokioExecutor::new())
                        .serve_connection(io, svc)
                        .await
                        .map_err(|e| eprintln!("[auth-proxy {peer}] conn error: {e}"));
                });
            }
        }
    }
}

/// Parse host:port, accepting `[ipv6]:port` and bare `:port` (→ 0.0.0.0:port).
fn parse_bind(s: &str) -> Result<SocketAddr, crate::Error> {
    // Let std do the heavy lifting; only rewrite the bare ":port" shortcut.
    let rewritten: String;
    let s = if let Some(port) = s.strip_prefix(':') {
        rewritten = format!("0.0.0.0:{port}");
        &rewritten
    } else {
        s
    };
    s.parse::<SocketAddr>()
        .map_err(|e| format!("invalid --bind {s:?}: {e}").into())
}

fn shutdown_signal() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
    Box::pin(async {
        let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(s) => s,
            Err(_) => return,
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    })
}

fn warn_unauth(creds_path: Option<&std::path::Path>) {
    if let Some(p) = creds_path {
        eprintln!(
            "[auth-proxy] warning: proxy is not authenticated — run \
             `claude-proxy login --creds {}` to authenticate",
            p.display()
        );
    } else {
        eprintln!(
            "[auth-proxy] warning: proxy is not authenticated — run \
             `claude-proxy login --creds <path>` to authenticate"
        );
    }
}

// ---------------------------------------------------------------------------
// Per-request handler
// ---------------------------------------------------------------------------

/// The beta gate Anthropic requires on subscription (Claude Code) OAuth tokens.
const OAUTH_BETA_HEADER: &str = "oauth-2025-04-20";
/// The identity string Anthropic requires as the *sole* system prompt when a
/// subscription OAuth token is used. Verified byte-for-byte against the live API.
const CLAUDE_CODE_IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

/// Rewrite an OpenAI-compatible `/v1/chat/completions` body into the only shape
/// Anthropic accepts from a subscription OAuth token.
///
/// Anthropic gates these tokens: inference is accepted only when the request
/// carries `anthropic-beta: oauth-2025-04-20` (added by the caller) AND its
/// system prompt is *exactly* the Claude Code identity. Native `/v1/messages`
/// callers (Claude Code, the ACP agent panel) already do this, so they are never
/// routed here. Plain OpenAI clients — marimo's chat and "generate with AI" —
/// send neither and get bounced with a canned `rate_limit_error`.
///
/// The compat endpoint concatenates multiple `system` messages, so a second
/// system block makes the prompt no longer *exactly* the identity and is
/// rejected (observed live). We therefore hoist every non-identity system
/// message into the first user turn — preserving the client's instructions —
/// and leave a single identity `system` message at the front.
///
/// Bodies that aren't JSON objects with a `messages` array are returned
/// untouched, so anything unexpected is forwarded verbatim rather than dropped.
fn rewrite_compat_oauth_body(body: Bytes) -> Bytes {
    let Ok(mut root) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return body;
    };
    let Some(obj) = root.as_object_mut() else {
        return body;
    };
    let Some(serde_json::Value::Array(messages)) = obj.remove("messages") else {
        return body;
    };

    let mut extra_system = String::new();
    let mut kept: Vec<serde_json::Value> = Vec::with_capacity(messages.len() + 1);
    for msg in messages {
        if msg.get("role").and_then(|r| r.as_str()) == Some("system") {
            if let Some(text) = msg.get("content").and_then(|c| c.as_str()) {
                if text == CLAUDE_CODE_IDENTITY {
                    continue; // re-added as the single identity block below
                }
                if !extra_system.is_empty() {
                    extra_system.push_str("\n\n");
                }
                extra_system.push_str(text);
                continue;
            }
            // Non-string system content is unusual for this endpoint; keep it
            // rather than risk silently dropping the caller's instructions.
        }
        kept.push(msg);
    }

    if !extra_system.is_empty() {
        match kept.iter_mut().find(|m| {
            m.get("role").and_then(|r| r.as_str()) == Some("user")
                && m.get("content").is_some_and(|c| c.is_string())
        }) {
            Some(user) => {
                let merged = format!(
                    "{extra_system}\n\n{}",
                    user["content"].as_str().unwrap_or_default()
                );
                user["content"] = serde_json::Value::String(merged);
            }
            None => kept.insert(
                0,
                serde_json::json!({ "role": "user", "content": extra_system }),
            ),
        }
    }

    kept.insert(
        0,
        serde_json::json!({ "role": "system", "content": CLAUDE_CODE_IDENTITY }),
    );

    obj.insert("messages".to_string(), serde_json::Value::Array(kept));
    serde_json::to_vec(&root).map(Bytes::from).unwrap_or(body)
}

// ---------------------------------------------------------------------------
// Subscription lookup (local route, never forwarded)
// ---------------------------------------------------------------------------

/// The entire response body of `SUBSCRIPTION_PATH`. Both fields are optional
/// because Claude Code itself treats an unrecognised organization type, or a
/// profile without a rate-limit tier, as `null`.
#[derive(Clone, serde::Serialize)]
struct Subscription {
    #[serde(rename = "subscriptionType")]
    subscription_type: Option<String>,
    #[serde(rename = "rateLimitTier")]
    rate_limit_tier: Option<String>,
}

/// The two fields we read out of the upstream profile.
///
/// The upstream body also carries `account.email`, `account.uuid` and
/// `organization.uuid`. Those are deliberately absent from this struct: what
/// we never deserialize, we can never leak into the sandbox by accident.
#[derive(serde::Deserialize)]
struct ProfileResp {
    #[serde(default)]
    organization: Option<ProfileOrg>,
}

#[derive(Default, serde::Deserialize)]
struct ProfileOrg {
    #[serde(default)]
    organization_type: Option<String>,
    #[serde(default)]
    rate_limit_tier: Option<String>,
}

/// Map an `organization_type` to Claude Code's `subscriptionType`. Unknown
/// types yield `None`, matching Claude Code's own `Map.get` semantics.
fn subscription_type_for(org_type: &str) -> Option<String> {
    ORG_TYPE_TO_SUBSCRIPTION
        .iter()
        .find(|(k, _)| *k == org_type)
        .map(|(_, v)| (*v).to_string())
}

async fn handle_subscription(state: &Arc<ServerState>) -> Response<BoxBody> {
    if let Some((sub, fetched)) = state.subscription.lock().await.as_ref() {
        if fetched.elapsed() < Duration::from_secs(SUBSCRIPTION_TTL_S) {
            return json_ok(sub);
        }
    }

    let Some(access_token) = state.creds.get_access_token(&state.http).await else {
        return unauth_envelope();
    };

    let profile: ProfileResp = match state.http.get_json(PROFILE_URL, &access_token).await {
        Ok(p) => p,
        Err(e) => {
            // Not fatal for the caller: the launcher falls back to its own
            // defaults rather than failing the sandbox launch.
            eprintln!("[auth-proxy] subscription lookup failed: {e}");
            return plain_error(StatusCode::BAD_GATEWAY, "profile lookup failed".into());
        }
    };

    let org = profile.organization.unwrap_or_default();
    let sub = Subscription {
        subscription_type: org.organization_type.as_deref().and_then(subscription_type_for),
        rate_limit_tier: org.rate_limit_tier,
    };
    *state.subscription.lock().await = Some((sub.clone(), Instant::now()));
    json_ok(&sub)
}

// ---------------------------------------------------------------------------
// Per-request dispatch
// ---------------------------------------------------------------------------

async fn handle(state: Arc<ServerState>, req: Request<Incoming>) -> Response<BoxBody> {
    let path = req.uri().path().to_string();

    // Auth first, so an unauthenticated caller can't probe which paths we
    // forward by reading 403-vs-401 off the path allowlist below.
    let bearer = extract_bearer(&req);
    if !state.auth.check(bearer.as_deref()) {
        return plain_error(StatusCode::UNAUTHORIZED, "Unauthorized".into());
    }

    // Served locally — not forwarded, and not subject to ALLOWED_PREFIXES.
    if path == SUBSCRIPTION_PATH {
        return handle_subscription(&state).await;
    }

    if !ALLOWED_PREFIXES.iter().any(|p| path.starts_with(p)) {
        return plain_error(StatusCode::FORBIDDEN, format!("Path not allowed: {path}"));
    }

    // --- buffer request body (with read timeout) ---
    let (parts, body) = req.into_parts();
    let body_bytes = match tokio::time::timeout(
        Duration::from_secs(REQUEST_READ_TIMEOUT_S),
        body.collect(),
    )
    .await
    {
        Ok(Ok(b)) => b.to_bytes(),
        Ok(Err(e)) => {
            eprintln!("[auth-proxy] body read error: {e}");
            return plain_error(StatusCode::BAD_REQUEST, "body read error".into());
        }
        Err(_) => {
            return plain_error(StatusCode::REQUEST_TIMEOUT, "Request body read timed out".into());
        }
    };

    // --- make OpenAI-compatible chat requests valid under subscription OAuth ---
    // Scoped to the compat endpoint; native /v1/messages callers are forwarded
    // byte-for-byte. See rewrite_compat_oauth_body for the why.
    let is_compat_chat = path.ends_with("/chat/completions");
    let body_bytes = if is_compat_chat {
        rewrite_compat_oauth_body(body_bytes)
    } else {
        body_bytes
    };

    // --- fetch access token; 503 if proxy isn't authenticated ---
    let access_token = match state.creds.get_access_token(&state.http).await {
        Some(t) => t,
        None => return unauth_envelope(),
    };

    // --- build upstream request ---
    let upstream_uri = match build_upstream_uri(&parts.uri) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("[auth-proxy] bad upstream uri: {e}");
            return plain_error(StatusCode::BAD_REQUEST, "bad request URI".into());
        }
    };
    let mut up_req = Request::builder().method(parts.method.clone()).uri(upstream_uri);
    let drop_headers: [HeaderName; 9] = [
        HOST,
        AUTHORIZATION,
        HeaderName::from_static("x-api-key"),
        HeaderName::from_static("connection"),
        HeaderName::from_static("transfer-encoding"),
        HeaderName::from_static("proxy-authorization"),
        HeaderName::from_static("proxy-connection"),
        HeaderName::from_static("keep-alive"),
        HeaderName::from_static("te"),
    ];
    for (name, value) in parts.headers.iter() {
        if drop_headers.iter().any(|d| d == name) {
            continue;
        }
        if name == "trailer" || name == "upgrade" {
            continue;
        }
        // Replaced below for the compat OAuth rewrite (new body length + beta).
        if is_compat_chat && (name == CONTENT_LENGTH || name.as_str() == "anthropic-beta") {
            continue;
        }
        up_req = up_req.header(name, value);
    }
    up_req = up_req
        .header(HOST, API_HOST)
        .header(
            AUTHORIZATION,
            match HeaderValue::from_str(&format!("Bearer {access_token}")) {
                Ok(v) => v,
                Err(_) => return plain_error(StatusCode::INTERNAL_SERVER_ERROR, "bad token".into()),
            },
        );

    if is_compat_chat {
        up_req = up_req
            .header(
                HeaderName::from_static("anthropic-beta"),
                HeaderValue::from_static(OAUTH_BETA_HEADER),
            )
            .header(CONTENT_LENGTH, body_bytes.len().to_string());
    }

    let up_body = box_body(Full::new(body_bytes));
    let up_req = match up_req.body(up_body) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[auth-proxy] upstream build error: {e}");
            return plain_error(StatusCode::BAD_GATEWAY, "Upstream error".into());
        }
    };

    // --- send upstream ---
    let resp = match tokio::time::timeout(
        Duration::from_secs(UPSTREAM_TIMEOUT_S),
        state.http.inner.request(up_req),
    )
    .await
    {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            eprintln!("[auth-proxy] upstream error: {e}");
            return plain_error(StatusCode::BAD_GATEWAY, format!("Upstream error: {e}"));
        }
        Err(_) => {
            return plain_error(StatusCode::GATEWAY_TIMEOUT, "upstream timeout".into());
        }
    };

    // --- translate response headers: drop hop-by-hop & content-length / transfer-encoding ---
    let (resp_parts, resp_body) = resp.into_parts();
    let mut out = Response::builder().status(resp_parts.status);
    for (name, value) in resp_parts.headers.iter() {
        let n = name.as_str();
        if matches!(
            n,
            "transfer-encoding" | "connection" | "keep-alive" | "content-length"
        ) {
            continue;
        }
        out = out.header(name, value);
    }
    // Streaming bodies are forwarded as-is via BoxBody; hyper handles chunking.
    let body: BoxBody = resp_body
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
        .boxed();
    match out.body(body) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[auth-proxy] response build error: {e}");
            plain_error(StatusCode::BAD_GATEWAY, "response build error".into())
        }
    }
}

fn build_upstream_uri(incoming: &Uri) -> Result<Uri, crate::Error> {
    // Preserve path + query; force https://api.anthropic.com as authority.
    let pq = incoming
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    let full = format!("https://{API_HOST}{pq}");
    Ok(full.parse()?)
}

fn extract_bearer<B>(req: &Request<B>) -> Option<String> {
    let v = req.headers().get(AUTHORIZATION)?;
    let s = v.to_str().ok()?;
    if s.len() < 7 {
        return None;
    }
    if !s.get(..7)?.eq_ignore_ascii_case("bearer ") {
        return None;
    }
    Some(s[7..].trim().to_string())
}

fn json_ok<T: serde::Serialize>(value: &T) -> Response<BoxBody> {
    let bytes = match serde_json::to_vec(value) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[auth-proxy] json encode error: {e}");
            return plain_error(StatusCode::INTERNAL_SERVER_ERROR, "encode error".into());
        }
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "application/json")
        .body(box_body(Full::new(Bytes::from(bytes))))
        .unwrap_or_else(|_| Response::new(box_body(Full::new(Bytes::new()))))
}

fn plain_error(status: StatusCode, msg: String) -> Response<BoxBody> {
    let body = Bytes::from(msg);
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(box_body(Full::new(body)))
        .unwrap_or_else(|_| Response::new(box_body(Full::new(Bytes::new()))))
}

/// The exact Anthropic `authentication_error` envelope — preserved byte-for-byte
/// from the Python version so Claude Code surfaces the message verbatim.
fn unauth_envelope() -> Response<BoxBody> {
    let body = serde_json::json!({
        "type": "error",
        "error": {
            "type": "authentication_error",
            "message": "claude-proxy is not authenticated. \
                        Run `claude-proxy login --creds <path>` \
                        on the proxy host to authenticate."
        }
    });
    let bytes = serde_json::to_vec(&body).unwrap_or_default();
    Response::builder()
        .status(StatusCode::SERVICE_UNAVAILABLE)
        .header(CONTENT_TYPE, "application/json")
        .body(box_body(Full::new(Bytes::from(bytes))))
        .unwrap_or_else(|_| Response::new(box_body(Full::new(Bytes::new()))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::Request;

    #[test]
    fn parse_bind_ok() {
        assert_eq!(parse_bind("127.0.0.1:18080").unwrap().port(), 18080);
        assert_eq!(parse_bind("0.0.0.0:18080").unwrap().port(), 18080);
        assert_eq!(parse_bind(":18080").unwrap().port(), 18080);
        assert_eq!(parse_bind("[::1]:18080").unwrap().port(), 18080);
    }

    #[test]
    fn parse_bind_rejects_garbage() {
        assert!(parse_bind("nope").is_err());
    }

    #[test]
    fn bearer_extraction() {
        let req = Request::builder()
            .uri("/v1/models")
            .header(AUTHORIZATION, "Bearer abc123")
            .body(())
            .unwrap();
        assert_eq!(extract_bearer(&req).as_deref(), Some("abc123"));

        let req = Request::builder()
            .uri("/v1/models")
            .header(AUTHORIZATION, "bearer   xyz")
            .body(())
            .unwrap();
        assert_eq!(extract_bearer(&req).as_deref(), Some("xyz"));

        let req = Request::builder().uri("/v1/models").body(()).unwrap();
        assert_eq!(extract_bearer(&req), None);

        let req = Request::builder()
            .uri("/v1/models")
            .header(AUTHORIZATION, "Basic foo")
            .body(())
            .unwrap();
        assert_eq!(extract_bearer(&req), None);
    }

    #[test]
    fn unauth_envelope_shape() {
        let resp = unauth_envelope();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            resp.headers().get(CONTENT_TYPE).unwrap(),
            "application/json"
        );
    }

    fn rewrite(body: serde_json::Value) -> serde_json::Value {
        let out = rewrite_compat_oauth_body(Bytes::from(serde_json::to_vec(&body).unwrap()));
        serde_json::from_slice(&out).unwrap()
    }

    #[test]
    fn compat_prepends_identity_when_no_system() {
        let out = rewrite(serde_json::json!({
            "model": "claude-sonnet-4-6",
            "messages": [{ "role": "user", "content": "hi" }],
        }));
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], CLAUDE_CODE_IDENTITY);
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[1]["content"], "hi");
        // Unrelated fields survive.
        assert_eq!(out["model"], "claude-sonnet-4-6");
    }

    #[test]
    fn compat_hoists_client_system_into_first_user_turn() {
        let out = rewrite(serde_json::json!({
            "messages": [
                { "role": "system", "content": "You help with marimo." },
                { "role": "user", "content": "write a plot" },
            ],
        }));
        let msgs = out["messages"].as_array().unwrap();
        // Exactly one system message, and it is exactly the identity.
        let systems: Vec<_> = msgs.iter().filter(|m| m["role"] == "system").collect();
        assert_eq!(systems.len(), 1);
        assert_eq!(systems[0]["content"], CLAUDE_CODE_IDENTITY);
        assert_eq!(msgs[0]["role"], "system");
        // The client's instructions were folded into the user turn, not dropped.
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[1]["content"], "You help with marimo.\n\nwrite a plot");
    }

    #[test]
    fn compat_is_idempotent_when_identity_already_sole_system() {
        let once = rewrite(serde_json::json!({
            "messages": [
                { "role": "system", "content": CLAUDE_CODE_IDENTITY },
                { "role": "user", "content": "hi" },
            ],
        }));
        let msgs = once["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["content"], CLAUDE_CODE_IDENTITY);
        assert_eq!(msgs[1]["content"], "hi");
        // Running it again changes nothing.
        assert_eq!(rewrite(once.clone()), once);
    }

    #[test]
    fn compat_non_json_body_passes_through() {
        let raw = Bytes::from_static(b"not json at all");
        assert_eq!(rewrite_compat_oauth_body(raw.clone()), raw);
    }

    #[test]
    fn compat_body_without_messages_passes_through() {
        let raw = Bytes::from(serde_json::to_vec(&serde_json::json!({ "foo": 1 })).unwrap());
        assert_eq!(rewrite_compat_oauth_body(raw.clone()), raw);
    }

    #[test]
    fn org_types_map_to_claude_codes_subscription_names() {
        assert_eq!(subscription_type_for("claude_max").as_deref(), Some("max"));
        assert_eq!(subscription_type_for("claude_pro").as_deref(), Some("pro"));
        assert_eq!(
            subscription_type_for("claude_enterprise").as_deref(),
            Some("enterprise")
        );
        assert_eq!(subscription_type_for("claude_team").as_deref(), Some("team"));
    }

    /// Claude Code reads this table as a `Map.get`, so an organization type it
    /// doesn't know becomes `null` rather than an error. Mirror that instead of
    /// guessing a tier the account may not have.
    #[test]
    fn unknown_org_type_maps_to_none() {
        assert_eq!(subscription_type_for("claude_something_new"), None);
        assert_eq!(subscription_type_for(""), None);
    }

    /// The wire names are a contract with the launcher, which feeds them
    /// straight into `CLAUDE_CODE_SUBSCRIPTION_TYPE` / `..._RATE_LIMIT_TIER`.
    /// Equally load-bearing: no account or organization identity in the body.
    #[test]
    fn subscription_body_carries_tier_only() {
        let sub = Subscription {
            subscription_type: Some("max".into()),
            rate_limit_tier: Some("default_claude_max_20x".into()),
        };
        let v: serde_json::Value = serde_json::from_slice(&serde_json::to_vec(&sub).unwrap()).unwrap();
        assert_eq!(v["subscriptionType"], "max");
        assert_eq!(v["rateLimitTier"], "default_claude_max_20x");
        assert_eq!(
            v.as_object().unwrap().len(),
            2,
            "subscription response grew a field — identity must not leak: {v}"
        );
    }

    /// We parse the upstream profile through a struct that has no `account`
    /// field at all, so email / UUIDs can't survive the round-trip even if the
    /// handler were later changed to echo what it deserialized.
    #[test]
    fn profile_parse_drops_account_identity() {
        let raw = serde_json::json!({
            "account": { "uuid": "acc-uuid", "email": "person@example.com" },
            "organization": {
                "uuid": "org-uuid",
                "organization_type": "claude_max",
                "rate_limit_tier": "default_claude_max_20x",
            },
        });
        let parsed: ProfileResp = serde_json::from_slice(&serde_json::to_vec(&raw).unwrap()).unwrap();
        let org = parsed.organization.unwrap();
        assert_eq!(org.organization_type.as_deref(), Some("claude_max"));
        assert_eq!(org.rate_limit_tier.as_deref(), Some("default_claude_max_20x"));
    }

    /// A profile without an `organization` block must degrade to nulls rather
    /// than panicking the handler.
    #[test]
    fn profile_without_organization_is_all_none() {
        let parsed: ProfileResp = serde_json::from_str("{}").unwrap();
        let org = parsed.organization.unwrap_or_default();
        assert_eq!(org.organization_type, None);
        assert_eq!(org.rate_limit_tier, None);
    }
}

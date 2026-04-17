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
    time::Duration,
};

use http_body_util::{BodyExt, Full};
use hyper::{
    body::{Bytes, Incoming},
    header::{HeaderName, HeaderValue, AUTHORIZATION, CONTENT_TYPE, HOST},
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
        API_HOST, ALLOWED_PREFIXES, REQUEST_READ_TIMEOUT_S, UPSTREAM_TIMEOUT_S,
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

async fn handle(state: Arc<ServerState>, req: Request<Incoming>) -> Response<BoxBody> {
    let path = req.uri().path().to_string();

    if !ALLOWED_PREFIXES.iter().any(|p| path.starts_with(p)) {
        return plain_error(StatusCode::FORBIDDEN, format!("Path not allowed: {path}"));
    }

    let bearer = extract_bearer(&req);
    if !state.auth.check(bearer.as_deref()) {
        return plain_error(StatusCode::UNAUTHORIZED, "Unauthorized".into());
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
}

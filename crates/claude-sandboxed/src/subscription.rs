//! Ask the auth proxy which plan the account is on, so `run.rs` can export the
//! real `CLAUDE_CODE_SUBSCRIPTION_TYPE` / `CLAUDE_CODE_RATE_LIMIT_TIER` into
//! the sandbox instead of a hardcoded guess.
//!
//! Why the launcher has to do this at all: Claude Code resolves both values
//! from `{BASE_API_URL}/api/oauth/profile`, and `BASE_API_URL` is hardcoded to
//! `api.anthropic.com` — it does *not* follow `ANTHROPIC_BASE_URL`. A sandboxed
//! Claude therefore reaches the real API with the proxy bearer and is rejected,
//! so it can never learn its own tier. The proxy holds the real credentials, so
//! it answers on our behalf (see `claude-proxy`'s `SUBSCRIPTION_PATH`).
//!
//! Why this hand-rolls HTTP rather than pulling in a client crate: the proxy
//! always serves plain HTTP (`claude-proxy`'s `serve` binds a bare
//! `TcpListener`, no TLS), the response is a two-field JSON object, and asking
//! for `Connection: close` lets us read to EOF and skip chunked-transfer
//! decoding entirely. That is a materially smaller thing to get right than a
//! new dependency tree in a launcher that otherwise makes no network calls.
//!
//! Every failure path returns `None`. Tier information is a display nicety —
//! it must never be able to fail a sandbox launch, so callers fall back to
//! `constants::FALLBACK_*`.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use serde::Deserialize;

/// Path served by `claude-proxy`. Must match its `SUBSCRIPTION_PATH`.
const SUBSCRIPTION_PATH: &str = "/_sandbox/subscription";

/// Budget for the whole exchange (connect, then read). Short on purpose: this
/// sits on the launch path, and the fallback is perfectly usable.
const TIMEOUT: Duration = Duration::from_secs(2);

/// The account's plan tier, exactly as Claude Code names it. Either field can
/// be absent — an organization type the proxy doesn't recognise maps to `null`.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
pub struct Subscription {
    #[serde(rename = "subscriptionType")]
    pub subscription_type: Option<String>,
    #[serde(rename = "rateLimitTier")]
    pub rate_limit_tier: Option<String>,
}

/// Ask the proxy at `host_url` (a host-reachable `http://host:port`) for the
/// account's tier, authenticating with the sandbox-to-proxy `token`.
///
/// `None` means "couldn't find out" — an old proxy without the route, an
/// unauthenticated proxy, a timeout, or an `https` URL (a TLS-terminating
/// front end, which this deliberately doesn't handle).
pub fn fetch(host_url: &str, token: &str) -> Option<Subscription> {
    let (host, port) = split_http_url(host_url)?;
    let body = get(&host, port, SUBSCRIPTION_PATH, token)?;
    match serde_json::from_slice::<Subscription>(&body) {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("claude-sandboxed: could not parse subscription response: {e}");
            None
        }
    }
}

/// Split `http://host:port` into its parts. Returns `None` for any other
/// scheme, and for a URL with no explicit port — every proxy URL the launcher
/// builds carries one (`proxy_external::resolve` defaults it from the scheme,
/// `proxy_embedded` uses the published port).
fn split_http_url(raw: &str) -> Option<(String, u16)> {
    let parsed = url::Url::parse(raw).ok()?;
    if parsed.scheme() != "http" {
        return None;
    }
    let host = parsed.host_str()?.to_string();
    let port = parsed.port_or_known_default()?;
    Some((host, port))
}

/// One-shot HTTP/1.1 GET. Returns the response body on `200`, `None` otherwise.
fn get(host: &str, port: u16, path: &str, token: &str) -> Option<Vec<u8>> {
    let addr = match (host, port).to_socket_addrs().ok().and_then(|mut a| a.next()) {
        Some(a) => a,
        None => {
            eprintln!("claude-sandboxed: cannot resolve auth proxy at {host}:{port}");
            return None;
        }
    };

    let mut stream = TcpStream::connect_timeout(&addr, TIMEOUT).ok()?;
    stream.set_read_timeout(Some(TIMEOUT)).ok()?;
    stream.set_write_timeout(Some(TIMEOUT)).ok()?;

    // `Connection: close` is what lets the caller read to EOF below instead of
    // implementing chunked-transfer decoding.
    let req = format!(
        "GET {path} HTTP/1.1\r\n\
         Host: {host}:{port}\r\n\
         Authorization: Bearer {token}\r\n\
         Accept: application/json\r\n\
         Connection: close\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).ok()?;
    stream.flush().ok()?;

    let mut raw = Vec::new();
    // A read timeout surfaces as an error *after* some bytes may already have
    // arrived, so keep whatever we got and let the parser judge it.
    let _ = stream.read_to_end(&mut raw);

    match parse_response(&raw) {
        Some((200, body)) => Some(body.to_vec()),
        Some((status, _)) => {
            // 403 is the expected answer from a proxy predating this route.
            eprintln!(
                "claude-sandboxed: auth proxy returned HTTP {status} for {path} \
                 — falling back to default subscription tier"
            );
            None
        }
        None => {
            eprintln!("claude-sandboxed: unreadable response from auth proxy at {host}:{port}");
            None
        }
    }
}

/// Split a raw HTTP/1.1 response into `(status_code, body)`.
fn parse_response(raw: &[u8]) -> Option<(u16, &[u8])> {
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&raw[..split]).ok()?;
    let body = &raw[split + 4..];

    // Status line: `HTTP/1.1 200 OK`.
    let mut parts = head.lines().next()?.split_whitespace();
    let version = parts.next()?;
    if !version.starts_with("HTTP/") {
        return None;
    }
    let status = parts.next()?.parse::<u16>().ok()?;
    Some((status, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_200_with_a_json_body() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"subscriptionType\":\"max\"}";
        let (status, body) = parse_response(raw).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"{\"subscriptionType\":\"max\"}");
    }

    /// The proxy answers 502 when the upstream profile lookup fails, and 403
    /// when it predates this route. Both must be reported as "unknown", never
    /// mistaken for a body.
    #[test]
    fn non_200_statuses_are_recognised() {
        let (status, _) = parse_response(b"HTTP/1.1 502 Bad Gateway\r\n\r\nprofile lookup failed").unwrap();
        assert_eq!(status, 502);
        let (status, _) = parse_response(b"HTTP/1.1 403 Forbidden\r\n\r\nPath not allowed").unwrap();
        assert_eq!(status, 403);
    }

    #[test]
    fn empty_body_still_parses() {
        let (status, body) = parse_response(b"HTTP/1.1 204 No Content\r\n\r\n").unwrap();
        assert_eq!(status, 204);
        assert!(body.is_empty());
    }

    #[test]
    fn truncated_and_garbage_responses_are_rejected() {
        // Headers never terminated — e.g. the read timed out mid-response.
        assert!(parse_response(b"HTTP/1.1 200 OK\r\nContent-Type: app").is_none());
        assert!(parse_response(b"").is_none());
        assert!(parse_response(b"not http at all\r\n\r\nbody").is_none());
    }

    /// Both fields are optional: an organization type the proxy doesn't know
    /// yields `null`, and the launcher must fall back for that field alone.
    #[test]
    fn subscription_fields_are_individually_optional() {
        let s: Subscription = serde_json::from_str(r#"{"subscriptionType":null,"rateLimitTier":"standard"}"#).unwrap();
        assert_eq!(s.subscription_type, None);
        assert_eq!(s.rate_limit_tier.as_deref(), Some("standard"));

        let s: Subscription = serde_json::from_str("{}").unwrap();
        assert_eq!(s, Subscription::default());
    }

    #[test]
    fn wire_names_match_the_proxy() {
        let s: Subscription =
            serde_json::from_str(r#"{"subscriptionType":"max","rateLimitTier":"default_claude_max_20x"}"#)
                .unwrap();
        assert_eq!(s.subscription_type.as_deref(), Some("max"));
        assert_eq!(s.rate_limit_tier.as_deref(), Some("default_claude_max_20x"));
    }

    #[test]
    fn only_http_urls_are_accepted() {
        assert_eq!(
            split_http_url("http://127.0.0.1:18080"),
            Some(("127.0.0.1".into(), 18080))
        );
        // TLS-terminating front end — out of scope, caller falls back.
        assert_eq!(split_http_url("https://proxy.example:443"), None);
        assert_eq!(split_http_url("not a url"), None);
    }

    /// `proxy_external` accepts a URL without an explicit port and defaults it
    /// from the scheme; make sure we agree rather than bailing out.
    #[test]
    fn port_defaults_to_80_for_http() {
        assert_eq!(
            split_http_url("http://proxy.example"),
            Some(("proxy.example".into(), 80))
        );
    }

    /// Serve one canned response on an ephemeral port and hand back its URL.
    /// Exercises the socket path end-to-end — `parse_response` alone doesn't
    /// cover request framing, the `Connection: close` handshake, or read-to-EOF.
    fn serve_once(response: &'static [u8]) -> (String, std::thread::JoinHandle<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let handle = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            // Read just the request head; the client sends no body.
            let mut req = Vec::new();
            let mut buf = [0u8; 512];
            while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                match sock.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => req.extend_from_slice(&buf[..n]),
                    Err(_) => break,
                }
            }
            sock.write_all(response).unwrap();
            // Closing is what terminates the client's read-to-EOF.
            drop(sock);
            String::from_utf8_lossy(&req).into_owned()
        });
        (url, handle)
    }

    #[test]
    fn fetch_round_trips_against_a_real_socket() {
        let (url, server) = serve_once(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n\
              {\"subscriptionType\":\"max\",\"rateLimitTier\":\"default_claude_max_20x\"}",
        );
        let got = fetch(&url, "sekrit");
        assert_eq!(
            got,
            Some(Subscription {
                subscription_type: Some("max".into()),
                rate_limit_tier: Some("default_claude_max_20x".into()),
            })
        );

        // The proxy authenticates this with the same bearer check it uses for
        // forwarded traffic, so the header must actually be on the wire.
        let req = server.join().unwrap();
        assert!(req.starts_with("GET /_sandbox/subscription HTTP/1.1\r\n"), "{req}");
        assert!(req.contains("Authorization: Bearer sekrit\r\n"), "{req}");
        assert!(req.contains("Connection: close\r\n"), "{req}");
    }

    /// A proxy predating the route answers 403 from its path allowlist. That
    /// must degrade to the fallback, not propagate an error.
    #[test]
    fn fetch_returns_none_on_403() {
        let (url, server) = serve_once(
            b"HTTP/1.1 403 Forbidden\r\nContent-Type: text/plain\r\n\r\n\
              Path not allowed: /_sandbox/subscription",
        );
        assert_eq!(fetch(&url, "sekrit"), None);
        server.join().unwrap();
    }

    #[test]
    fn fetch_returns_none_on_unparseable_body() {
        let (url, server) = serve_once(b"HTTP/1.1 200 OK\r\n\r\nnot json");
        assert_eq!(fetch(&url, "sekrit"), None);
        server.join().unwrap();
    }

    /// Nothing is listening — the common "proxy died" case. Must not hang or
    /// panic; the connect timeout bounds it.
    #[test]
    fn fetch_returns_none_when_nothing_is_listening() {
        // Bind then drop, so the port is almost certainly free.
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        assert_eq!(fetch(&format!("http://127.0.0.1:{port}"), "sekrit"), None);
    }
}

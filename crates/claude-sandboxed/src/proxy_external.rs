//! External auth-proxy mode (`--auth-proxy URL --auth-token-file PATH`).
//!
//! We parse the URL with `url::Url`, resolve the hostname to an IPv4 on
//! the host side, and emit an nft carveout rule so a Tailscale/RFC1918
//! proxy IP isn't rejected by the LAN-block rules.

use std::net::ToSocketAddrs;
use std::path::Path;

use crate::paths;

/// Resolved external-proxy state ready to hand off to `run.rs` + `firewall.rs`.
pub struct External {
    /// Value for ANTHROPIC_BASE_URL inside the sandbox (scheme://ip:port).
    pub proxy_url: String,
    /// Podman --network value.
    pub network: String,
    /// Optional nft rule to insert before the LAN rejects.
    pub carveout: Option<String>,
    /// The sandbox-to-proxy bearer token, loaded from --auth-token-file.
    pub token: String,
}

pub fn prepare(url: &str, token_file: &Path) -> Result<External, crate::Error> {
    let token = load_token(token_file)?;
    let (scheme, ip, port) = resolve(url)?;
    let carveout = format!(
        "nft add rule inet sandbox output ip daddr {ip} tcp dport {port} accept"
    );
    Ok(External {
        proxy_url: format!("{scheme}://{ip}:{port}"),
        network: "pasta:--no-map-gw,--map-guest-addr,none".into(),
        carveout: Some(carveout),
        token,
    })
}

fn load_token(path: &Path) -> Result<String, crate::Error> {
    if !path.is_file() {
        return Err(format!("auth token file not found: {}", path.display()).into());
    }
    let raw = std::fs::read_to_string(path)?;
    let trimmed: String = raw.chars().filter(|c| !c.is_whitespace()).collect();
    if trimmed.is_empty() {
        return Err(format!("auth token file is empty: {}", path.display()).into());
    }
    Ok(trimmed)
}

/// Parse + DNS-resolve. Returns `(scheme, ip_literal, port)`.
///
/// Picks the first IPv4 the resolver returns — the nft carveout we emit
/// is IPv4-scoped, so handing back a v6 address would land in the LAN
/// reject block and fail to match.
fn resolve(raw: &str) -> Result<(&'static str, std::net::IpAddr, u16), crate::Error> {
    let parsed = url::Url::parse(raw)
        .map_err(|e| -> crate::Error { format!("invalid URL {raw:?}: {e}").into() })?;
    let scheme = match parsed.scheme() {
        "http" => "http",
        "https" => "https",
        other => return Err(format!("unsupported proxy scheme {other:?}: {raw}").into()),
    };
    let host = parsed
        .host_str()
        .ok_or_else(|| -> crate::Error { format!("proxy URL has no host: {raw}").into() })?;
    let port = parsed
        .port()
        .unwrap_or(if scheme == "http" { 80 } else { 443 });

    // `ToSocketAddrs::to_socket_addrs` returns `io::Error` on resolver failure.
    let addrs = (host, port)
        .to_socket_addrs()
        .map_err(|e| -> crate::Error { format!("cannot resolve {host}: {e}").into() })?;
    let v4 = addrs.clone().find(|a| a.is_ipv4());
    let addr = v4
        .or_else(|| addrs.into_iter().next())
        .ok_or_else(|| -> crate::Error { format!("no addresses for {host}").into() })?;
    Ok((scheme, addr.ip(), addr.port()))
}

/// Default sandbox-side auth-proxy port. Exposed for callers that want to
/// reference it by symbol rather than hard-coding the number; URL-derived
/// ports fall back to the scheme default (80 / 443) inside `resolve`.
#[allow(dead_code)]
pub const DEFAULT_PORT: u16 = paths::AUTH_PROXY_PORT;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_bad_scheme() {
        assert!(resolve("file:///etc/passwd").is_err());
    }

    #[test]
    fn rejects_missing_host() {
        assert!(resolve("http://").is_err());
    }

    #[test]
    fn resolves_ip_literal() {
        let (s, ip, p) = resolve("http://127.0.0.1:18080").unwrap();
        assert_eq!(s, "http");
        assert_eq!(ip.to_string(), "127.0.0.1");
        assert_eq!(p, 18080);
    }

    #[test]
    fn default_port_for_http() {
        let (_, _, p) = resolve("http://127.0.0.1").unwrap();
        assert_eq!(p, 80);
    }

    #[test]
    fn default_port_for_https() {
        let (_, _, p) = resolve("https://127.0.0.1").unwrap();
        assert_eq!(p, 443);
    }
}

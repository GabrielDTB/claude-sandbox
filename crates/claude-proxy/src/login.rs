//! Interactive PKCE out-of-band OAuth login.
//!
//! User opens the authorize URL in a browser on another machine, approves,
//! and pastes `<code>#<state>` back. We verify the returned state matches
//! the one we generated (defence against code-injection by a third party
//! luring the user to paste their code), exchange it at
//! `platform.claude.com/v1/oauth/token`, and write a creds file matching
//! Claude Code's on-disk shape.

use std::{io::BufRead, path::PathBuf};

use base64::Engine;
use rand::RngCore;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::{
    cli::LoginArgs,
    config::SystemConfig,
    constants::{AUTHORIZE_URL, CLIENT_ID, OAUTH_REDIRECT_URI, OAUTH_SCOPES, TOKEN_URL},
    creds::{save, OauthBlock},
    server::UpstreamClient,
};

pub async fn run(args: LoginArgs, config: &SystemConfig) -> Result<u8, crate::Error> {
    let creds_path: PathBuf = config
        .creds_path(args.creds, true)
        .ok_or("login needs a creds path (--creds, $CLAUDE_PROXY_CREDS, or config file)")?;

    let verifier = url_safe_random(64);
    let challenge = {
        let mut h = Sha256::new();
        h.update(verifier.as_bytes());
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(h.finalize())
    };
    let state = url_safe_random(32);

    // Order of params doesn't matter for the authorize endpoint, but keep the
    // same set as Python for visual parity in logs.
    let authorize_url = {
        let mut url = url::Url::parse(AUTHORIZE_URL)?;
        url.query_pairs_mut()
            .append_pair("code", "true")
            .append_pair("client_id", CLIENT_ID)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", OAUTH_REDIRECT_URI)
            .append_pair("scope", OAUTH_SCOPES)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &state);
        url.to_string()
    };

    eprintln!("Open this URL in a browser and approve the request:");
    eprintln!();
    eprintln!("  {authorize_url}");
    eprintln!();
    eprintln!("The page will display an authorization code.");
    eprintln!("Paste it here (format: <code>#<state>):");
    eprintln!();
    eprint!("code: ");

    let mut line = String::new();
    let stdin = std::io::stdin();
    stdin.lock().read_line(&mut line)?;
    let entered = line.trim();
    if entered.is_empty() {
        eprintln!("error: empty code");
        return Ok(1);
    }

    let (code, returned_state) = match entered.split_once('#') {
        Some((c, s)) => (c.to_string(), s.to_string()),
        None => (entered.to_string(), String::new()),
    };

    if !returned_state.is_empty() && returned_state != state {
        eprintln!(
            "error: state mismatch — the code was not issued for this login session. Start over."
        );
        return Ok(1);
    }

    let body = serde_json::json!({
        "grant_type": "authorization_code",
        "code": code,
        "redirect_uri": OAUTH_REDIRECT_URI,
        "client_id": CLIENT_ID,
        "code_verifier": verifier,
        "state": state,
    });

    #[derive(Deserialize)]
    struct TokenResp {
        access_token: String,
        refresh_token: String,
        expires_in: i64,
        #[serde(default)]
        scope: Option<String>,
    }

    let http = UpstreamClient::new()?;
    let resp: TokenResp = http.post_json(TOKEN_URL, &body).await.map_err(|e| {
        // Python prints the HTTP status + body; UpstreamClient::post_json
        // already includes that in its error.
        format!("token exchange failed: {e}")
    })?;

    let scopes: Vec<String> = match resp.scope {
        Some(s) => s.split_whitespace().map(str::to_string).collect(),
        None => OAUTH_SCOPES.split_whitespace().map(str::to_string).collect(),
    };

    let block = OauthBlock {
        access_token: Some(resp.access_token),
        refresh_token: Some(resp.refresh_token),
        expires_at: crate::creds::now_ms() + resp.expires_in * 1000,
        scopes,
        extra: Default::default(),
    };
    save(&creds_path, &block)?;

    eprintln!();
    eprintln!("wrote credentials to {}", creds_path.display());
    eprintln!("access token expires in {}s", resp.expires_in);
    eprintln!(
        "a running `serve` will pick up the new credentials on the next request — no restart needed"
    );
    Ok(0)
}

/// `secrets.token_urlsafe(n)` clone: n random bytes, base64url-no-pad encoded.
fn url_safe_random(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut buf);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_matches_rfc7636_example() {
        // Example from RFC 7636 appendix B.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let mut h = Sha256::new();
        h.update(verifier.as_bytes());
        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(h.finalize());
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }
}

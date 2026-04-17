//! External contracts with Anthropic's OAuth + API backend. These MUST NOT
//! change without a server-side change; they are copied verbatim from the
//! Python reference implementation.

pub const API_HOST: &str = "api.anthropic.com";

pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";

pub const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
pub const OAUTH_REDIRECT_URI: &str = "https://console.anthropic.com/oauth/code/callback";
pub const OAUTH_SCOPES: &str = "org:create_api_key user:profile user:inference";

pub const ALLOWED_PREFIXES: &[&str] = &["/v1/", "/api/oauth/claude_cli/"];

/// Refresh access token this many seconds before actual expiry (clock-skew margin).
pub const REFRESH_MARGIN_S: u64 = 300;
/// Stop a trickle-fed request body from tying up a worker forever.
pub const REQUEST_READ_TIMEOUT_S: u64 = 60;
/// Cap on any single upstream round-trip (streams can still run longer because
/// this is wall-clock-per-chunk, not total).
pub const UPSTREAM_TIMEOUT_S: u64 = 300;

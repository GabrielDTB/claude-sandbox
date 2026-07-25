//! External contracts with Anthropic's OAuth + API backend. These MUST NOT
//! change without a server-side change; they are copied verbatim from the
//! Python reference implementation.

pub const API_HOST: &str = "api.anthropic.com";

pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";

pub const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
pub const OAUTH_REDIRECT_URI: &str = "https://platform.claude.com/oauth/code/callback";
pub const OAUTH_SCOPES: &str =
    "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";

pub const ALLOWED_PREFIXES: &[&str] = &["/v1/", "/api/oauth/claude_cli/"];

/// Local route (never forwarded) that reports the account's plan tier to the
/// *launcher*, so it can export `CLAUDE_CODE_SUBSCRIPTION_TYPE` /
/// `CLAUDE_CODE_RATE_LIMIT_TIER` into the sandbox.
///
/// This exists because Claude Code resolves those two values from
/// `{BASE_API_URL}/api/oauth/profile` with `BASE_API_URL` hardcoded to
/// `api.anthropic.com` — it does *not* honour `ANTHROPIC_BASE_URL`. A
/// sandboxed Claude therefore can never fetch its own profile: its bearer is
/// the proxy token, which the real API rejects. The launcher asks us instead.
///
/// Deliberately not a forwarded path: `/api/oauth/profile` is not in
/// `ALLOWED_PREFIXES` and must not be added, because the upstream response
/// carries the account email and the account / organization UUIDs. This route
/// returns only the two tier fields, so identity never reaches the sandbox.
/// The `_sandbox/` prefix cannot collide with a real Anthropic path.
pub const SUBSCRIPTION_PATH: &str = "/_sandbox/subscription";

/// Upstream profile endpoint backing [`SUBSCRIPTION_PATH`].
pub const PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";

/// How long a fetched profile stays fresh. Matches Claude Code's own profile
/// TTL (24h), and keeps the per-launch cost to a loopback round-trip rather
/// than an upstream one.
pub const SUBSCRIPTION_TTL_S: u64 = 86_400;

/// `organization.organization_type` → the `subscriptionType` Claude Code
/// expects. Copied verbatim from the Claude Code bundle; an organization type
/// absent from this table maps to `null`, exactly as Claude Code does.
pub const ORG_TYPE_TO_SUBSCRIPTION: &[(&str, &str)] = &[
    ("claude_max", "max"),
    ("claude_pro", "pro"),
    ("claude_enterprise", "enterprise"),
    ("claude_team", "team"),
];

/// Refresh access token this many seconds before actual expiry (clock-skew margin).
pub const REFRESH_MARGIN_S: u64 = 300;
/// Stop a trickle-fed request body from tying up a worker forever.
pub const REQUEST_READ_TIMEOUT_S: u64 = 60;
/// Cap on any single upstream round-trip (streams can still run longer because
/// this is wall-clock-per-chunk, not total).
pub const UPSTREAM_TIMEOUT_S: u64 = 300;

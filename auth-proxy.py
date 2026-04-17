#!/usr/bin/env python3
"""
Credential proxy for sandboxed Claude Code.

Forwards requests to api.anthropic.com with the real OAuth token injected,
so the sandbox itself never sees the credentials. The sandbox authenticates
itself to the proxy with a minted token; the proxy strips that token and
replaces it with the upstream OAuth bearer on every forwarded request.

Subcommands:
  serve    run the HTTP server
  login    interactive out-of-band PKCE OAuth; writes creds file
  mint     create a new sandbox token (prints raw token to stdout once)
  list     show stored tokens
  revoke   revoke a stored token by id

The token store is a JSON file; mutations are serialized with flock.
A running 'serve' picks up mint/revoke changes automatically via mtime
polling. The same mtime trick is used for the creds file: running
`login` against the creds path a live `serve` is watching is picked up
on the next request without restart.
"""

import argparse
import base64
import fcntl
import grp
import hashlib
import http.client
import http.server
import json
import os
import pwd
import secrets
import signal
import socket
import socketserver
import ssl
import sys
import threading
import time
import urllib.parse

API_HOST = "api.anthropic.com"
CLIENT_ID = "9d1c250a-e61b-44d9-88ed-5944d1962f5e"
TOKEN_URL_HOST = "platform.claude.com"
TOKEN_URL_PATH = "/v1/oauth/token"
REFRESH_MARGIN_S = 300  # refresh 5 minutes before expiry
REQUEST_READ_TIMEOUT_S = 60  # body read timeout to stop trickle-fed DoS
UPSTREAM_TIMEOUT_S = 300
ALLOWED_PREFIXES = ("/v1/", "/api/oauth/claude_cli/")

# System-wide config. Written by the NixOS module; tells the CLI which user
# to drop privileges to and which state-file paths to use. Missing file ==
# "not a managed system install" and the CLI falls back to flags/env.
DEFAULT_CONFIG_PATH = "/etc/claude-proxy/config.json"


# ---------------------------------------------------------------------------
# System config + privilege drop
# ---------------------------------------------------------------------------

def _load_system_config() -> dict:
    """Read /etc/claude-proxy/config.json (or $CLAUDE_PROXY_CONFIG). Silent
    fallback to an empty dict on any error — this file is optional."""
    path = os.environ.get("CLAUDE_PROXY_CONFIG") or DEFAULT_CONFIG_PATH
    try:
        with open(path) as f:
            data = json.load(f)
        return data if isinstance(data, dict) else {}
    except (FileNotFoundError, PermissionError, json.JSONDecodeError):
        return {}


def _drop_privs(user: str, group: str | None = None) -> None:
    """setgid+setuid to `user`. Must be called as root. Irreversible.

    Does not return success; raises on failure. Updates HOME so anything
    downstream that reads ~ sees the target user's home, not root's.
    """
    pw = pwd.getpwnam(user)
    gid = grp.getgrnam(group).gr_gid if group else pw.pw_gid
    os.setgroups([gid])
    os.setgid(gid)
    os.setuid(pw.pw_uid)
    os.environ["HOME"] = pw.pw_dir
    os.environ["USER"] = pw.pw_name
    os.environ["LOGNAME"] = pw.pw_name


def _enforce_root_and_drop(config: dict, cmd: str) -> None:
    """Headscale-style: when /etc/claude-proxy/config.json exists, require
    root for state-changing commands and drop to the configured service
    user before touching files. Does nothing when no config is present
    (dev / standalone use)."""
    user = config.get("user")
    if not user:
        return  # no managed config → skip priv logic entirely
    if os.geteuid() != 0:
        print(
            f"error: `claude-proxy {cmd}` must be run as root on a managed "
            f"install (config present at {os.environ.get('CLAUDE_PROXY_CONFIG') or DEFAULT_CONFIG_PATH}). "
            f"Use `sudo claude-proxy {cmd} ...`.",
            file=sys.stderr,
        )
        sys.exit(2)
    try:
        _drop_privs(user, config.get("group"))
    except (KeyError, PermissionError, OSError) as e:
        print(f"error: failed to drop privileges to {user!r}: {e}", file=sys.stderr)
        sys.exit(1)


# ---------------------------------------------------------------------------
# Token store (JSON on disk, coordinated via flock)
# ---------------------------------------------------------------------------

def _hash(token: str) -> str:
    return hashlib.sha256(token.encode()).hexdigest()


def _load_store_locked(path: str) -> dict:
    """Caller already holds a shared or exclusive lock on the file."""
    with open(path) as f:
        f.seek(0)
        data = f.read()
    if not data.strip():
        return {"tokens": []}
    return json.loads(data)


def _write_store_atomic(path: str, store: dict) -> None:
    """Atomic rename. Caller holds LOCK_EX on an fd for `path` (if it exists)."""
    tmp = f"{path}.tmp"
    with open(tmp, "w") as f:
        json.dump(store, f, indent=2)
        f.flush()
        os.fsync(f.fileno())
    os.rename(tmp, path)


def _open_for_lock(path: str, create: bool):
    """Open (or create) the store for locking. Returns fd."""
    if create and not os.path.exists(path):
        os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
        # Create with an empty object. O_CREAT|O_EXCL would race with concurrent
        # mint calls; open O_CREAT without EXCL, then flock before touching.
        fd = os.open(path, os.O_RDWR | os.O_CREAT, 0o600)
        # Initialise only if empty.
        size = os.fstat(fd).st_size
        if size == 0:
            fcntl.flock(fd, fcntl.LOCK_EX)
            try:
                # Re-check under lock.
                if os.fstat(fd).st_size == 0:
                    os.write(fd, b'{"tokens": []}\n')
            finally:
                fcntl.flock(fd, fcntl.LOCK_UN)
        return fd
    return os.open(path, os.O_RDWR)


def mint_cmd(args) -> int:
    _maybe_warn_unauth(args.creds)
    fd = _open_for_lock(args.token_store, create=True)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX)
        store = _load_store_locked(args.token_store)
        raw = secrets.token_hex(32)
        entry = {
            "id": secrets.token_hex(4),
            "name": args.name or "",
            "hash": _hash(raw),
            "created_at": int(time.time()),
            "revoked_at": None,
        }
        store.setdefault("tokens", []).append(entry)
        _write_store_atomic(args.token_store, store)
        fcntl.flock(fd, fcntl.LOCK_UN)
    finally:
        os.close(fd)
    # Print to stdout so the user can capture it. Raw token is never printed again.
    print(raw)
    print(f"(id: {entry['id']}, name: {entry['name'] or '<none>'})", file=sys.stderr)
    return 0


def list_cmd(args) -> int:
    _maybe_warn_unauth(args.creds)
    fd = _open_for_lock(args.token_store, create=False)
    try:
        fcntl.flock(fd, fcntl.LOCK_SH)
        store = _load_store_locked(args.token_store)
        fcntl.flock(fd, fcntl.LOCK_UN)
    finally:
        os.close(fd)
    tokens = store.get("tokens", [])
    if not tokens:
        print("(no tokens)")
        return 0
    print(f"{'ID':<10} {'NAME':<20} {'CREATED':<20} {'STATUS'}")
    for t in tokens:
        created = time.strftime("%Y-%m-%d %H:%M:%S", time.localtime(t["created_at"]))
        status = "revoked" if t.get("revoked_at") else "active"
        print(f"{t['id']:<10} {(t.get('name') or ''):<20} {created:<20} {status}")
    return 0


def revoke_cmd(args) -> int:
    _maybe_warn_unauth(args.creds)
    fd = _open_for_lock(args.token_store, create=False)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX)
        store = _load_store_locked(args.token_store)
        found = False
        for t in store.get("tokens", []):
            if t["id"] == args.id:
                if t.get("revoked_at"):
                    print(f"token {args.id} already revoked", file=sys.stderr)
                    return 1
                t["revoked_at"] = int(time.time())
                found = True
                break
        if not found:
            print(f"token {args.id} not found", file=sys.stderr)
            return 1
        _write_store_atomic(args.token_store, store)
        fcntl.flock(fd, fcntl.LOCK_UN)
    finally:
        os.close(fd)
    print(f"revoked {args.id}")
    return 0


# ---------------------------------------------------------------------------
# Server-side token cache (reloaded when the store file's mtime changes)
# ---------------------------------------------------------------------------

class TokenAuth:
    """Thread-safe auth check backed by either an ephemeral token or a store file."""

    def __init__(self, *, store_path: str | None, initial_token: str | None):
        self.store_path = store_path
        self._lock = threading.Lock()
        # (mtime, {hash -> entry}) — dict swap is atomic under GIL, but lock
        # around the swap to keep mtime and dict consistent.
        self._mtime = 0.0
        self._hashes: dict[str, dict] = {}
        if initial_token is not None:
            self._hashes = {
                _hash(initial_token): {"id": "ephemeral", "name": "initial", "revoked_at": None}
            }
        elif store_path is not None:
            self._reload()

    def _reload(self) -> None:
        fd = os.open(self.store_path, os.O_RDONLY)
        try:
            fcntl.flock(fd, fcntl.LOCK_SH)
            try:
                with os.fdopen(fd, closefd=False) as f:
                    data = f.read()
            finally:
                fcntl.flock(fd, fcntl.LOCK_UN)
        finally:
            os.close(fd)
        store = json.loads(data) if data.strip() else {"tokens": []}
        new_hashes: dict[str, dict] = {}
        for t in store.get("tokens", []):
            new_hashes[t["hash"]] = t
        with self._lock:
            self._hashes = new_hashes

    def _maybe_reload(self) -> None:
        if self.store_path is None:
            return
        try:
            mtime = os.stat(self.store_path).st_mtime
        except FileNotFoundError:
            return
        with self._lock:
            stale = mtime != self._mtime
            if stale:
                self._mtime = mtime
        if stale:
            try:
                self._reload()
            except Exception as e:
                print(f"[auth-proxy] token store reload failed: {e}", file=sys.stderr)

    def check(self, token: str | None) -> bool:
        if not token:
            return False
        self._maybe_reload()
        with self._lock:
            entry = self._hashes.get(_hash(token))
        if entry is None:
            return False
        return not entry.get("revoked_at")


# ---------------------------------------------------------------------------
# OAuth credentials (shared, refreshed in place)
# ---------------------------------------------------------------------------

class Credentials:
    """OAuth creds backed by a JSON file.

    The file may be absent at startup (service was enabled before anyone ran
    `login`); in that case has_credentials() returns False and
    get_access_token() returns None. A subsequent `login` that writes the
    file is picked up on the next request via mtime reload — no restart.
    """

    def __init__(self, path: str):
        self.path = path
        self.lock = threading.Lock()
        self.access_token: str | None = None
        self.refresh_token: str | None = None
        self.expires_at: float = 0.0
        self.scopes: str = ""
        self._mtime: float = 0.0
        self._load_if_possible()

    def _load_if_possible(self) -> bool:
        """Populate in-memory fields from disk. Returns True on success.

        Does not raise on missing/empty/malformed file; leaves the instance
        unauthenticated instead. Must be called with self.lock held.
        """
        try:
            st = os.stat(self.path)
        except FileNotFoundError:
            self.access_token = None
            self.refresh_token = None
            self.expires_at = 0.0
            self.scopes = ""
            self._mtime = 0.0
            return False
        try:
            with open(self.path) as f:
                data = json.load(f)
            oauth = data.get("claudeAiOauth") or {}
            if not oauth.get("refreshToken"):
                raise ValueError("missing refreshToken")
            self.access_token = oauth.get("accessToken")
            self.refresh_token = oauth["refreshToken"]
            self.expires_at = float(oauth.get("expiresAt", 0)) / 1000.0
            self.scopes = " ".join(oauth.get("scopes", []))
            self._mtime = st.st_mtime
            return True
        except (OSError, ValueError, json.JSONDecodeError) as e:
            print(f"[auth-proxy] creds load failed: {e}", file=sys.stderr)
            self.access_token = None
            self.refresh_token = None
            self.expires_at = 0.0
            self.scopes = ""
            self._mtime = st.st_mtime  # don't retry until file changes
            return False

    def _maybe_reload(self) -> None:
        try:
            mtime = os.stat(self.path).st_mtime
        except FileNotFoundError:
            mtime = 0.0
        if mtime == self._mtime:
            return
        with self.lock:
            if mtime == self._mtime:
                return
            self._load_if_possible()

    def _save(self) -> None:
        """Atomic write. Called under self.lock."""
        try:
            with open(self.path) as f:
                data = json.load(f)
        except (FileNotFoundError, json.JSONDecodeError):
            data = {}
        data["claudeAiOauth"] = {
            "accessToken": self.access_token,
            "refreshToken": self.refresh_token,
            "expiresAt": int(self.expires_at * 1000),
            "scopes": self.scopes.split() if self.scopes else [],
        }
        os.makedirs(os.path.dirname(self.path) or ".", exist_ok=True)
        tmp = f"{self.path}.tmp"
        with open(tmp, "w") as f:
            json.dump(data, f)
            f.flush()
            os.fsync(f.fileno())
        os.chmod(tmp, 0o600)
        os.rename(tmp, self.path)
        try:
            self._mtime = os.stat(self.path).st_mtime
        except FileNotFoundError:
            pass

    def _refresh(self) -> bool:
        """Called under self.lock."""
        if not self.refresh_token:
            return False
        body = json.dumps({
            "grant_type": "refresh_token",
            "refresh_token": self.refresh_token,
            "client_id": CLIENT_ID,
            "scope": self.scopes,
        }).encode()
        ctx = ssl.create_default_context()
        conn = http.client.HTTPSConnection(TOKEN_URL_HOST, timeout=15, context=ctx)
        try:
            conn.request("POST", TOKEN_URL_PATH, body=body,
                         headers={"Content-Type": "application/json"})
            resp = conn.getresponse()
            if resp.status != 200:
                print(f"[auth-proxy] token refresh failed: {resp.status} {resp.read().decode()}",
                      file=sys.stderr)
                return False
            data = json.loads(resp.read())
            self.access_token = data["access_token"]
            if data.get("refresh_token"):
                self.refresh_token = data["refresh_token"]
            self.expires_at = time.time() + data["expires_in"]
            self._save()
            print("[auth-proxy] token refreshed", file=sys.stderr)
            return True
        finally:
            conn.close()

    def has_credentials(self) -> bool:
        self._maybe_reload()
        return bool(self.refresh_token)

    def get_access_token(self) -> str | None:
        self._maybe_reload()
        with self.lock:
            if not self.refresh_token:
                return None
            if time.time() > self.expires_at - REFRESH_MARGIN_S:
                if not self._refresh():
                    return None
            return self.access_token


# ---------------------------------------------------------------------------
# HTTP handler
# ---------------------------------------------------------------------------

class ProxyHandler(http.server.BaseHTTPRequestHandler):
    # Set by the server.
    auth: TokenAuth = None  # type: ignore[assignment]
    creds: Credentials = None  # type: ignore[assignment]

    # Keep HTTP/1.0 default: connection-close terminates the response, so we
    # don't need to re-chunk streamed upstream bodies.

    def setup(self) -> None:
        super().setup()
        # Body-read timeout: stops a trickle-fed `Content-Length: <huge>` from
        # tying up a worker thread forever.
        try:
            self.request.settimeout(REQUEST_READ_TIMEOUT_S)
        except OSError:
            pass

    def _extract_bearer(self) -> str | None:
        h = self.headers.get("Authorization") or self.headers.get("authorization")
        if not h or not h.lower().startswith("bearer "):
            return None
        return h[7:].strip()

    def _send_unauth_error(self) -> None:
        """Return a JSON error in Anthropic's envelope shape so Claude Code
        surfaces the message verbatim instead of a generic 5xx."""
        body = json.dumps({
            "type": "error",
            "error": {
                "type": "authentication_error",
                "message": (
                    "claude-proxy is not authenticated. "
                    "Run `claude-proxy login --creds <path>` "
                    "on the proxy host to authenticate."
                ),
            },
        }).encode()
        self.send_response(503)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        try:
            self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError):
            pass

    def do_request(self) -> None:
        if not any(self.path.startswith(p) for p in ALLOWED_PREFIXES):
            self.send_error(403, f"Path not allowed: {self.path}")
            return

        if not self.auth.check(self._extract_bearer()):
            self.send_error(401, "Unauthorized")
            return

        try:
            content_length = int(self.headers.get("Content-Length", "0") or "0")
            if content_length < 0:
                raise ValueError
        except ValueError:
            self.send_error(400, "Invalid Content-Length")
            return

        try:
            body = self.rfile.read(content_length) if content_length else None
        except (socket.timeout, TimeoutError):
            self.send_error(408, "Request body read timed out")
            return
        except (ConnectionError, OSError) as e:
            print(f"[auth-proxy] body read error: {e}", file=sys.stderr)
            return

        access_token = self.creds.get_access_token()
        if not access_token:
            self._send_unauth_error()
            return

        fwd_headers: dict[str, str] = {}
        drop = {"host", "authorization", "x-api-key", "connection",
                "transfer-encoding", "proxy-authorization", "proxy-connection",
                "keep-alive", "te", "trailer", "upgrade"}
        for key in self.headers:
            if key.lower() in drop:
                continue
            fwd_headers[key] = self.headers[key]
        fwd_headers["Host"] = API_HOST
        fwd_headers["Authorization"] = f"Bearer {access_token}"

        ctx = ssl.create_default_context()
        conn = http.client.HTTPSConnection(API_HOST, timeout=UPSTREAM_TIMEOUT_S, context=ctx)
        try:
            conn.request(self.command, self.path, body=body, headers=fwd_headers)
            resp = conn.getresponse()

            self.send_response(resp.status)
            is_streaming = False
            for key, value in resp.getheaders():
                lower = key.lower()
                # Drop hop-by-hop and length headers that no longer apply after
                # http.client has dechunked the upstream body for us.
                if lower in ("transfer-encoding", "connection", "keep-alive",
                             "content-length"):
                    continue
                if lower == "content-type" and "text/event-stream" in value:
                    is_streaming = True
                self.send_header(key, value)
            self.end_headers()

            if is_streaming:
                while True:
                    chunk = resp.read(4096)
                    if not chunk:
                        break
                    self.wfile.write(chunk)
                    self.wfile.flush()
            else:
                self.wfile.write(resp.read())
        except (BrokenPipeError, ConnectionResetError):
            pass
        except Exception as e:
            print(f"[auth-proxy] upstream error: {e}", file=sys.stderr)
            try:
                self.send_error(502, f"Upstream error: {e}")
            except OSError:
                pass
        finally:
            conn.close()

    do_GET = do_request
    do_POST = do_request
    do_PUT = do_request
    do_DELETE = do_request
    do_PATCH = do_request

    def log_message(self, format, *args):
        tname = threading.current_thread().name
        msg = (format % args) if args else format
        print(f"[auth-proxy {tname}] {msg}", file=sys.stderr)


class ThreadingServer(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True
    allow_reuse_address = True


def _parse_bind(spec: str) -> tuple[str, int]:
    # Accept "host:port" and "[ipv6]:port".
    if spec.startswith("["):
        host_end = spec.rfind("]")
        host = spec[1:host_end]
        port = int(spec[host_end + 2:])
    else:
        host, _, port_s = spec.rpartition(":")
        if not host:
            host = "0.0.0.0"
        port = int(port_s)
    return host, port


def _warn_unauth(creds_path: str | None) -> None:
    """Print a one-line stderr hint that the proxy is unauthenticated.

    Emitted from every subcommand that knows the creds path, so operators
    running `list` / `mint` / `revoke` / `serve` don't silently talk to a
    proxy that can't actually forward requests.
    """
    if creds_path:
        print(
            f"[auth-proxy] warning: proxy is not authenticated — run "
            f"`claude-proxy login --creds {creds_path}` to authenticate",
            file=sys.stderr,
        )
    else:
        print(
            "[auth-proxy] warning: proxy is not authenticated — run "
            "`claude-proxy login --creds <path>` to authenticate",
            file=sys.stderr,
        )


def _maybe_warn_unauth(creds_path: str | None) -> None:
    """Called from CLI subcommands that take an optional --creds; warns if
    that file is missing/empty. No-op when creds_path is None."""
    if not creds_path:
        return
    try:
        st = os.stat(creds_path)
    except FileNotFoundError:
        _warn_unauth(creds_path)
        return
    if st.st_size == 0:
        _warn_unauth(creds_path)
        return
    # Cheap validity probe: must parse and have a refreshToken.
    try:
        with open(creds_path) as f:
            data = json.load(f)
        if not (data.get("claudeAiOauth") or {}).get("refreshToken"):
            _warn_unauth(creds_path)
    except (OSError, json.JSONDecodeError):
        _warn_unauth(creds_path)


# ---------------------------------------------------------------------------
# Login (PKCE OAuth, out-of-band)
# ---------------------------------------------------------------------------

# Matches what the Claude Code CLI uses: claude.ai hosts the authorize page;
# after approval it redirects to a callback page on console.anthropic.com
# that shows the auth code for the user to paste back. Format is
# "<code>#<state>".
AUTHORIZE_URL = "https://claude.ai/oauth/authorize"
OAUTH_REDIRECT_URI = "https://console.anthropic.com/oauth/code/callback"
OAUTH_SCOPES = "org:create_api_key user:profile user:inference"


def login_cmd(args) -> int:
    verifier = secrets.token_urlsafe(64)
    challenge = (
        base64.urlsafe_b64encode(hashlib.sha256(verifier.encode()).digest())
        .rstrip(b"=")
        .decode()
    )
    state = secrets.token_urlsafe(32)

    params = {
        "code": "true",
        "client_id": CLIENT_ID,
        "response_type": "code",
        "redirect_uri": OAUTH_REDIRECT_URI,
        "scope": OAUTH_SCOPES,
        "code_challenge": challenge,
        "code_challenge_method": "S256",
        "state": state,
    }
    authorize_url = AUTHORIZE_URL + "?" + urllib.parse.urlencode(params)

    print("Open this URL in a browser and approve the request:", file=sys.stderr)
    print("", file=sys.stderr)
    print(f"  {authorize_url}", file=sys.stderr)
    print("", file=sys.stderr)
    print("The page will display an authorization code.", file=sys.stderr)
    print("Paste it here (format: <code>#<state>):", file=sys.stderr)
    print("", file=sys.stderr)

    try:
        entered = input("code: ").strip()
    except EOFError:
        print("error: no input", file=sys.stderr)
        return 1
    if not entered:
        print("error: empty code", file=sys.stderr)
        return 1

    if "#" in entered:
        code, _, returned_state = entered.partition("#")
    else:
        code, returned_state = entered, ""

    if returned_state and returned_state != state:
        print("error: state mismatch — the code was not issued for this "
              "login session. Start over.", file=sys.stderr)
        return 1

    body = json.dumps({
        "grant_type": "authorization_code",
        "code": code,
        "redirect_uri": OAUTH_REDIRECT_URI,
        "client_id": CLIENT_ID,
        "code_verifier": verifier,
        "state": state,
    }).encode()
    ctx = ssl.create_default_context()
    conn = http.client.HTTPSConnection(TOKEN_URL_HOST, timeout=30, context=ctx)
    try:
        conn.request("POST", TOKEN_URL_PATH, body=body,
                     headers={"Content-Type": "application/json"})
        resp = conn.getresponse()
        resp_body = resp.read().decode(errors="replace")
        if resp.status != 200:
            print(f"error: token exchange failed: {resp.status} {resp_body}",
                  file=sys.stderr)
            return 1
        data = json.loads(resp_body)
    finally:
        conn.close()

    try:
        access_token = data["access_token"]
        refresh_token = data["refresh_token"]
        expires_in = data["expires_in"]
    except KeyError as e:
        print(f"error: token response missing {e}: {data}", file=sys.stderr)
        return 1

    scope = data.get("scope", OAUTH_SCOPES)
    scopes_list = scope.split() if isinstance(scope, str) else list(scope)

    # Preserve any existing top-level keys in the creds file (matches Claude
    # Code's on-disk shape; the refresh path already round-trips this way).
    try:
        with open(args.creds) as f:
            existing = json.load(f)
        if not isinstance(existing, dict):
            existing = {}
    except (FileNotFoundError, json.JSONDecodeError):
        existing = {}

    existing["claudeAiOauth"] = {
        "accessToken": access_token,
        "refreshToken": refresh_token,
        "expiresAt": int((time.time() + expires_in) * 1000),
        "scopes": scopes_list,
    }

    os.makedirs(os.path.dirname(args.creds) or ".", exist_ok=True)
    tmp = f"{args.creds}.tmp"
    with open(tmp, "w") as f:
        json.dump(existing, f, indent=2)
        f.flush()
        os.fsync(f.fileno())
    os.chmod(tmp, 0o600)
    os.rename(tmp, args.creds)

    print("", file=sys.stderr)
    print(f"wrote credentials to {args.creds}", file=sys.stderr)
    print(f"access token expires in {expires_in}s", file=sys.stderr)
    print("a running `serve` will pick up the new credentials on the "
          "next request — no restart needed", file=sys.stderr)
    return 0


def serve_cmd(args) -> int:
    if args.token_store and args.initial_token_env:
        print("error: --token-store and --initial-token-env are mutually exclusive", file=sys.stderr)
        return 2
    if not args.token_store and not args.initial_token_env:
        print("error: one of --token-store or --initial-token-env is required", file=sys.stderr)
        return 2

    if args.initial_token_env:
        tok = os.environ.get(args.initial_token_env)
        if not tok:
            print(f"error: env var {args.initial_token_env} is empty or unset", file=sys.stderr)
            return 2
        auth = TokenAuth(store_path=None, initial_token=tok)
    else:
        # Bootstrap an empty store on first boot so the service can start
        # without pre-provisioning. Admin still has to `mint` before any
        # sandbox can auth; until then, every request returns 401.
        if not os.path.exists(args.token_store):
            fd = _open_for_lock(args.token_store, create=True)
            os.close(fd)
            print(f"[auth-proxy] initialised empty token store at {args.token_store} — "
                  f"run `claude-proxy mint` before any client can authenticate",
                  file=sys.stderr)
        auth = TokenAuth(store_path=args.token_store, initial_token=None)

    creds = Credentials(args.creds)
    if creds.has_credentials():
        print(f"[auth-proxy] loaded credentials from {args.creds}, "
              f"access token expires in "
              f"{int(creds.expires_at - time.time())}s", file=sys.stderr)
    else:
        _warn_unauth(args.creds)

    ProxyHandler.auth = auth
    ProxyHandler.creds = creds

    host, port = _parse_bind(args.bind)
    server = ThreadingServer((host, port), ProxyHandler)

    stop = threading.Event()

    def _sig_stop(signum, frame):
        print(f"[auth-proxy] received signal {signum}, shutting down", file=sys.stderr)
        stop.set()
        # server.shutdown() must run from a different thread than serve_forever.
        threading.Thread(target=server.shutdown, daemon=True).start()

    signal.signal(signal.SIGTERM, _sig_stop)
    signal.signal(signal.SIGINT, _sig_stop)

    print(f"[auth-proxy] listening on {host}:{port}", file=sys.stderr)
    try:
        server.serve_forever()
    finally:
        server.server_close()
    return 0


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------

def _creds_default(config: dict, required: bool) -> str | None:
    """Layer: explicit config > env > (if required) ~/.claude fallback."""
    if config.get("credentials_file"):
        return config["credentials_file"]
    v = os.environ.get("CLAUDE_PROXY_CREDS") or os.environ.get("CLAUDE_CREDENTIALS")
    if v:
        return os.path.expanduser(v)
    if required:
        return os.path.expanduser("~/.claude/.credentials.json")
    return None


def _store_default(config: dict) -> str | None:
    if config.get("token_store"):
        return config["token_store"]
    v = os.environ.get("CLAUDE_PROXY_TOKEN_STORE")
    return os.path.expanduser(v) if v else None


def main(argv: list[str] | None = None) -> int:
    config = _load_system_config()

    p = argparse.ArgumentParser(prog="claude-proxy",
                                description="OAuth forwarding proxy for sandboxed Claude Code")
    sub = p.add_subparsers(dest="cmd", required=True)

    serve_creds = _creds_default(config, required=True)
    store_default = _store_default(config)
    # If config supplies token_store, don't force the flag; otherwise keep
    # it required so serve/mint/etc fail loudly when they have no path.
    store_required = store_default is None

    s = sub.add_parser("serve", help="run the proxy server")
    s.add_argument("--bind", default="0.0.0.0:18080",
                   help="address:port to listen on (default: 0.0.0.0:18080)")
    s.add_argument("--creds", default=serve_creds,
                   help="path to OAuth credentials file (populated by `login`; "
                        "env: CLAUDE_PROXY_CREDS)")
    s.add_argument("--token-store", default=store_default,
                   help="path to persistent token store JSON "
                        "(env: CLAUDE_PROXY_TOKEN_STORE)")
    s.add_argument("--initial-token-env",
                   help="name of env var containing the sole accepted token (ephemeral mode)")
    # `serve` runs under the systemd User= in managed installs, so it never
    # needs priv-drop. In dev / standalone use it runs as the invoking user.
    s.set_defaults(func=serve_cmd, requires_root=False)

    lg = sub.add_parser("login",
                        help="run interactive OAuth login and write the creds file")
    lg.add_argument("--creds", default=serve_creds,
                    help="path to write OAuth credentials to (env: CLAUDE_PROXY_CREDS)")
    lg.set_defaults(func=login_cmd, requires_root=True)

    m = sub.add_parser("mint", help="mint a new sandbox token")
    m.add_argument("--token-store", default=store_default, required=store_required,
                   help="path to token store JSON (env: CLAUDE_PROXY_TOKEN_STORE)")
    m.add_argument("--creds", default=_creds_default(config, required=False),
                   help="path to creds file (env: CLAUDE_PROXY_CREDS); "
                        "only used to warn if the proxy isn't authenticated")
    m.add_argument("--name", default=None, help="human-readable label")
    m.set_defaults(func=mint_cmd, requires_root=True)

    l = sub.add_parser("list", help="list tokens in the store")
    l.add_argument("--token-store", default=store_default, required=store_required,
                   help="path to token store JSON (env: CLAUDE_PROXY_TOKEN_STORE)")
    l.add_argument("--creds", default=_creds_default(config, required=False),
                   help="path to creds file (env: CLAUDE_PROXY_CREDS); "
                        "only used to warn if the proxy isn't authenticated")
    l.set_defaults(func=list_cmd, requires_root=True)

    r = sub.add_parser("revoke", help="revoke a token by id")
    r.add_argument("--token-store", default=store_default, required=store_required,
                   help="path to token store JSON (env: CLAUDE_PROXY_TOKEN_STORE)")
    r.add_argument("--creds", default=_creds_default(config, required=False),
                   help="path to creds file (env: CLAUDE_PROXY_CREDS); "
                        "only used to warn if the proxy isn't authenticated")
    r.add_argument("id", help="token id (from `list`)")
    r.set_defaults(func=revoke_cmd, requires_root=True)

    args = p.parse_args(argv)

    # Centralised priv handling: when /etc/claude-proxy/config.json exists,
    # state-touching commands require root and drop to the configured user
    # before any file I/O happens. No-op on dev / standalone (no config).
    if getattr(args, "requires_root", False):
        _enforce_root_and_drop(config, args.cmd)

    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())

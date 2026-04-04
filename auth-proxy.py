#!/usr/bin/env python3
"""
Credential proxy for sandboxed Claude Code.

Runs in its own container, forwarding API requests to api.anthropic.com
with real OAuth credentials injected. The sandbox container sets
ANTHROPIC_BASE_URL to point here and never sees the raw tokens.

Handles automatic token refresh when the access token expires.
"""

import http.server
import http.client
import json
import os
import ssl
import sys
import threading
import time

CREDENTIALS_PATH = os.path.expanduser(
    os.environ.get("CLAUDE_CREDENTIALS", "~/.claude/.credentials.json")
)
API_HOST = "api.anthropic.com"
CLIENT_ID = "9d1c250a-e61b-44d9-88ed-5944d1962f5e"
TOKEN_URL_HOST = "platform.claude.com"
TOKEN_URL_PATH = "/v1/oauth/token"
REFRESH_MARGIN_S = 300  # refresh 5 minutes before expiry

lock = threading.Lock()
credentials = {"access_token": None, "refresh_token": None, "expires_at": 0, "scopes": ""}


def load_credentials():
    with open(CREDENTIALS_PATH) as f:
        data = json.load(f)
    oauth = data["claudeAiOauth"]
    credentials["access_token"] = oauth["accessToken"]
    credentials["refresh_token"] = oauth["refreshToken"]
    credentials["expires_at"] = oauth["expiresAt"] / 1000  # ms -> s
    credentials["scopes"] = " ".join(oauth.get("scopes", []))


def save_credentials():
    with open(CREDENTIALS_PATH) as f:
        data = json.load(f)
    data["claudeAiOauth"]["accessToken"] = credentials["access_token"]
    data["claudeAiOauth"]["refreshToken"] = credentials["refresh_token"]
    data["claudeAiOauth"]["expiresAt"] = int(credentials["expires_at"] * 1000)
    with open(CREDENTIALS_PATH, "w") as f:
        json.dump(data, f)


def refresh_token():
    body = json.dumps({
        "grant_type": "refresh_token",
        "refresh_token": credentials["refresh_token"],
        "client_id": CLIENT_ID,
        "scope": credentials["scopes"],
    }).encode()

    ctx = ssl.create_default_context()
    conn = http.client.HTTPSConnection(TOKEN_URL_HOST, timeout=15, context=ctx)
    try:
        conn.request("POST", TOKEN_URL_PATH, body=body,
                      headers={"Content-Type": "application/json"})
        resp = conn.getresponse()
        if resp.status != 200:
            print(f"[auth-proxy] token refresh failed: {resp.status} {resp.read().decode()}", file=sys.stderr)
            return False
        data = json.loads(resp.read())
        credentials["access_token"] = data["access_token"]
        if data.get("refresh_token"):
            credentials["refresh_token"] = data["refresh_token"]
        credentials["expires_at"] = time.time() + data["expires_in"]
        save_credentials()
        print("[auth-proxy] token refreshed", file=sys.stderr)
        return True
    finally:
        conn.close()


def get_access_token():
    with lock:
        if time.time() > credentials["expires_at"] - REFRESH_MARGIN_S:
            refresh_token()
        return credentials["access_token"]


class ProxyHandler(http.server.BaseHTTPRequestHandler):
    # Paths that Claude Code needs to reach
    ALLOWED_PREFIXES = ("/v1/", "/api/oauth/claude_cli/")

    def do_request(self):
        if not any(self.path.startswith(p) for p in self.ALLOWED_PREFIXES):
            self.send_error(403, f"Path not allowed: {self.path}")
            return

        content_length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(content_length) if content_length else None

        # Build forwarded headers, injecting auth
        fwd_headers = {}
        for key in self.headers:
            lower = key.lower()
            # Drop hop-by-hop and auth headers from the container
            if lower in ("host", "authorization", "x-api-key", "connection",
                         "transfer-encoding", "proxy-authorization"):
                continue
            fwd_headers[key] = self.headers[key]
        fwd_headers["Host"] = API_HOST
        fwd_headers["Authorization"] = f"Bearer {get_access_token()}"

        ctx = ssl.create_default_context()
        conn = http.client.HTTPSConnection(API_HOST, timeout=300, context=ctx)
        try:
            conn.request(self.command, self.path, body=body, headers=fwd_headers)
            resp = conn.getresponse()

            self.send_response(resp.status)
            # Check if this is a streaming response
            is_streaming = False
            for key, value in resp.getheaders():
                lower = key.lower()
                if lower in ("transfer-encoding", "connection"):
                    continue
                if lower == "content-type" and "text/event-stream" in value:
                    is_streaming = True
                self.send_header(key, value)
            self.end_headers()

            if is_streaming:
                # Stream SSE chunks as they arrive
                while True:
                    chunk = resp.read(4096)
                    if not chunk:
                        break
                    self.wfile.write(chunk)
                    self.wfile.flush()
            else:
                self.wfile.write(resp.read())
        except Exception as e:
            print(f"[auth-proxy] upstream error: {e}", file=sys.stderr)
            self.send_error(502, f"Upstream error: {e}")
        finally:
            conn.close()

    do_GET = do_request
    do_POST = do_request
    do_PUT = do_request
    do_DELETE = do_request
    do_PATCH = do_request

    def log_message(self, format, *args):
        print(f"[auth-proxy] {args[0]}", file=sys.stderr)


def main():
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 18080
    load_credentials()
    print(f"[auth-proxy] loaded credentials, token expires in {int(credentials['expires_at'] - time.time())}s", file=sys.stderr)

    http.server.HTTPServer.allow_reuse_address = True
    server = http.server.HTTPServer(("0.0.0.0", port), ProxyHandler)
    print(f"[auth-proxy] listening on 0.0.0.0:{port}", file=sys.stderr)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    server.server_close()


if __name__ == "__main__":
    main()

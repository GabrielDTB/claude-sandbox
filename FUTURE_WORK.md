# Future Work

Known issues that have been considered but not yet addressed. See `HARDENING.md` for the current threat model and mitigations.

## Auth proxy rewrite (planned)

The current `auth-proxy.py` will be replaced. When doing so, the replacement should address the following that the current design does not:

- **Client authentication.** The proxy's host-side port is published on `127.0.0.1::<random>` (via `podman run -p`). Any process on the host running as the user can connect and use the OAuth tokens — the proxy accepts any caller. Two reasonable fixes: (a) bind to a Unix socket under `$SANDBOX_DIR` with `0600` perms and bind-mount the socket into the sandbox instead of using pasta `-T`; or (b) generate a random per-launch token, inject it into the sandbox as an env var for the HTTP client, validate it in the proxy's request handler.
- **Request robustness.** `int(self.headers.get("Content-Length", 0))` can raise on malformed input outside the try/except, crashing the handler. There's also no timeout on the request body read, so a trickle-fed `Content-Length: <huge>` ties up a worker until the proxy's 256m memory cap kills it.

## Terminal escape / clipboard vector

The sandbox's PTY output is not filtered. A malicious agent can emit arbitrary escape sequences to the user's terminal:

- **OSC 52 clipboard writes** — the sandbox can stuff arbitrary content into the user's host clipboard. If the user later pastes into another terminal, they may execute attacker-controlled commands. Risk is amplified if the user's clipboard manager has parsing bugs of its own.
- **Terminal response sequences** (DA1/DA2, cursor position reports) can inject characters into the shell input buffer after the sandbox exits.
- **Screen/cursor manipulation** can hide output or forge UI.

`tput sgr0` + `tput cnorm` on exit only reset attributes and cursor visibility — they don't flush the buffer or strip sequences that were already interpreted. Full mitigation requires a PTY-side filter that allowlists or strips escape sequences before forwarding to the host terminal.

## Concurrent launches against the same `box/`

`$SANDBOX_DIR/setup-firewall.sh` is rewritten on every launcher invocation at a fixed path. Two concurrent launches sharing the same `box/` (and therefore the same `.claude-sandbox-state/`) race on that write; the same is true for the `claude.json` bootstrap, the shared `box-git/` directory, and (when `--devenv`/`--flake` are used) the dev-env cache.

The "multiple sandboxes can run simultaneously" note in `HARDENING.md` is intended for *different* boxes, each with its own state dir — that case is fine. Same-box concurrency is not a supported configuration. Low priority; fixing would mean per-launch subdirs or a lock file around state setup.

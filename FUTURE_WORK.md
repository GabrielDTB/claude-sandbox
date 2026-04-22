# Future Work

Known issues that have been considered but not yet addressed. See `HARDENING.md` for the current threat model and mitigations.

## Terminal escape / clipboard vector

The launcher's PTY interposer (`crates/claude-sandboxed/src/pty.rs`) owns the pseudo-terminal between the host and `podman run` — it intercepts ^Z for job control and restores the saved termios, SGR, and cursor on exit, but it does **not** filter the byte stream. A malicious agent can emit arbitrary escape sequences to the user's terminal:

- **OSC 52 clipboard writes** — the sandbox can stuff arbitrary content into the user's host clipboard. If the user later pastes into another terminal, they may execute attacker-controlled commands. Risk is amplified if the user's clipboard manager has parsing bugs of its own.
- **Terminal response sequences** (DA1/DA2, cursor position reports) can inject characters into the shell input buffer after the sandbox exits.
- **Screen/cursor manipulation** can hide output or forge UI.

The post-exit termios / SGR / cursor reset only addresses state that outlives the session — it doesn't flush the buffer or strip sequences that were already interpreted mid-session. Full mitigation requires an allowlisting or stripping filter in the pty pump before bytes reach the host terminal.

## Concurrent launches against the same workspace

`<state-dir>/setup-firewall.sh` is rewritten on every launcher invocation at a fixed path. Two concurrent launches sharing the same workspace (and therefore the same `.claude-sandboxed/`) race on that write; the same is true for the `claude.json` / `claude/settings.json` bootstrap, the stub `.credentials.json` (overwritten each launch with the per-launch sandbox bearer), the shared `box-git/` directory (including the host-`.git` → `box-git/` copy performed by the git-integration option), the hook-file snapshot at `<state-dir>/git-hooks-snapshot.json`, and (when `--devenv`/`--flake` are used) the dev-env cache.

The "multiple sandboxes can run simultaneously" note in `HARDENING.md` is intended for *different* workspaces, each with its own state dir — that case is fine (PID-scoped container names, independent network namespaces, per-launch minted tokens). Same-workspace concurrency is not a supported configuration. Low priority; fixing would mean per-launch subdirs or a lock file around state setup.

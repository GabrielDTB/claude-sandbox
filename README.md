# claude-sandboxed

Run Claude Code inside a hardened rootless [podman](https://podman.io/) container, with a separate auth proxy that keeps real Anthropic OAuth tokens out of the sandbox.

`claude-sandboxed` is packaged as a Nix flake. It exposes two binaries:

- **`claude-sandboxed`** — the launcher. Prepares per-workspace state, spawns the sandbox container, wires it to the auth proxy, and streams a PTY back to the user.
- **`claude-proxy`** — the auth proxy. Holds the real OAuth credentials, validates sandbox-side bearer tokens, and forwards requests to `api.anthropic.com` with the real `Authorization` header swapped in.

See `HARDENING.md` for the full threat model and mitigation checklist, and `FUTURE_WORK.md` for known-open issues.

---

## Table of contents

- [Overview](#overview)
- [Quick start](#quick-start)
- [Installing via Nix](#installing-via-nix)
- [`claude-sandboxed` CLI](#claude-sandboxed-cli)
- [User config file](#user-config-file)
- [Inherited globals](#inherited-globals)
- [Auth proxy modes](#auth-proxy-modes)
- [Running the proxy as a system service (NixOS)](#running-the-proxy-as-a-system-service-nixos)
- [`claude-proxy` admin CLI](#claude-proxy-admin-cli)
- [Dev environment injection](#dev-environment-injection)
- [Git integration](#git-integration)
- [Hook-change detection](#hook-change-detection)
- [Persistent state layout](#persistent-state-layout)
- [Resource limits](#resource-limits)
- [Tests](#tests)
- [Project layout](#project-layout)

---

## Overview

The goal is defense-in-depth isolation so a compromised or misbehaving agent cannot:

- Exfiltrate OAuth tokens (they never enter the sandbox — the proxy owns them).
- Reach the host's LAN, loopback, cloud metadata services, or tailnet (pasta with `--no-map-gw` + nftables LAN blocks).
- Persist code that the host will later execute (project mount is treated as untrusted; hooks under `hooks/`, `.githooks/`, `git-hooks/`, `.git-hooks/`, `.husky/` and root-level `.pre-commit-config.yaml` / `pre-commit-config.yaml` are snapshotted and diffed on exit).

What the agent _can_ do: outbound internet to the public DNS resolvers we inject, writes inside its `/workspace` mount, reads on anything we bind in with `--bind`. Everything else is cut off.

### Component diagram

```
              host                                             container boundaries
  ┌──────────────────────────────┐    ┌───────────────────────────────────────────┐
  │ user terminal                │    │ sandbox container (read-only, no OAuth)   │
  │        │                     │    │   /workspace (your project, rw)           │
  │        ▼                     │    │   /home/user/.claude (rw, stub creds)     │
  │ claude-sandboxed launcher    │    │   /home/user/.claude.json (rw, seeded)    │
  │   ├─ state prep              │    │                                           │
  │   ├─ image load (once)       │◀── pasta -T ────┐                              │
  │   ├─ spawn proxy (embedded)  │                 │  Authorization: Bearer <stub>│
  │   ├─ firewall + nftables     │                 ▼                              │
  │   └─ pty ↔ podman run ───────┼──▶ claude (Anthropic CLI) ───┐                 │
  │                              │                              │                 │
  └──────────────────────────────┘                              │                 │
                                                                ▼                 │
                          ┌───────────────────────────────────────────────────────┤
                          │ claude-auth-proxy container (or external host)        │
                          │   • holds real OAuth creds + refresh token            │
                          │   • validates sha256(bearer) against token store      │
                          │   • strips stub bearer, injects real Authorization    │
                          │   • forwards to https://api.anthropic.com             │
                          └───────────────────────────────────────────────────────┘
```

---

## Quick start

One-liner, from a checkout:

```sh
nix run . -- <workspace>
```

Or from the flake URL:

```sh
nix run github:<you>/claude-sandboxed -- <workspace>
```

Prerequisites:

- Linux with `podman` available (rootless is fine).
- An existing Claude credentials file at `$CLAUDE_CREDENTIALS` or `~/.claude/.credentials.json` (create it by running `claude login` once on the host, outside the sandbox).
- If you want GitHub CLI inside the sandbox, point `gh_token_file` in `~/.config/claude-sandboxed/config.toml` (or `--gh-token-file` / `CLAUDE_SANDBOX_GH_TOKEN_FILE`) at a file containing a PAT.

The first launch lazily `podman load`s the container images (sandbox, minimal, auth proxy) and marks them loaded under `$XDG_CACHE_HOME/claude-sandbox/`. Subsequent launches skip the load.

---

## Installing via Nix

### Flake outputs

```nix
{
  inputs.claude-sandboxed.url = "github:<you>/claude-sandboxed";
}
```

| Output | What it is |
| --- | --- |
| `packages.<sys>.default` | The `claude-sandboxed` launcher (with test harnesses attached). |
| `packages.<sys>.sandbox` | Same launcher without the test-harness passthru. Useful for consumers who only want the binary. |
| `packages.<sys>.proxy` | The standalone `claude-proxy` binary. |
| `packages.<sys>.test` | In-sandbox isolation test harness (`test-sandbox.sh` under a loaded image). |
| `packages.<sys>.redteam` | Red-team escape-vector test harness (`test-redteam.sh`). |
| `overlays.default` | Adds `pkgs.claude-sandboxed` and `pkgs.claude-proxy`. |
| `nixosModules.default` | Imports both `./module.nix` (proxy service) and `./sandbox-module.nix` (launcher + optional shared cgroup slice), and wires their `.package` options to the flake's packages automatically. |

### As a consumer

Add the overlay, install the launcher system-wide, and enable the proxy:

```nix
{
  nixpkgs.overlays = [ inputs.claude-sandboxed.overlays.default ];

  imports = [ inputs.claude-sandboxed.nixosModules.default ];

  programs.claude-sandboxed = {
    enable = true;
    sharedLimit = {
      enable = true;
      memoryGB = 48;      # combined RAM+swap cap across all concurrent sandboxes
    };
  };

  services.claude-proxy = {
    enable = true;
    bind = "100.64.0.1:18080";   # e.g. Tailscale address
    openFirewall = true;
  };
}
```

### Ad-hoc build

```sh
nix build .#default          # launcher
nix build .#proxy            # standalone proxy
nix run  .#test              # isolation tests
nix run  .#redteam           # red-team tests
```

---

## `claude-sandboxed` CLI

```
claude-sandboxed <workspace> [options] [-- claude-args...]
```

| Flag | Env | Description |
| --- | --- | --- |
| `<workspace>` (positional) | — | Host directory exposed at `/workspace` inside the sandbox. Required, except with `--print-default-config`. |
| `--devenv PATH` | — | Inject a [devenv](https://devenv.sh) project's environment into the sandbox. Mutually exclusive with `--flake`. |
| `--flake PATH` | — | Inject a flake's `devShell` environment into the sandbox. |
| `--state-dir PATH` | — | Where per-sandbox state lives. Default: `./.claude-sandboxed`. |
| `--bind SRC:DST` (repeatable) | — | Bind-mount `SRC` read-only at `DST` inside the sandbox. |
| `--bind-rw SRC:DST` (repeatable) | — | Bind-mount read-write. |
| `--env KEY=VALUE` (repeatable) | — | Extra env vars for the sandbox. |
| `--cpus N` | `CPU_LIMIT` | Per-container CPU cap (passed to `podman --cpus`). Default: unlimited. |
| `--memory SIZE` | `MEMORY_LIMIT` | Per-container memory cap. Also sets `--memory-swap=SIZE` so swap can't double the budget. Default: unlimited. |
| `--cgroup-parent SLICE` | `CLAUDE_SANDBOX_CGROUP_PARENT` | Place the container under this cgroup (typically a systemd user slice). Auto-discovered from `/etc/claude-sandboxed/slice` when unset; see [Resource limits](#resource-limits). |
| `--gpu` | `GPU=1` | Pass NVIDIA GPUs through via `nvidia-container-toolkit`. |
| `--anonymous` | — | Suppress identity-leaking config (GH token). |
| `--no-tools` | — | Use the minimal container image (core packages only, no dev tools). |
| `--permissive` | — | Pass `--dangerously-skip-permissions` to `claude` inside. Combined with `permissive = true` in config it also seeds `skipDangerousModePermissionPrompt: true` into a fresh sandbox's `claude/settings.json`. |
| `--auth-proxy URL` | `CLAUDE_SANDBOX_AUTH_PROXY` | Use an external proxy at `URL` instead of spawning an embedded one. Requires `--auth-token-file`. |
| `--auth-token-file PATH` | `CLAUDE_SANDBOX_AUTH_TOKEN_FILE` | File containing the sandbox bearer token for the external proxy. |
| `--gh-token-file PATH` | `CLAUDE_SANDBOX_GH_TOKEN_FILE` | File containing a GitHub PAT to expose inside the sandbox as `$GH_TOKEN`. Unset by default. Ignored with `--anonymous`. |
| `--copy-git` / `--no-copy-git` | — | Force the host `.git` copy on / off for this launch, overriding config. See [Git integration](#git-integration). |
| `--profile NAME` | — | Inherit skills from `[profiles.NAME]` in `config.toml`. See [Inherited globals](#inherited-globals). |
| `--skill-tag TAG` (repeatable) | — | Additional skill tag to inherit (prefix-at-segment-boundary match). Layered on top of `--profile`. |
| `--skill-file PATH` (repeatable) | — | Specific skill directory to inherit, relative to `$XDG_DATA_HOME/claude-sandboxed/skills/`. |
| `--print-default-config` | — | Print an annotated reference `config.toml` to stdout and exit. Pipe into `~/.config/claude-sandboxed/config.toml` to bootstrap. |
| `[-- claude-args…]` | — | Trailing arguments are passed verbatim to `claude` inside the sandbox. |

Precedence for any option that also exists in the config file: **flag > env > config file > built-in default**.

### Environment variables the launcher reads

- `CLAUDE_CREDENTIALS` — host-side OAuth creds file (embedded proxy only). Default: `~/.claude/.credentials.json`. Must exist before first launch.
- `CLAUDE_SANDBOX_GH_TOKEN_FILE` — equivalent to `--gh-token-file` / `gh_token_file` in `config.toml`: path to a file containing a GitHub PAT to expose inside the sandbox as `$GH_TOKEN`. Unset by default. Ignored with `--anonymous`.
- `PIDS_LIMIT` — overrides the container's default `--pids-limit 4096`.
- `TERM`, `COLORTERM`, `LANG` — forwarded to the container (with sensible defaults).

### Informational invocations

```sh
claude-sandboxed --print-default-config > ~/.config/claude-sandboxed/config.toml
```

Short-circuits before any filesystem or podman work, so it runs fine in environments without `$HOME` or `podman`.

---

## User config file

Location: `$XDG_CONFIG_HOME/claude-sandboxed/config.toml`, falling back to `$HOME/.config/claude-sandboxed/config.toml`. A missing file is not an error; a malformed one is. All fields are optional; unknown keys are rejected (`deny_unknown_fields`) so typos fail loudly. Precedence for any option that also exists on the CLI: **flag > env > config > built-in default**.

The authoritative, always-up-to-date schema is printed by the launcher itself. Bootstrap a fresh config with:

```sh
claude-sandboxed --print-default-config > ~/.config/claude-sandboxed/config.toml
```

Every example line in the output is commented out, so piping it into place is a no-op until you uncomment the fields you want.

Keys (see `--print-default-config` for the full annotations):

- `auth_proxy` — URL of an external auth proxy. Equivalent to `--auth-proxy` / `$CLAUDE_SANDBOX_AUTH_PROXY`.
- `auth_token_file` — path to the sandbox bearer token for `auth_proxy`. Equivalent to `--auth-token-file` / `$CLAUDE_SANDBOX_AUTH_TOKEN_FILE`.
- `default_model` — seed value for `model` in a fresh sandbox's `claude/settings.json`. First-launch only.
- `default_theme` — seed value for `theme` in a fresh sandbox's `claude.json`. First-launch only.
- `permissive` — durable default for `--permissive`; also seeds `skipDangerousModePermissionPrompt: true` on first launch.
- `copy_git_on_init` — copy host `.git` into `box-git/` on first launch (default: `true`). See [Git integration](#git-integration).
- `copy_git_on_launch` — re-copy host `.git` into `box-git/` on every launch (default: `false`).
- `cgroup_parent` — default value for `--cgroup-parent` / `$CLAUDE_SANDBOX_CGROUP_PARENT`.

Path fields (`auth_token_file`) support `~` / `~/…` expansion; other relative paths resolve against the config file's own directory (Cargo.toml-style). `~user` (other users' homes) is not supported.

### Seed vs durable fields

- `default_model`, `default_theme`: only applied on first launch of a fresh sandbox (`claude.json` / `claude/settings.json` missing or empty). Later launches keep whatever the user picked inside via `/model` or `/theme`.
- `permissive`: both a durable default for the `--permissive` flag _and_ a one-shot seed of `skipDangerousModePermissionPrompt: true` into a fresh `claude/settings.json`.
- `copy_git_on_init` / `copy_git_on_launch`: runtime flags, applied every launch (but "on init" only fires when `box-git/` is empty).

---

## Inherited globals

Skill directories can be shared across sandboxes instead of being re-derived per project. Content lives on the host under `$XDG_DATA_HOME/claude-sandboxed/skills/` (fallback `~/.local/share/claude-sandboxed/skills/`):

```
~/.local/share/claude-sandboxed/
└── skills/
    ├── languages/python/
    │   └── typing-helper/
    │       ├── SKILL.md
    │       └── examples.py
    └── cli/clap/
        └── derive-help/
            └── SKILL.md
```

A **skill** is any directory containing a `SKILL.md` file. Its **name** is the final path component of that directory (`typing-helper`, `derive-help`); that name is also the mount-target inside the sandbox (`/home/user/.claude/skills/<name>/`), so names must be unique across the selected set — a collision is a hard error.

The directory chain between `skills/` and the skill's parent becomes the skill's implicit **tag** — `skills/languages/python/typing-helper/SKILL.md` carries the tag `languages/python`. Tag matching is **prefix-at-segment-boundary**: the tag `languages` matches `languages/python` and `languages/rust`, but not `languages-extended`. The separator is `/`.

Stray `.md` files that aren't inside a `SKILL.md`-bearing directory are ignored. The walk stops descending at the first `SKILL.md` it finds, so files beneath a skill (e.g. `examples.py`, or a nested `SKILL.md`) are the skill's own assets — not separately-inherited skills.

### File-level tags via frontmatter

`SKILL.md` can declare additional tags through YAML frontmatter. A configured tag matches the skill if it prefix-at-boundary-matches *any* of the skill's chains — the dir chain OR any frontmatter entry:

```markdown
---
tags: [cli/clap, general]
description: Helper for writing clap derive structs
---

# body
```

If this is `skills/languages/python/typing-helper/SKILL.md`, the skill already carries the dir tag `languages/python`. The frontmatter adds two more: `cli/clap` and `general`. Any of `languages`, `languages/python`, `cli`, `cli/clap`, or `general` will now select it.

Details:

- Delimiters are `---` (the opening fence must be the very first line). The closing fence can be `---` or `...`, each on its own line.
- Unknown sibling fields (`description`, `model`, etc.) are ignored — coexist cleanly with Claude Code's other frontmatter conventions.
- Malformed frontmatter is a hard error: unclosed `---`, invalid YAML, empty-string tags, or tags with leading/trailing `/` all fail loudly.
- Frontmatter scan is capped at 64 KiB per `SKILL.md` — pathological inputs that open `---` and never close are rejected before they can bloat memory.

### Layered configuration

Selection is assembled from up to three layers (outermost → innermost):

1. **top-level** `[skills]` — defaults applied to every launch
2. **profile-shared** `[profiles.<name>]` — applied across every kind when the profile is selected
3. **profile-kind** `[profiles.<name>.skills]` — most specific

Every layer can set four fields:

| Field | Semantics |
| --- | --- |
| `tags` | **Override.** When present, replaces the inherited tag list entirely (even `tags = []` clears). |
| `extra_tags` | **Additive.** Always unioned with whatever was resolved above. |
| `extra_files` | **Override.** Replaces the inherited explicit-entry list. Paths are relative to `skills/` and name a `SKILL.md`-bearing directory (no absolute, no `..`). |
| `extra_extra_files` | **Additive.** Always unioned with whatever was resolved above. Yes, the `extra_extra_` is intentional — `extra_files` was already taken for the override list. |

Deepest-specified `tags`/`extra_files` win. All layers' `extra_tags` and `extra_extra_files` are concatenated. CLI flags then stack additively on top.

```toml
# Applied to every launch
[skills]
tags       = ["misc"]
extra_tags = []

# Pick with --profile python-cli
[profiles.python-cli]
tags       = ["languages/python"]   # shared across every kind
extra_tags = ["cli/clap"]           # added to the resolved tags

[profiles.python-cli.skills]
tags        = ["cli/clap"]                  # overrides profile-shared for skills
extra_files = ["misc/my-readme-style"]      # names a skill directory
```

Unknown keys at any layer are rejected (`deny_unknown_fields`). A launch selects at most one profile — no profile composition.

### Selecting per launch

| Flag | Effect |
| --- | --- |
| `--profile NAME` | Select `[profiles.NAME]` from `config.toml` as the middle + inner layers. Unknown name is a hard error before podman runs. |
| `--skill-tag TAG` | Add an ad-hoc tag (repeatable), stacked additively on top of the resolved config values. |
| `--skill-file PATH` | Add a specific skill directory (repeatable), same additive behavior as `extra_extra_files`. |

All mechanisms are deduplicated: a skill matching both a tag walk and an explicit entry mounts exactly once.

### How they reach the sandbox

Each selected skill is bind-mounted **read-only** as its own `-v` arg at `/home/user/.claude/skills/<name>/`. The parent `skills/` dir is still the sandbox's tmpfs `.claude/` tree, so the agent can still create sibling skill directories normally — only the inherited skill dirs themselves are immutable. Nothing is copied into per-sandbox state; edits to the host content dir are picked up on next launch.

Requesting any tag or skill entry when the `skills/` subdirectory does not exist is a hard error (the user asked for something concrete; silently mounting nothing would hide the misconfiguration).

---

## Auth proxy modes

The sandbox never sees a real Anthropic OAuth token. What it gets is a "stub" `.credentials.json` whose `accessToken` is a sandbox-to-proxy bearer that the proxy validates and strips before forwarding. Two ways to wire this up:

### Embedded (default)

No `--auth-proxy` flag — the launcher spawns a `claude-auth-proxy-<pid>` container for the life of the sandbox:

- Mounts the host's real creds file read-write into `/credentials.json` (the proxy writes back on OAuth refresh).
- Mints a fresh 256-bit sandbox token on every launch (hex-encoded, never written to disk).
- Passes the token via `INITIAL_TOKEN` env and runs the proxy with `--initial-token-env INITIAL_TOKEN` (ephemeral mode — that single token is the only thing accepted; no persistent token store).
- Publishes the proxy port on `127.0.0.1::<random>`, and routes into the sandbox via pasta `-T` forwarding so the sandbox can reach it as `http://127.0.0.1:18080`.
- On exit, the container is killed, logs flushed into `<state-dir>/auth-proxy.log`, and the stub creds overwritten on the next launch.

Stale `claude-auth-proxy-*` and `claude-sandbox-*` containers from crashed launches are reaped at startup before anything new spawns.

### External

`--auth-proxy URL --auth-token-file PATH` (or the same via env / config):

- The launcher skips the embedded container entirely.
- It DNS-resolves `URL` on the host (first IPv4 wins, to match the legacy behavior) and inserts an nftables `accept` rule for that `IP:port` _before_ the RFC1918/CGNAT/link-local reject block. This is how the sandbox reaches a proxy on a Tailscale address or inside your LAN despite the usual LAN-block policy.
- The contents of `--auth-token-file` are read on the host and injected into the sandbox's stub creds — this is the token your proxy's token store must recognise.
- The proxy itself runs standalone (see below), bound to a trusted interface.

Only `http` and `https` schemes are accepted. Default ports: 80 / 443 respectively.

---

## Running the proxy as a system service (NixOS)

`services.claude-proxy` runs `claude-proxy serve` as a dedicated unprivileged systemd service with aggressive hardening (`ProtectSystem=strict`, empty `CapabilityBoundingSet`, `RestrictAddressFamilies = [ AF_INET AF_INET6 ]`, `SystemCallFilter = [ @system-service ~@privileged ~@resources ]`, etc.).

Options:

| Option | Default | Description |
| --- | --- | --- |
| `services.claude-proxy.enable` | `false` | Enable the service. |
| `services.claude-proxy.package` | flake's `packages.<sys>.proxy` (via the NixOS module) | The `claude-proxy` package. |
| `services.claude-proxy.bind` | `"127.0.0.1:18080"` | Listen address. Bind to a trusted interface (Tailscale, VPN, loopback) for anything multi-host — the minted-token check is defense-in-depth, not a substitute for network scoping. |
| `services.claude-proxy.credentialsFile` | `/var/lib/claude-proxy/credentials.json` | OAuth creds file. The service rewrites this on every refresh, so it must be service-owned. |
| `services.claude-proxy.tokenStore` | `/var/lib/claude-proxy/tokens.json` | Persistent token store. Hot-reloads on mtime change — no restart after mint/revoke. |
| `services.claude-proxy.user` / `.group` | `claude-proxy` | Service identity. |
| `services.claude-proxy.openFirewall` | `false` | Open `bind`'s port in the NixOS firewall. |

The module also writes `/etc/claude-proxy/config.json` — the contract that lets the admin CLI (see next section) discover which user to drop to and which files to touch.

At boot the service starts even without creds, serving `503 Service Unavailable` wrapped in Anthropic's `authentication_error` envelope, until you run `sudo claude-proxy login`.

---

## `claude-proxy` admin CLI

All state-mutating subcommands are intended to be invoked as **root**. They self-privdrop to the configured service user after reading `/etc/claude-proxy/config.json`, so the admin doesn't need `-u claude-proxy`, `--creds`, or `--token-store`. On machines without that file (dev / standalone) the commands just run as the invoking user.

| Subcommand | Purpose |
| --- | --- |
| `serve` | Run the proxy server. Systemd invokes this directly; you generally don't run it by hand. Accepts `--token-store PATH` for persistent mode or `--initial-token-env VAR` for ephemeral (single-token, no store) mode. |
| `login` | Interactive OAuth (PKCE / out-of-band). Prints a claude.ai URL, you paste the code back, creds land in `credentialsFile`. The running `serve` picks them up on the next request via mtime reload. |
| `mint [--name LABEL]` | Mint a new sandbox token, store its hash, print the raw token to stdout **once**. |
| `list` | List token ids and labels (never the raw tokens). |
| `revoke <id>` | Revoke a token by id. `serve` picks up the change via mtime reload. |

Token changes (`mint` / `revoke`) are coordinated with the running service via an `flock` on the store file plus an mtime-gated reload — no `systemctl restart` needed.

### End-to-end external-proxy setup

On the proxy host:

```sh
sudo claude-proxy login                    # one-time OAuth bootstrap
sudo claude-proxy mint --name my-laptop    # prints the raw token; copy it
```

On the sandbox host:

```sh
mkdir -p ~/.config/claude-sandboxed
umask 077 && printf %s "$TOKEN" > ~/.config/claude-sandboxed/sandbox-token

cat > ~/.config/claude-sandboxed/config.toml <<'EOF'
auth_proxy      = "http://proxy.tailnet.ts.net:18080"
auth_token_file = "~/.config/claude-sandboxed/sandbox-token"
EOF

claude-sandboxed ./my-project
```

To rotate: `sudo claude-proxy mint --name my-laptop-v2`, update the token file, next launch picks it up; `sudo claude-proxy revoke <old-id>` when ready.

---

## Dev environment injection

`--flake PATH` / `--devenv PATH` captures an external project's dev environment and threads it into the sandbox:

- The environment is captured by diffing the target `devShell` (or devenv project) against a bare stdenv build. Only the devShell's actual contributions survive the diff, so sandbox-specific cruft (NIX_*, TEMP, SSL_CERT_FILE from the build sandbox) cancels out.
- Every store path referenced in the captured environment is bind-mounted read-only into the sandbox under its own store path.
- An entrypoint script sources the captured env and `exec`s `claude`.
- The capture result is cached under `<state-dir>/dev-env.sh` + `dev-closure-paths`, keyed by a hash of the source — re-used across launches until the source changes.

The flags are mutually exclusive.

---

## Git integration

The sandbox's `.git` is stored on the host at `<state-dir>/box-git/` and bind-mounted at `/workspace/.git` inside the container. This decouples the sandbox's git state from the host's, so an untrusted agent can't scribble on your real `.git` — but still gets a working repo to operate on.

Three modes, controlled by (in order) CLI override → config fields → built-in defaults:

| Effective mode | When it applies |
| --- | --- |
| **OnInit** (default) | Copy the host's `.git` into `box-git/` only when `box-git/` is uninitialised. Lets later launches preserve whatever the sandbox did to its own repo copy. |
| **OnLaunch** | Wipe `box-git/` and re-copy from the host on every launch. Host → sandbox only; the sandbox's mutations are discarded each run. Enabled by `--copy-git` or `copy_git_on_launch = true`. |
| **Off** | Never copy. `box-git/` stays empty. Enabled by `--no-copy-git` or `copy_git_on_init = false`. |

The copier treats a host `.git` as "real" only when it's a directory containing `HEAD` — submodule `.git` _files_ and empty placeholders are skipped with a warning. `*.lock` files are skipped to avoid picking up a concurrent host git's index lock.

---

## Hook-change detection

Before launch, the launcher snapshots a SHA-256 per hook-like file in the workspace:

- anything under a component named `hooks/`, `.githooks/`, `git-hooks/`, `.git-hooks/`, or `.husky/` at any depth
- root-level `.pre-commit-config.yaml` or `pre-commit-config.yaml`

The `.git/` and `.claude-sandboxed/` subtrees are excluded — what happens inside the repo's own `.git/` (or our state dir's `box-git/`) isn't directly host-executable. Symlinks are fingerprinted by their target text, so a file → symlink swap surfaces as a modification.

On exit, it diffs the snapshot and prints a warning for any new, modified, or removed file. In interactive sessions the warning blocks on Enter so it can't be scrolled past. The agent can still _create_ hook files — this is a detection signal, not a prevention. Treat the workspace as untrusted regardless.

---

## Persistent state layout

State lives under `<state-dir>` (default `./.claude-sandboxed`):

```
.claude-sandboxed/
├── claude/                       # bind-mounted at /home/user/.claude (rw)
│   ├── .credentials.json         # stub; rewritten each launch
│   └── settings.json             # seeded on first launch (model / skipDangerousModePermissionPrompt)
├── claude.json                   # bind-mounted at /home/user/.claude.json (rw)
│                                 # seeded on first launch (onboarding, theme, workspace trust)
├── box-git/                      # bind-mounted at /workspace/.git (rw)
├── setup-firewall.sh             # nftables + capability-drop script; rewritten each launch
├── git-hooks-snapshot.json       # fingerprints for hook-change detection
├── auth-proxy.log                # embedded proxy stderr + logs (truncated each launch)
├── dev-env.sh                    # (if --flake/--devenv) captured environment
├── dev-env.hash                  # (ditto) capture cache key
├── dev-closure-paths             # (ditto) list of store paths to bind-mount
└── dev-entrypoint.sh             # (ditto) entrypoint that sources dev-env.sh before exec claude
```

A compromised session can poison its own `box-git/`, `claude/`, or `claude.json` for future sessions — the blast radius is confined to the sandbox, but persistence _is_ a feature (iterative work needs it). To reset: `rm -rf .claude-sandboxed`.

---

## Resource limits

Per-container limits are set on every launch:

- `--pids-limit` from `$PIDS_LIMIT` (default `4096`).
- `--memory` / `--memory-swap` from `--memory` or `$MEMORY_LIMIT` (default unlimited; when set, swap is capped equal to RAM so the budget isn't silently doubled).
- `--cpus` from `--cpus` or `$CPU_LIMIT` (default unlimited).
- The embedded auth-proxy container is hardcoded to `--pids-limit 64 --memory 256m --memory-swap 256m` regardless.

For a **combined** cap across every concurrently-running sandbox, use `programs.claude-sandboxed.sharedLimit` on NixOS:

```nix
programs.claude-sandboxed.sharedLimit = {
  enable = true;
  memoryGB = 48;        # MemoryMax + MemorySwapMax=0 on the slice
  slice = "claude-sandboxed.slice";   # default; rarely overridden
};
```

The module creates a systemd **user** slice (so rootless podman can place containers into it without delegation) and writes `/etc/claude-sandboxed/slice` with the unit name. The launcher auto-discovers that file and passes `--cgroup-parent <slice>` on every launch, provided the slice is loaded. Set `--cgroup-parent` or `cgroup_parent` explicitly to override. On a multi-user machine each login session gets its own independent slice at the same limit — there is no portable way to share a cgroup across rootless users.

---

## Tests

Two shell harnesses run inside a freshly-loaded sandbox image:

- `nix run .#test` — `test-sandbox.sh`: baseline isolation (network namespace, seccomp, `/proc` masking, capability drop).
- `nix run .#redteam` — `test-redteam.sh`: escape-vector scenarios (symlink races, OSC 52 clipboard writes, `/proc/self/mounts` leak, etc.). Failures that are documented accepted-risks are `assert_warn` (visible but non-fatal); regressions on anything marked `assert_equal` fail the build.

Rust unit tests:

```sh
cargo test -p claude-sandboxed
cargo test -p claude-proxy
```

Both crates are included in `doCheck` of their respective Nix packages.

---

## Project layout

```
crates/
├── claude-sandboxed/                  # launcher binary
│   └── src/
│       ├── main.rs                    # entrypoint, config merge, ordering
│       ├── cli.rs                     # clap CLI surface
│       ├── config.rs                  # TOML config + --print-default-config
│       ├── state.rs                   # state dir layout, seed values, git copy
│       ├── proxy_embedded.rs          # spawn/teardown of the auth-proxy container
│       ├── proxy_external.rs          # URL parse + DNS + firewall carveout
│       ├── run.rs                     # builds and runs `podman run`
│       ├── firewall.rs                # nftables + capability-drop script
│       ├── pty.rs                     # PTY interposer; ^Z handling, termios restore
│       ├── devenv.rs                  # --flake / --devenv capture
│       ├── globals.rs                 # inherited skills/memory selection + per-file mounts
│       ├── hookscan.rs                # hook snapshot + verify
│       ├── images.rs                  # marker-cached `podman load`
│       ├── paths.rs                   # Nix-baked store paths via option_env!
│       ├── reap.rs                    # stale container reaper
│       ├── constants.rs               # centralised podman args (pids, mem, DNS, prefixes)
│       └── doc_drift.rs               # README-vs-code drift tests (cfg(test))
└── claude-proxy/                      # auth-proxy binary
    └── src/
        ├── main.rs                    # tokio runtime + entrypoint
        ├── cli.rs                     # clap subcommands + privdrop dispatch
        ├── server.rs                  # hyper HTTP server, forward + rewrite
        ├── login.rs                   # OAuth PKCE / OOB flow
        ├── creds.rs                   # creds file read/write + refresh
        ├── token_store.rs             # mint/list/revoke + flock + mtime reload
        ├── privdrop.rs                # root → service-user setgid/setuid
        ├── config.rs                  # /etc/claude-proxy/config.json reader
        └── constants.rs

flake.nix                              # inputs + outputs
package.nix                            # claude-sandboxed derivation + test harnesses
sandbox.nix                            # pure launcher derivation (no test passthru)
proxy.nix                              # claude-proxy derivation
container.nix                          # OCI images + seccomp profile
module.nix                             # services.claude-proxy (NixOS)
sandbox-module.nix                     # programs.claude-sandboxed (NixOS) + sharedLimit slice
test-sandbox.sh / test-redteam.sh      # shell test harnesses
test-lib.sh                            # shared assert_* helpers
HARDENING.md                           # threat model + mitigation checklist
FUTURE_WORK.md                         # known-open issues
```

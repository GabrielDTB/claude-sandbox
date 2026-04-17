# Sandbox Hardening Checklist

## Escape vectors

- [x] **Terminal injection (partial)** — Podman runs in a separate session. `tput sgr0`+`tput cnorm` on exit resets terminal attributes. Remaining risk: escape sequences (OSC 52 clipboard write, response sequences) during the session — full mitigation requires a PTY filter.
- [~] **Symlink attacks** — Not blocked. The sandbox can `symlink`/`symlinkat` inside writable mounts (`/workspace`, `/tmp`, `/home/user`). Host tools don't traverse the box automatically, so blast radius is confined to the container's own view. Accepted risk.
- [x] **FIFO / device node creation** — `mknod`/`mknodat` blocked by custom seccomp rule (device nodes are also blocked by podman's default profile; this additionally covers FIFOs).
- [~] **Config persistence** — `.claude-sandbox/` stores persistent state (Claude config, git data) outside `box/`. Subdirectories are bind-mounted individually into the container. A compromised session can poison its own `~/.claude` (hooks, MCP configs, `CLAUDE.md`) or git hooks for future sessions, but the blast radius stays inside the sandbox. Accepted tradeoff — persistence is required for the agent to do useful iterative work. To reset, delete the state directory between runs.
- [~] **Git hooks / history** — The agent's workspace (`box/`) is fully writable. Git repo data is stored in `.claude-sandbox/box-git/` and mounted at `$WORKSPACE/.git` inside the container, keeping `box/.git/` empty on the host so the outer repo can track `box/` contents as regular files. The agent can create hooks, modify history, etc. — treat `box/` as untrusted.

## Network isolation

- [x] **Host/LAN isolation** — Podman network namespace via `pasta` with `--no-map-gw` and `--map-guest-addr none`. Container has outbound internet but cannot reach host localhost, LAN, cloud metadata (169.254.169.254), or `host.containers.internal`.
- [x] **`/proc/net/*` isolated** — Network namespace means these show the container's (empty) connections.
- [~] **OAuth tokens isolated from sandbox** — Real OAuth tokens never enter the sandbox container. A separate auth proxy container holds the credentials and injects auth headers on forwarded API requests. The sandbox receives only a stub credentials file with dummy tokens. The proxy is reached via pasta `-T` forwarding into the sandbox's loopback. Note: podman also publishes the proxy's host-side port on `127.0.0.1::<random>`, so any host process running as the user can reach the proxy and use the tokens — the proxy itself has no client authentication. Tracked in `FUTURE_WORK.md`; a proxy rewrite is planned.

## Resource limits

- [x] **PID limit** — `--pids-limit 4096` (sandbox default, overridable via `PIDS_LIMIT`), `--pids-limit 64` (auth proxy, hardcoded).
- [~] **Memory limit** — `--memory 256m` hardcoded for the auth proxy. The sandbox defaults to unlimited; callers can pass `--memory` (or set `MEMORY_LIMIT`). Accepted risk — the host is trusted to survive a sandbox OOM and setting a default low enough to be safe would be too low to be useful.
- [~] **CPU limit** — Sandbox defaults to unlimited; pass `--cpus` or set `CPU_LIMIT` to override. Same rationale as memory.
- [ ] **No disk limits** — Project bind mount is unbounded. No clean way to limit without an overlay approach that complicates persistence. Accepted risk — recoverable with `rm`.

## Information leaks

- [x] **`/proc/version`, `/proc/cmdline`** — Masked empty via `--security-opt mask=`.
- [~] **`/proc/cpuinfo`, `/proc/meminfo`** — Not masked. Kernel/CPU/RAM fingerprint leaks to the sandbox. Accepted as a low-value leak.
- [~] **`/proc/mounts`** — Listed in `mask=` but the mask is defeated in practice: `/proc/mounts` is a symlink to `/proc/self/mounts`, which isn't covered. Container mount info (filesystem type, host device path) still leaks. Low risk; `test-redteam.sh` has a matching `assert_warn`.
- [x] **`/etc/resolv.conf` leak** — Podman injects host DNS config, which can expose tailnet domains and local DNS resolvers. Mitigated with `--dns 1.1.1.1 --dns 1.0.0.1 --dns 8.8.8.8` and `--dns-search .` to override with public resolvers and clear the search domain.
- [x] **`/etc/hosts` leak** — `--hosts-file none` prevents podman from copying the host's `/etc/hosts`. Podman still injects `host.containers.internal` and the container's LAN IP (inherent to pasta's network setup), but host machine name and custom entries are no longer exposed.
- [~] **Nix store path enumeration** — Closure store paths reveal dependency versions. Accepted — agent can discover versions through other means anyway.

## Already mitigated (by podman defaults + container image)

- [x] Filesystem isolation — OCI container with `--read-only` rootfs
- [x] Nix store scoped to closure
- [x] Environment cleared — only explicit `-e` flags, no host env leakage
- [x] PID namespace — podman default
- [x] IPC namespace — podman default
- [x] UTS namespace — `--hostname sandbox`
- [x] Network namespace — `--network pasta`
- [x] User namespace — podman rootless (mapped UID, no real root)
- [x] Seccomp — podman default profile blocks ptrace, mount, kexec, bpf, modules, keyring, etc. Custom profile adds only `mknod`/`mknodat` blocking.
- [x] No-new-privileges — `--security-opt no-new-privileges`
- [x] FHS layout — `buildEnv` with scoped `/bin`, `/lib`, `/etc`
- [x] `/etc` minimal — only resolv.conf (from podman), SSL certs, passwd, nsswitch.conf
- [x] No home directory — `/home/user` on tmpfs
- [x] No git identity — no `.gitconfig` exposed
- [x] Project path masked — mounted at `/workspace/<name>`
- [x] No host system paths — no `/run/current-system`, `/etc/static`, `/etc/nix`

## Notes

- **The `box/` project dir is the primary remaining attack surface.** Files with host-side execution semantics (`.envrc`, `.tool-versions`, `Makefile`, `.vscode/settings.json`, `.idea/`, `.git/hooks/`) are writable. This is inherent to the writable project mount — treat `box/` as untrusted output. The host-side workspace manages version control and config at the top level, outside the container's reach.
- **Custom seccomp is minimal.** Only `mknod`/`mknodat` blocked on top of podman's ~50+ default blocklist. If podman's defaults ever add `mknod` blocking (device nodes are already covered — this is specifically for FIFOs), the custom profile becomes unnecessary.
- **Multiple sandboxes can run simultaneously.** Each gets its own auth proxy container with a dynamically assigned host port, PID-scoped container name, and independent pasta network namespace.

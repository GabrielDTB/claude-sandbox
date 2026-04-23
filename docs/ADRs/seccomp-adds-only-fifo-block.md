---
status: accepted
date: 2026-04-22
tags: [seccomp, container, minimal]
---

# Custom seccomp only adds mknod/mknodat blocking

## Context

Podman's default seccomp profile already blocks ~50+ dangerous
syscalls: `ptrace`, `mount`, `unshare`, `kexec_load`, `bpf`, module
loading, keyring operations, and so on. Device node creation is
separately blocked via `CAP_MKNOD` drop. However, `mknod`/`mknodat`
called with `S_IFIFO` (FIFO creation) is not covered by the cap drop
and is not in podman's default seccomp blocklist — the agent can
create FIFOs inside writable mounts.

A FIFO in `/workspace` or `/tmp` is a host-visible entity (via the
workspace bind-mount) that can be used as a coordination primitive
for escapes we haven't enumerated. Blocking it is cheap defense.

## Decision

Apply a custom seccomp profile (`container.nix` / `seccompProfile`)
that inherits the default `SCMP_ACT_ALLOW` and adds a single
blocklist: `mknod`, `mknodat`, both returning `EPERM` (errno 1).
No other additions on top of podman's defaults.

## Alternatives considered

- **Aggressive custom seccomp with a tight allowlist**: rejected.
  Fragile against shell tooling, breaks coreutils in non-obvious
  ways, drifts badly against evolving podman defaults (syscalls
  added to Linux need manual reconsideration on every upgrade).
  Maintenance cost dwarfs the marginal isolation.
- **Only rely on CAP_MKNOD drop**: rejected. Cap drop covers device
  nodes (which also need `CAP_MKNOD` anyway), but the syscall itself
  is what needs blocking to catch FIFOs.
- **No custom seccomp**: rejected. Leaves the FIFO escape vector
  wide open with no real downside to closing it.

## Consequences

- Custom profile is tiny — three lines of syscall names. Easy to
  audit, easy to justify to a reviewer, easy to diff against podman
  default behavior.
- If podman's default profile ever adds `mknod`/`mknodat` (they're
  plausible candidates), our custom profile becomes a no-op. Harmless
  in that case, but we should notice and drop it so the intent of
  "minimal delta from podman default" stays visible. HARDENING.md
  calls this out so the maintainer sees it.
- The `test-redteam.sh` harness asserts FIFO creation fails; this is
  the drift test for this decision.

## Related

- [[workspace-is-untrusted]] (FIFO would be a host-visible artifact
  the agent could author)

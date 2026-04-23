---
status: accepted
date: 2026-04-22
tags: [git, workspace, isolation]
refinedBy: [[box-git-tri-mode]]
---

# The sandbox's .git is decoupled from the host's

## Context

The sandbox needs a functional git repository for the agent to operate
on (log, diff, branch, commit, etc. are core to what the agent does).
The host project's `.git` is visible at the workspace mount point, so
the naive implementation is a straight bind-mount of the host's `.git`
into the container.

Two problems follow from that naive implementation, in tension with
[[workspace-is-untrusted]]:

1. The agent inside the container can write to the host's real
   `.git`, including hooks (`.git/hooks/*`), packed-refs, or worse.
   The next time the host runs a git operation, that code runs
   host-side.
2. The host running a concurrent git operation on the same repo races
   on index locks, packfile writes, and config rewrites.

## Decision

The sandbox's `.git` is stored on the host at
`<state-dir>/box-git/` and bind-mounted at `/workspace/.git` inside the
container — *not* as a pass-through of the host's `.git`. The host's
real `.git` is never writable from inside the sandbox.

## Alternatives considered

- **Bind-mount host .git read-only**: rejected. Git needs to write
  (index, HEAD, refs); read-only breaks the workflow entirely.
- **Bind-mount host .git read-write**: rejected. Directly violates
  [[workspace-is-untrusted]] and races on host locks.
- **No .git at all in the sandbox**: rejected. The agent genuinely
  needs git for its core workflow.
- **Ephemeral in-container .git (no host persistence)**: rejected as a
  base design but enabled as an option via [[box-git-tri-mode]].

## Consequences

- A separate policy question appears: *how* should `box-git/` be
  populated from the host's `.git`? That is refined by
  [[box-git-tri-mode]].
- Sandbox mutations never flow back to the host automatically; the
  user is responsible for push/PR flows from inside the sandbox.
- Submodule `.git` *files* (not directories) and empty placeholders
  require care during copy — see implementation for the "real repo"
  heuristic (`HEAD` present).
- `.lock` files are skipped during copy so a concurrent host git's
  transient index lock cannot contribute to the sandbox's repo.

## Related

- [[workspace-is-untrusted]]
- [[box-git-tri-mode]] (refines this by picking *when* the copy happens)

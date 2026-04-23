---
status: accepted
date: 2026-04-22
tags: [git, workspace, ux]
refines: [[box-git-decoupled-from-host]]
---

# box-git supports three population modes: OnInit, OnLaunch, Off

## Context

Given [[box-git-decoupled-from-host]], the sandbox has its own `.git`
at `<state-dir>/box-git/`. The remaining question is when the launcher
should copy the host's `.git` into that directory.

Two user stories tension each other:

1. **"Start from the real repo, let me iterate"** — the default case.
   First launch seeds the sandbox with the host's git state; subsequent
   launches preserve whatever the agent did to its copy (branches,
   commits, stash entries).
2. **"Keep it pristine, wipe every time"** — short-lived sessions where
   the agent's mutations should be discarded and the host's current
   state is the source of truth every run.

There's also a third case: **"Never copy"** — for users who want to
hand-manage the sandbox's git state or run without a repo at all.

## Decision

Three modes, resolved in the order CLI flags → config file → default:

- **OnInit** (default): populate `box-git/` from the host `.git` only
  when `box-git/` is uninitialized. Preserves sandbox mutations on
  subsequent launches.
- **OnLaunch**: wipe and re-copy every launch. Host wins each time.
  Enabled by `--copy-git` or `copy_git_on_launch = true`.
- **Off**: never copy. `box-git/` stays empty until the agent
  initializes it. Enabled by `--no-copy-git` or `copy_git_on_init = false`.

## Alternatives considered

- **Single mode (always OnInit)**: rejected as insufficient. Short-lived
  session users have legitimate reason to want wipe semantics.
- **Always wipe-and-recopy (only OnLaunch)**: rejected. Destroys the
  iterative workflow's value; agent commits vanish between runs.
- **Merge host and sandbox state on launch**: rejected. Three-way
  merging a .git directory is a fractal of edge cases (concurrent
  commits, diverged refs) and git already has a model for this —
  `git push`/`git pull` — that the agent can run inside the sandbox.
- **Prompt the user at each launch**: rejected. Breaks non-interactive
  workflows and fatigues interactive ones.

## Consequences

- Default flow is sensible for most users: first launch seeds, later
  launches resume.
- The copier treats a `.git` as "real" only if it's a directory
  containing `HEAD`. Submodule `.git` files and empty placeholders are
  skipped with a warning.
- Precedence is load-bearing; the combination of
  `copy_git_on_init` / `copy_git_on_launch` / `--copy-git` / `--no-copy-git`
  must compose coherently. Tested in `state.rs::tests`.
- When `OnLaunch` is set, `copy_git_on_init` is implied — you can't
  wipe-and-recopy without also seeding.

## Related

- [[box-git-decoupled-from-host]] (this refines it)
- [[workspace-is-untrusted]]
- [[persist-state-across-sessions]] (OnInit persists, OnLaunch doesn't)

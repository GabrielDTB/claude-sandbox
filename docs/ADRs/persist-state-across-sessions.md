---
status: accepted
date: 2026-04-22
tags: [threat-model, ux, tradeoff]
---

# Sandbox state persists across sessions

## Context

The sandbox has persistent state under `<state-dir>` (default
`./.claude-sandboxed`): `box-git/` (the sandbox's git history),
`claude/` (stub credentials, settings, MCP configs, `CLAUDE.md`), and
`claude.json` (onboarding state, theme, workspace trust). A compromised
agent in session N can write poisoned content to any of this —
settings that enable dangerous modes, malicious MCP server configs,
misleading `CLAUDE.md` instructions, git hooks in `box-git/hooks/`.

This is in direct tension with [[workspace-is-untrusted]]: we are
accepting that the agent can influence its own future self.

## Decision

State persists across sessions. The blast radius is bounded to the
sandbox — poisoned state cannot reach the host's filesystem, tools,
or credentials — and the usability benefit of iterative work outweighs
the within-sandbox-self-poisoning risk. Users who want a clean slate
are documented the reset path: `rm -rf .claude-sandboxed`.

## Alternatives considered

- **Wipe state every launch**: rejected. Destroys the core iterative
  workflow; agent can't resume a task, every session starts cold,
  login / onboarding repeats forever.
- **Snapshot + diff state on exit, prompt user**: rejected. User
  fatigue for state the user has no mental model of, and the state is
  sandbox-internal — the user can't meaningfully evaluate a diff of
  claude.json.
- **Persist only git, wipe the rest**: rejected. claude.json's
  onboarding state is nontrivial to re-seed, theme/model choices
  matter for UX, and stub credentials are rewritten every launch
  anyway (so their "persistence" isn't meaningful).
- **Persist under a cryptographic integrity wrapper**: rejected as
  overengineering. The threat is self-poisoning, not external
  tampering; a signature wouldn't help because the agent holds the
  writing end.

## Consequences

- Explicit tension with [[workspace-is-untrusted]]. We document this
  as an accepted-risk tradeoff; the HARDENING.md checklist marks it
  `[~]`.
- OAuth exposure specifically does *not* persist — stub credentials
  are rewritten every launch, and the real credentials were never in
  the sandbox to begin with (see [[auth-proxy-out-of-sandbox]]).
- The sandbox's agent-written hook files are covered by
  [[hook-change-detection-not-prevention]] as a detection layer —
  persistence does not imply "user will never notice."
- Reset is a single command; we accept the fact that some users will
  never run it.

## Related

- [[workspace-is-untrusted]] (explicit tension — we accept this risk)
- [[box-git-tri-mode]] (OnInit mode depends on this decision; OnLaunch
  partially undoes it for git specifically)
- [[auth-proxy-out-of-sandbox]] (credentials specifically *don't* live
  in persistent state)

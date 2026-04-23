---
status: accepted
date: 2026-04-22
tags: [threat-model, foundational]
---

# Workspace is untrusted

## Context

The sandbox exists to run an AI coding agent with filesystem write access
to a project directory. The agent is inherently adversarial for threat-model
purposes — not because we assume it's malicious, but because (a) it can
be manipulated via prompt injection from untrusted inputs, and (b) the
whole point of defense-in-depth is to survive the assumption breaking.

Files the agent writes to `/workspace` can have host-side execution
semantics: git hooks, `.envrc`, `.vscode/settings.json`, `.idea/`,
`Makefile`, `.pre-commit-config.yaml`, `.tool-versions`, husky configs.
If the host later runs any of that tooling against the directory, it
executes agent-authored code with the host user's privileges.

## Decision

Treat every byte under `/workspace` — including everything the agent
writes back — as potentially adversarial. The host must never assume
that opening, building, or executing anything inside the workspace is
safe without a prior inspection step.

## Alternatives considered

- **Read-only workspace**: rejected. Iterative development requires
  writes; the agent producing code is the core use case.
- **Allowlist of filenames the agent can write**: rejected as infeasible;
  the agent's job is partly to decide what files exist.
- **Trust the agent**: this is the status quo without the sandbox. The
  sandbox exists specifically to reject this position.

## Consequences

- Any subsystem producing host-reachable artifacts needs a mitigation
  layer. See [[box-git-decoupled-from-host]],
  [[hook-change-detection-not-prevention]].
- Anything persisted across sessions has to be blast-radius-bounded to
  the sandbox, because the agent can poison its own future self. See
  [[persist-state-across-sessions]] (explicit tension).
- Documentation and UX must repeatedly remind users not to run host
  tooling inside the workspace without inspection.

## Related

- [[box-git-decoupled-from-host]]
- [[persist-state-across-sessions]] (tension)
- [[hook-change-detection-not-prevention]]

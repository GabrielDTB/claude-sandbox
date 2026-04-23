---
status: accepted
date: 2026-04-22
tags: [auth, architecture, credentials]
---

# OAuth credentials never enter the sandbox

## Context

Claude Code inside the container makes HTTPS requests to
`api.anthropic.com`. Those requests need a bearer token minted from the
user's Anthropic OAuth credentials. Possession of those credentials lets
the holder impersonate the user against Anthropic's service for as long
as the refresh token remains valid — which is a long time.

If the sandbox is compromised (prompt injection producing filesystem
writes, a bug in Claude Code, a future escape we haven't thought of),
and the credentials are reachable inside the container, the blast radius
expands from "agent misbehaves within its workspace" to "attacker
impersonates the user against Anthropic indefinitely."

## Decision

The real OAuth credentials never enter the sandbox container. A separate
process (`claude-proxy`) holds them on the host. The sandbox receives
only a stub `.credentials.json` whose `accessToken` is a sandbox-to-proxy
bearer. The proxy validates `sha256(bearer)` against its token store,
strips the stub bearer, injects the real `Authorization` header, and
forwards to `api.anthropic.com`.

## Alternatives considered

- **Bind-mount credentials file read-only**: rejected. A compromised
  agent can `cat` the file and exfiltrate.
- **Credential-helper binary inside the container**: rejected. Any
  helper that can produce a real token can be invoked adversarially;
  the helper itself becomes the credential.
- **Pass credentials via environment variable**: rejected. Env vars are
  visible via `/proc/self/environ` and propagate to every child process.
- **Trust the container isolation alone**: rejected. The isolation is
  defense-in-depth; credentials are the highest-value asset and deserve
  an additional boundary.

## Consequences

- Two runtime shapes follow: embedded proxy (convenience, per-sandbox)
  and external proxy (shared, multi-host). See
  [[embedded-vs-external-proxy]].
- The sandbox-to-proxy transport is itself a trust boundary. The token
  model is load-bearing; see [[per-launch-bearer-embedded]].
- OAuth refresh happens on the proxy side and is invisible to the
  sandbox — which also means the sandbox cannot observe token lifetime.

## Related

- [[embedded-vs-external-proxy]]
- [[per-launch-bearer-embedded]]
- [[network-namespace-via-pasta]] (the proxy is reached over this)

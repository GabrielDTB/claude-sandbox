---
status: accepted
date: 2026-04-22
tags: [network, isolation, podman]
---

# Network isolation via pasta with --no-map-gw --map-guest-addr none

## Context

The sandbox needs outbound internet to reach the auth proxy (which
reaches Anthropic) but must *not* reach:

- Host loopback (other services on the host machine)
- LAN addresses (RFC1918, CGNAT, link-local)
- Cloud metadata services (169.254.169.254)
- Tailnet / VPN addresses (100.64.0.0/10, 10.x typical)
- `host.containers.internal` (podman's host-side shim)

Rootless podman offers two network backends with meaningful isolation
semantics: `slirp4netns` (older, stable) and `pasta` (newer, more
principled defaults).

## Decision

Use `--network pasta:--no-map-gw,--map-guest-addr,none` for both the
sandbox container and the embedded auth-proxy container. In the
embedded-proxy case we additionally pass `-T,<port>` for proxy loopback
forwarding.

`--no-map-gw` prevents the container from using the host as its default
gateway (blocking "talk to the host as a router" attacks). `--map-guest-addr
none` prevents the container's guest IP from being spoofable onto
host-reachable interfaces.

## Alternatives considered

- **slirp4netns with --disable-host-loopback**: rejected. Blocks host
  loopback but does not cleanly block LAN. Pasta's isolation model is
  stricter by default.
- **Bridge network with manual firewall rules**: rejected. Bridging
  requires `CAP_NET_ADMIN` (not rootless-friendly) and moves per-container
  route management into our code.
- **No network isolation, trust seccomp and process isolation alone**:
  rejected. The auth proxy has to be reachable from the sandbox, so
  *some* network namespace exists regardless; the only question is
  whether it's scoped correctly.

## Consequences

- Reaching the auth proxy:
  - Embedded mode: pasta `-T` forwards the proxy port into the sandbox
    as loopback. The container sees the proxy at `127.0.0.1:<port>`.
  - External mode: the launcher inserts an nftables `accept` rule for
    the resolved proxy `IP:port` *before* the default RFC1918/CGNAT
    reject block, so the proxy (on e.g. a Tailscale address) is
    reachable despite the policy.
- `/etc/resolv.conf` would leak host DNS; we override with
  `--dns 1.1.1.1 --dns 1.0.0.1 --dns 8.8.8.8` and clear the search
  domain with `--dns-search .`.
- Inherent leak: pasta still injects `host.containers.internal` into
  `/etc/hosts`. `--hosts-file none` prevents host-authored entries but
  not pasta's own; accepted as a low-signal leak of "there is a host."

## Related

- [[auth-proxy-out-of-sandbox]]
- [[embedded-vs-external-proxy]] (implements both mode's network wiring)

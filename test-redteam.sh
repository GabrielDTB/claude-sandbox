#!/usr/bin/env bash
# Red team tests. Runs inside the podman container.
# Tests for holes in the sandbox that an adversarial agent could exploit.
set -uo pipefail
source "${TEST_LIB:-$(dirname "$0")/test-lib.sh}"

echo "=== Host-executed config files ==="

assert_warn "can write .envrc (direnv)" \
  bash -c 'echo "echo PWNED" > .envrc'

assert_warn "can write .tool-versions (asdf/mise)" \
  bash -c 'echo "nodejs 0.0.1" > .tool-versions'

assert_warn "can write Makefile" \
  bash -c 'echo "all:" > Makefile'

assert_warn "can write .vscode/settings.json" \
  bash -c 'mkdir -p .vscode && echo "{}" > .vscode/settings.json'

assert_warn "can write .idea/ config" \
  bash -c 'mkdir -p .idea && echo "<xml/>" > .idea/workspace.xml'

rm -f .envrc .tool-versions Makefile 2>/dev/null
rm -rf .vscode .idea 2>/dev/null

echo ""
echo "=== /proc information leaks ==="

PROC_NET_LINES=$(wc -l < /proc/net/tcp 2>/dev/null || echo 999)
assert "/proc/net/tcp shows only header (isolated namespace)" \
  test "$PROC_NET_LINES" -le 2

PROC_NET_UDP_LINES=$(wc -l < /proc/net/udp 2>/dev/null || echo 999)
assert "/proc/net/udp shows only header (isolated namespace)" \
  test "$PROC_NET_UDP_LINES" -le 2

assert_warn "/proc/mounts leaks host info" \
  bash -c 'grep -q "/dev/" /proc/mounts 2>/dev/null'

assert "/proc/version is masked (empty)" \
  bash -c 'test ! -s /proc/version'

assert "/proc/cmdline is masked (empty)" \
  bash -c 'test ! -s /proc/cmdline'

echo ""
echo "=== /proc escape attempts ==="

assert_fails "cannot write to /proc/sys" \
  bash -c 'echo 1 > /proc/sys/net/ipv4/ip_forward'

assert_fails "cannot see host filesystem via /proc/self/root" \
  bash -c 'test -d /proc/self/root/run/current-system'

FD_COUNT=$(ls /proc/self/fd 2>/dev/null | wc -l)
echo "  INFO: open file descriptors: $FD_COUNT"
assert "no excessive fd leaks (< 20 open fds)" \
  test "$FD_COUNT" -lt 20

echo ""
echo "=== Network: LAN isolation ==="

assert "public internet is reachable (by design)" \
  bash -c 'timeout 3 bash -c "echo > /dev/tcp/1.1.1.1/53" 2>/dev/null'

assert_fails "cannot reach RFC1918 10.0.0.0/8" \
  bash -c 'timeout 2 bash -c "echo > /dev/tcp/10.0.0.1/80" 2>/dev/null'

assert_fails "cannot reach RFC1918 172.16.0.0/12" \
  bash -c 'timeout 2 bash -c "echo > /dev/tcp/172.16.0.1/80" 2>/dev/null'

assert_fails "cannot reach RFC1918 192.168.0.0/16" \
  bash -c 'timeout 2 bash -c "echo > /dev/tcp/192.168.1.1/80" 2>/dev/null'

assert_fails "cannot reach CGNAT/Tailscale 100.64.0.0/10" \
  bash -c 'timeout 2 bash -c "echo > /dev/tcp/100.100.100.100/80" 2>/dev/null'

assert_fails "cannot reach cloud metadata (169.254.169.254)" \
  bash -c 'timeout 2 bash -c "echo > /dev/tcp/169.254.169.254/80" 2>/dev/null'

assert_fails "cannot reach host via host.containers.internal (169.254.1.2)" \
  bash -c 'timeout 2 bash -c "echo > /dev/tcp/169.254.1.2/22" 2>/dev/null'

assert_fails "cannot reach localhost services" \
  bash -c 'timeout 2 bash -c "echo > /dev/tcp/127.0.0.1/22" 2>/dev/null'

echo ""
echo "=== Network: firewall evasion ==="

assert_fails "cannot flush nftables ruleset" \
  bash -c 'nft flush ruleset 2>&1'

assert_fails "cannot delete nftables table" \
  bash -c 'nft delete table inet sandbox 2>&1'

assert_fails "cannot add permissive nftables rule" \
  bash -c 'nft add rule inet sandbox output accept 2>&1'

assert_fails "cannot insert rule before block rules" \
  bash -c 'nft insert rule inet sandbox output ip daddr 10.0.0.0/8 accept 2>&1'

assert_fails "cannot re-acquire NET_ADMIN (no-new-privileges)" \
  bash -c 'capsh --has-p=cap_net_admin 2>&1'

echo ""
echo "=== Filesystem attack surface ==="

assert_fails "cannot hardlink to /proc/self/exe" \
  ln /proc/self/exe hardlink-test

assert_fails "cannot create FIFOs" \
  bash -c 'mkfifo fifo-test'

assert_fails "cannot create device nodes" \
  bash -c 'mknod devnode-test c 1 3'

assert "no real root privileges (can't chown to other uid)" \
  bash -c '! touch /tmp/root-test || ! chown 1234:1234 /tmp/root-test 2>/dev/null; rm -f /tmp/root-test'

rm -f hardlink-test 2>/dev/null

echo ""
echo "=== Credential exposure ==="

if [ -f /home/user/.claude/.credentials.json ]; then
  assert_warn "can copy credentials to writable location" \
    bash -c 'cp /home/user/.claude/.credentials.json /tmp/stolen-creds'
  rm -f /tmp/stolen-creds

  CRED_CONTENT=$(cat /home/user/.claude/.credentials.json 2>/dev/null)
  if [ -n "$CRED_CONTENT" ]; then
    echo "  WARN: credentials readable (exfil limited by network isolation)"
    WARN=$((WARN + 1))
  fi
else
  assert "no credentials file in container (auth proxy handles auth)" true
fi

echo ""
echo "=== Resource limits ==="

PIDS_MAX=$(cat /sys/fs/cgroup/pids.max 2>/dev/null || cat /sys/fs/cgroup/pids/pids.max 2>/dev/null || echo "unknown")
echo "  INFO: pids.max = $PIDS_MAX"

MEM_MAX=$(cat /sys/fs/cgroup/memory.max 2>/dev/null || cat /sys/fs/cgroup/memory/memory.limit_in_bytes 2>/dev/null || echo "unknown")
echo "  INFO: memory.max = $MEM_MAX"

assert "PID limit is set" \
  test "$PIDS_MAX" != "max" -a "$PIDS_MAX" != "unknown"

assert "memory limit is set" \
  test "$MEM_MAX" != "max" -a "$MEM_MAX" != "unknown"

echo ""
echo "=== Seccomp filter ==="

assert_fails "cannot create device nodes (mknod)" \
  bash -c 'mknod /tmp/devnode-test c 1 3'

assert_fails "cannot call mount" \
  bash -c 'mount -t tmpfs none /tmp 2>&1'

assert "normal file operations still work" \
  bash -c 'echo test > /tmp/seccomp-test && cat /tmp/seccomp-test && rm /tmp/seccomp-test'

echo ""
echo "=== Results ==="
echo "  $PASS passed, $FAIL failed, $WARN known open issues"

if [ "$FAIL" -gt 0 ]; then
  exit 1
fi

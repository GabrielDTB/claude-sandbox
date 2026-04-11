#!/usr/bin/env bash
# Sandbox isolation tests. Runs inside the podman container.
set -uo pipefail
source "${TEST_LIB:-$(dirname "$0")/test-lib.sh}"

echo "=== Filesystem isolation ==="

assert "workspace dir exists" \
  test -d /workspace

assert "claude config dir exists at ~/.claude" \
  test -d /home/user/.claude

assert "FHS /bin exists" \
  test -d /bin

assert "FHS /usr/bin exists" \
  test -d /usr/bin

assert "FHS /usr/lib exists" \
  test -d /usr/lib

assert "FHS /sbin exists" \
  test -d /sbin

assert "FHS /etc exists" \
  test -d /etc

assert "PATH is FHS-only (no /nix/store)" \
  bash -c '! echo "$PATH" | grep -q /nix/store'

assert "/usr/bin/env exists (shebang support)" \
  test -x /usr/bin/env

assert "/etc/resolv.conf is readable" \
  test -r /etc/resolv.conf

assert "/etc/ssl is readable" \
  test -d /etc/ssl

assert "/etc/passwd exists" \
  test -r /etc/passwd

assert "/home/user exists" \
  test -d /home/user

echo ""
echo "=== Nix store isolation ==="

STORE_COUNT=$(ls /nix/store | wc -l)
assert "nix store has < 300 entries (closure only)" \
  test "$STORE_COUNT" -lt 300

echo "  (store has $STORE_COUNT entries)"

echo ""
echo "=== Environment isolation ==="

ENV_OUTPUT=$(env)

assert_eq "HOME is /home/user" "/home/user" "$HOME"
assert_eq "USER is user" "user" "$USER"
assert_eq "TMPDIR is /tmp" "/tmp" "$TMPDIR"

assert_not_contains "no DISPLAY in env" "^DISPLAY=" "$ENV_OUTPUT"
assert_not_contains "no SSH_AUTH_SOCK in env" "^SSH_AUTH_SOCK=" "$ENV_OUTPUT"
assert_not_contains "no DBUS in env" "^DBUS_SESSION_BUS_ADDRESS=" "$ENV_OUTPUT"
assert_not_contains "no XDG vars in env" "^XDG_" "$ENV_OUTPUT"
assert_not_contains "no AWS vars in env" "^AWS_" "$ENV_OUTPUT"

assert_not_contains "PATH has no /home/" "^PATH=.*/home/" "$ENV_OUTPUT"

echo ""
echo "=== Namespace isolation ==="

assert_eq "hostname is sandbox" "sandbox" "$(hostname)"

assert "PID namespace is isolated (few processes)" \
  test "$(ls /proc | grep -c '^[0-9]')" -lt 20

echo ""
echo "=== Write isolation ==="

assert "can write to project dir" \
  bash -c 'touch "$PWD/test-write-file"'

assert_fails "cannot write to /etc" \
  bash -c 'touch /etc/test-write'

assert_fails "cannot write to /nix/store" \
  bash -c 'touch /nix/store/test-write'

rm -f "$PWD/test-write-file"

echo ""
echo "=== Credentials ==="

if [ -f /home/user/.claude/.credentials.json ]; then
  assert "credentials file exists" true
  assert_fails "credentials file is read-only" \
    bash -c 'echo "test" >> /home/user/.claude/.credentials.json'
else
  echo "  SKIP: no credentials file bind-mounted"
fi

echo ""
echo "=== Seccomp filter ==="

assert_fails "cannot create device nodes (mknod)" \
  bash -c 'mknod /tmp/devnode-test c 1 3'

assert_fails "cannot call mount" \
  bash -c 'mount -t tmpfs none /tmp 2>&1'

assert "normal file operations still work" \
  bash -c 'echo test > /tmp/seccomp-test && cat /tmp/seccomp-test && rm /tmp/seccomp-test'

assert "process spawning still works" \
  bash -c 'echo hello | grep hello'

echo ""
echo "=== Network isolation ==="

assert_fails "cannot reach RFC1918 10.0.0.0/8" \
  bash -c 'timeout 2 bash -c "echo > /dev/tcp/10.0.0.1/80" 2>/dev/null'

assert_fails "cannot reach RFC1918 172.16.0.0/12" \
  bash -c 'timeout 2 bash -c "echo > /dev/tcp/172.16.0.1/80" 2>/dev/null'

assert_fails "cannot reach RFC1918 192.168.0.0/16" \
  bash -c 'timeout 2 bash -c "echo > /dev/tcp/192.168.1.1/80" 2>/dev/null'

assert_fails "cannot reach CGNAT/Tailscale 100.64.0.0/10" \
  bash -c 'timeout 2 bash -c "echo > /dev/tcp/100.100.100.100/80" 2>/dev/null'

assert_fails "cannot reach link-local 169.254.0.0/16" \
  bash -c 'timeout 2 bash -c "echo > /dev/tcp/169.254.169.254/80" 2>/dev/null'

echo ""
echo "=== Firewall persistence ==="

assert_fails "cannot modify nftables rules (caps dropped)" \
  bash -c 'nft add rule inet sandbox output accept 2>&1'

assert_fails "cannot flush nftables rules (caps dropped)" \
  bash -c 'nft flush ruleset 2>&1'

echo ""
echo "=== Resource limits ==="

ULIMIT_NPROC=$(ulimit -u 2>/dev/null || echo "unknown")
echo "  INFO: ulimit -u = $ULIMIT_NPROC"

echo ""
echo "=== Results ==="
echo "  $PASS passed, $FAIL failed"

if [ "$FAIL" -gt 0 ]; then
  exit 1
fi

#!/usr/bin/env bash
# Shared test assertion helpers.

PASS=0
FAIL=0
WARN=0

assert() {
  local name="$1"
  shift
  if "$@" >/dev/null 2>&1; then
    echo "  PASS: $name"
    PASS=$((PASS + 1))
  else
    echo "  FAIL: $name"
    FAIL=$((FAIL + 1))
  fi
}

assert_fails() {
  local name="$1"
  shift
  if ! "$@" >/dev/null 2>&1; then
    echo "  PASS: $name"
    PASS=$((PASS + 1))
  else
    echo "  FAIL: $name"
    FAIL=$((FAIL + 1))
  fi
}

assert_eq() {
  local name="$1"
  local expected="$2"
  local actual="$3"
  if [ "$expected" = "$actual" ]; then
    echo "  PASS: $name"
    PASS=$((PASS + 1))
  else
    echo "  FAIL: $name (expected '$expected', got '$actual')"
    FAIL=$((FAIL + 1))
  fi
}

assert_not_contains() {
  local name="$1"
  local needle="$2"
  local haystack="$3"
  if ! echo "$haystack" | grep -q "$needle"; then
    echo "  PASS: $name"
    PASS=$((PASS + 1))
  else
    echo "  FAIL: $name (found '$needle')"
    FAIL=$((FAIL + 1))
  fi
}

# WARN means the test found a known-open hole (documented in HARDENING.md)
assert_warn() {
  local name="$1"
  shift
  if "$@" >/dev/null 2>&1; then
    echo "  WARN: $name (known open issue)"
    WARN=$((WARN + 1))
  else
    echo "  PASS: $name (mitigated)"
    PASS=$((PASS + 1))
  fi
}

//! Generate the container-side `setup-firewall.sh`.
//!
//! The script runs inside the sandbox container as the entrypoint wrapper:
//! it installs nftables rules that reject LAN ranges (defense-in-depth
//! against SSRF to the host's internal services), then drops CAP_NET_ADMIN
//! + CAP_SETPCAP via an inline Python `ctypes` block — by this point the
//!   admin caps have been used, so we drop them before exec'ing claude.
//!
//! The nftables rules and the cap-drop Python block below are load-bearing
//! for the sandbox's threat model (see `HARDENING.md` — network isolation
//! and the `NET_ADMIN` / `SETPCAP` drop). Any change here is a behavioral
//! change; `test-redteam.sh` exercises both.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// Write the firewall script to `path` with mode `0755`.
///
/// If `carveout` is `Some(rule)`, the nft rule is inserted between `oif lo`
/// accept and the LAN rejects — needed for external-proxy mode when the
/// proxy lives on a Tailscale / RFC1918 address that would otherwise be
/// caught by the reject rules.
pub fn write_script(path: &Path, carveout: Option<&str>) -> Result<(), crate::Error> {
    let mut f = fs::File::create(path)?;
    f.write_all(SHEBANG_AND_HEAD.as_bytes())?;
    if let Some(rule) = carveout {
        f.write_all(rule.as_bytes())?;
        f.write_all(b"\n")?;
    }
    f.write_all(REJECTS.as_bytes())?;
    f.write_all(CAP_DROP_PY.as_bytes())?;
    f.flush()?;
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms)?;
    Ok(())
}

/// Lines 552-557 of package.nix.
const SHEBANG_AND_HEAD: &str = "\
#!/bin/bash
set -e
nft add table inet sandbox
nft add chain inet sandbox output '{ type filter hook output priority 0; }'
nft add rule inet sandbox output oif lo accept
";

/// Lines 559-566 of package.nix — LAN reject rules + final accept.
const REJECTS: &str = "\
nft add rule inet sandbox output ip daddr 10.0.0.0/8 reject
nft add rule inet sandbox output ip daddr 172.16.0.0/12 reject
nft add rule inet sandbox output ip daddr 192.168.0.0/16 reject
nft add rule inet sandbox output ip daddr 100.64.0.0/10 reject
nft add rule inet sandbox output ip daddr 169.254.0.0/16 reject
nft add rule inet sandbox output ip6 daddr fc00::/7 reject
nft add rule inet sandbox output ip6 daddr fe80::/10 reject
nft add rule inet sandbox output accept
";

/// Lines 569-591 of package.nix — runs inside the container, where
/// `python3` lives in `builtinTools`. Verbatim, single-quoted Python
/// program; do NOT interpolate into this.
const CAP_DROP_PY: &str = r#"python3 -c '
import ctypes, os, sys
libc = ctypes.CDLL(None)
# Drop CAP_NET_ADMIN(12) and CAP_SETPCAP(8) from bounding set
for cap in (12, 8):
    libc.prctl(24, cap)  # PR_CAPBSET_DROP
# Clear all ambient caps
libc.prctl(47, 4, 0, 0, 0)  # PR_CAP_AMBIENT, PR_CAP_AMBIENT_CLEAR_ALL
# Drop from effective/permitted/inheritable via capset syscall
class CapHeader(ctypes.Structure):
    _fields_ = [("version", ctypes.c_uint32), ("pid", ctypes.c_int)]
class CapData(ctypes.Structure):
    _fields_ = [("effective", ctypes.c_uint32), ("permitted", ctypes.c_uint32), ("inheritable", ctypes.c_uint32)]
hdr = CapHeader(0x20080522, 0)
data = (CapData * 2)()
libc.syscall(125, ctypes.byref(hdr), ctypes.byref(data))  # capget
mask = ~((1 << 12) | (1 << 8))
data[0].effective &= mask
data[0].permitted &= mask
data[0].inheritable &= mask
libc.syscall(126, ctypes.byref(hdr), ctypes.byref(data))  # capset
os.execvp(sys.argv[1], sys.argv[1:])
' "$@"
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn script_has_lo_accept_before_rejects() {
        let f = NamedTempFile::new().unwrap();
        write_script(f.path(), None).unwrap();
        let s = fs::read_to_string(f.path()).unwrap();
        let lo = s.find("oif lo accept").unwrap();
        let reject = s.find("reject").unwrap();
        assert!(lo < reject);
    }

    #[test]
    fn carveout_placed_between_lo_and_rejects() {
        let f = NamedTempFile::new().unwrap();
        let rule = "nft add rule inet sandbox output ip daddr 100.64.0.1 tcp dport 18080 accept";
        write_script(f.path(), Some(rule)).unwrap();
        let s = fs::read_to_string(f.path()).unwrap();
        let lo = s.find("oif lo accept").unwrap();
        let carve = s.find("100.64.0.1 tcp dport 18080 accept").unwrap();
        let reject = s.find("reject").unwrap();
        assert!(lo < carve && carve < reject);
    }

    #[test]
    fn script_is_executable() {
        let f = NamedTempFile::new().unwrap();
        write_script(f.path(), None).unwrap();
        let mode = fs::metadata(f.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
    }
}

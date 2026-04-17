//! Headscale-style privilege drop.
//!
//! On a managed install (`/etc/claude-proxy/config.json` present with a
//! `user` field), state-changing subcommands must be invoked as root and
//! this function drops the process to the configured service user before
//! any file I/O happens. On dev / standalone use (no config file, or no
//! `user` field) this function is a no-op — the caller's own uid is used.
//!
//! Order is critical and must match the Python version:
//!   setgroups([gid]) → setgid(gid) → setuid(uid)
//! Supplementary groups have to be wiped before setgid because setuid-to-
//! non-root irrevocably loses CAP_SETGID. Updating HOME/USER/LOGNAME in env
//! keeps anything downstream that expands `~` from looking at root's home.

use std::env;
use std::ffi::CString;

use nix::unistd::{Gid, Uid};

use crate::config::SystemConfig;

pub fn enforce_root_and_drop(config: &SystemConfig, cmd: &str) -> Result<(), crate::Error> {
    let Some(user) = config.user.as_deref() else {
        // No managed config → dev mode, no priv logic at all.
        return Ok(());
    };

    if !Uid::effective().is_root() {
        let hint = SystemConfig::config_path_hint();
        eprintln!(
            "error: `claude-proxy {cmd}` must be run as root on a managed install \
             (config present at {}). Use `sudo claude-proxy {cmd} ...`.",
            hint.display()
        );
        std::process::exit(2);
    }

    let group = config.group.as_deref();
    let pw = lookup_user(user)?;
    let gid = match group {
        Some(g) => lookup_group(g)?,
        None => pw.gid,
    };

    // setgroups first — requires CAP_SETGID which we still have as root.
    nix::unistd::setgroups(&[Gid::from_raw(gid)])
        .map_err(|e| format!("setgroups({gid}) failed: {e}"))?;
    nix::unistd::setgid(Gid::from_raw(gid)).map_err(|e| format!("setgid({gid}) failed: {e}"))?;
    nix::unistd::setuid(Uid::from_raw(pw.uid))
        .map_err(|e| format!("setuid({}) failed: {e}", pw.uid))?;

    // Update env so downstream tools don't see root's HOME.
    env::set_var("HOME", &pw.home);
    env::set_var("USER", &pw.name);
    env::set_var("LOGNAME", &pw.name);
    Ok(())
}

struct Passwd {
    uid: u32,
    gid: u32,
    name: String,
    home: String,
}

fn lookup_user(name: &str) -> Result<Passwd, crate::Error> {
    let cname = CString::new(name).map_err(|_| "invalid user name: contains NUL")?;
    // SAFETY: getpwnam is a standard POSIX call. The returned pointer is
    // owned by libc (statically allocated in some libc impls) and is valid
    // until the next getpw* call on this thread — we copy everything before
    // returning.
    let ptr = unsafe { libc::getpwnam(cname.as_ptr()) };
    if ptr.is_null() {
        return Err(format!("user {name:?} not found").into());
    }
    let pw = unsafe { &*ptr };
    let name = unsafe { cstr_to_string(pw.pw_name)? };
    let home = unsafe { cstr_to_string(pw.pw_dir)? };
    Ok(Passwd {
        uid: pw.pw_uid,
        gid: pw.pw_gid,
        name,
        home,
    })
}

fn lookup_group(name: &str) -> Result<u32, crate::Error> {
    let cname = CString::new(name).map_err(|_| "invalid group name: contains NUL")?;
    let ptr = unsafe { libc::getgrnam(cname.as_ptr()) };
    if ptr.is_null() {
        return Err(format!("group {name:?} not found").into());
    }
    Ok(unsafe { (*ptr).gr_gid })
}

unsafe fn cstr_to_string(p: *const libc::c_char) -> Result<String, crate::Error> {
    if p.is_null() {
        return Ok(String::new());
    }
    let bytes = std::ffi::CStr::from_ptr(p).to_bytes();
    std::str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|e| format!("non-utf8 passwd field: {e}").into())
}

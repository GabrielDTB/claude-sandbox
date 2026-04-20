//! Job-control integration: ctrl+z pauses the sandbox cleanly, `fg` resumes.
//!
//! Without this module, hitting ctrl+z in an interactive session suspends
//! the launcher + its `podman run` client — but conmon, which actually
//! hosts the container, is not in our process group, so the claude
//! process inside keeps executing. The auth-proxy container (started
//! with `-d`) is likewise detached. The user sees a frozen UI whose
//! inner processes are still doing work; if they don't `fg` back in,
//! the containers leak.
//!
//! What we do:
//! * Install sigaction handlers for SIGTSTP and SIGCONT. Handlers only
//!   poke a self-pipe — async-signal-safe.
//! * A worker thread reads the pipe and does the real work: on SIGTSTP
//!   it `podman pause`s each container (cgroup freezer — every task in
//!   the container is truly stopped), saves host termios, then
//!   `kill(getpid, SIGSTOP)`s to hand control to the shell. On SIGCONT
//!   it restores termios and `podman unpause`s in reverse order.
//! * The podman-run client receives SIGTSTP alongside us (same pgrp,
//!   default disposition) and suspends naturally — we don't need to
//!   touch it.
//!
//! Using a self-pipe keeps the signal handler to a single write() call.
//! All the fork+exec for `podman pause` happens on the worker thread,
//! which is ordinary code.
//!
//! No-op when stdin is not a TTY — there's no shell job control to
//! integrate with in that case (piped/CI invocations).

use std::io::IsTerminal;
use std::mem::MaybeUninit;
use std::process::{Command, Stdio};
use std::ptr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI32, Ordering};
use std::thread;

/// Write end of the self-pipe. Signal handlers read this atomically and
/// write one byte to distinguish TSTP (`b'S'`) from CONT (`b'C'`). `-1`
/// means "not installed yet" and the handler is a no-op.
static PIPE_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

/// Host-side termios saved just before we stop, restored on resume.
/// Claude's TUI puts the terminal in raw mode; without this the shell
/// prompt after ctrl+z looks garbled.
static SAVED_TERMIOS: Mutex<Option<libc::termios>> = Mutex::new(None);

/// Install job-control integration for the given containers.
///
/// `proxy` is `None` for the external-proxy case — that container isn't
/// ours to pause.
pub fn install(sandbox: String, proxy: Option<String>) {
    if !std::io::stdin().is_terminal() {
        return;
    }

    let read_fd = match create_self_pipe() {
        Ok(fds) => fds,
        Err(e) => {
            eprintln!("claude-sandboxed: ctrl+z integration disabled ({e})");
            return;
        }
    };

    // Handlers only touch the self-pipe — safe to install now, before
    // the worker is even spawned.
    unsafe {
        install_handler(libc::SIGTSTP, on_tstp);
        install_handler(libc::SIGCONT, on_cont);
    }

    thread::Builder::new()
        .name("claude-sandboxed-suspend".into())
        .spawn(move || worker(read_fd, sandbox, proxy))
        .expect("spawn suspend worker thread");
}

/// Create the pipe, stash the write end in `PIPE_WRITE_FD`, return the
/// read end. Both ends are `O_CLOEXEC` so we don't leak them into podman.
fn create_self_pipe() -> std::io::Result<libc::c_int> {
    let mut fds: [libc::c_int; 2] = [-1, -1];
    // SAFETY: `fds` is a two-element array; pipe2 writes exactly two fds.
    let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    PIPE_WRITE_FD.store(fds[1], Ordering::Release);
    Ok(fds[0])
}

unsafe fn install_handler(sig: libc::c_int, handler: extern "C" fn(libc::c_int)) {
    let mut act = MaybeUninit::<libc::sigaction>::zeroed().assume_init();
    act.sa_sigaction = handler as usize;
    // SA_RESTART so blocking syscalls in the main thread (podman
    // waitpid, our own read()) resume cleanly instead of spraying EINTR.
    act.sa_flags = libc::SA_RESTART;
    libc::sigemptyset(&mut act.sa_mask);
    libc::sigaction(sig, &act, ptr::null_mut());
}

extern "C" fn on_tstp(_: libc::c_int) {
    poke(b'S');
}

extern "C" fn on_cont(_: libc::c_int) {
    poke(b'C');
}

/// Async-signal-safe: only calls `write(2)`, which POSIX requires be
/// signal-safe. Short writes / EAGAIN aren't possible here because each
/// byte fits trivially in the pipe buffer; we ignore the return value
/// defensively.
fn poke(byte: u8) {
    let fd = PIPE_WRITE_FD.load(Ordering::Acquire);
    if fd < 0 {
        return;
    }
    // SAFETY: `fd` is an open pipe we created; `&byte` is a valid
    // 1-byte buffer. write() is listed as async-signal-safe by POSIX.
    unsafe {
        let _ = libc::write(fd, (&byte as *const u8).cast(), 1);
    }
}

fn worker(read_fd: libc::c_int, sandbox: String, proxy: Option<String>) {
    let mut buf = [0u8; 1];
    loop {
        // SAFETY: fd is owned by this thread; buf is a local 1-byte
        // array. read() returns the number of bytes read or -1 on
        // error. Short reads aren't possible for len=1.
        let n = unsafe {
            libc::read(read_fd, buf.as_mut_ptr().cast(), 1)
        };
        if n <= 0 {
            // EOF or error; the pipe was closed somehow. Nothing more
            // we can do — leave signals to their default dispositions
            // for the remainder of the launcher's lifetime.
            return;
        }
        match buf[0] {
            b'S' => on_suspend(&sandbox, proxy.as_deref()),
            b'C' => on_resume(&sandbox, proxy.as_deref()),
            _ => {}
        }
    }
}

fn on_suspend(sandbox: &str, proxy: Option<&str>) {
    // Pause the sandbox first. The podman client has already received
    // SIGTSTP from the terminal and will stop on its own, so the UI is
    // frozen from the user's side anyway — but pausing gives us the
    // kernel-level guarantee that no task inside the container can
    // slip a syscall through before the user comes back.
    pause(sandbox);
    if let Some(p) = proxy {
        pause(p);
    }
    save_termios();
    // SIGSTOP isn't catchable, blockable, or ignorable — from here the
    // shell's job-control machinery runs exactly as if ctrl+z had hit
    // a "normal" process.
    //
    // SAFETY: kill() with a valid pid and signal is always safe.
    unsafe {
        libc::kill(libc::getpid(), libc::SIGSTOP);
    }
}

fn on_resume(sandbox: &str, proxy: Option<&str>) {
    restore_termios();
    // Reverse order: bring the proxy back first so the sandbox has
    // something to talk to the moment it's live again.
    if let Some(p) = proxy {
        unpause(p);
    }
    unpause(sandbox);
}

fn pause(name: &str) {
    let _ = Command::new("podman")
        .args(["pause", name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn unpause(name: &str) {
    let _ = Command::new("podman")
        .args(["unpause", name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn save_termios() {
    // SAFETY: tcgetattr writes into a stack-local termios when the fd
    // is a terminal; return <0 means "not a tty" and we leave the
    // saved value alone. (install() gates on isatty, but stdin can be
    // rebound by the user; this is defensive.)
    unsafe {
        let mut t = MaybeUninit::<libc::termios>::uninit();
        if libc::tcgetattr(libc::STDIN_FILENO, t.as_mut_ptr()) == 0 {
            *SAVED_TERMIOS.lock().unwrap() = Some(t.assume_init());
        }
    }
}

fn restore_termios() {
    let guard = SAVED_TERMIOS.lock().unwrap();
    let Some(t) = guard.as_ref() else { return };
    // SAFETY: `t` is a termios previously produced by tcgetattr on the
    // same fd; tcsetattr with TCSANOW is the matching restorer.
    unsafe {
        libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, t);
    }
}

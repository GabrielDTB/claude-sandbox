//! PTY interposer — the launcher owns the pseudo-terminal between the
//! host and `podman run`, so ctrl+z is intercepted here instead of
//! reaching claude inside the sandbox.
//!
//! Why: with plain `podman run -it`, the HOST tty is put in raw mode and
//! wired straight to the container. ^Z becomes byte 0x1a, flows through,
//! and lands on claude's TUI inside the sandbox — the TUI goes to
//! sleep, the host shell never gets its prompt back, and the containers
//! leak if the user doesn't know the recovery dance. Putting our own
//! pty between host and podman lets us see each byte from the user
//! before it reaches the container.
//!
//! Flow on ^Z:
//!   1. `pump_stdin` sees VSUSP in the byte stream.
//!   2. `podman pause` the sandbox and auth-proxy — cgroup freezer,
//!      kernel-level stop of every task inside.
//!   3. Restore host termios so the shell's output after SIGSTOP lands
//!      in a sane terminal.
//!   4. `kill(getpid, SIGSTOP)`. SIGSTOP is process-wide on Linux, so
//!      all launcher threads freeze together. The shell reaps us and
//!      prints "[N]+ Stopped ...".
//!   5. `fg` sends SIGCONT; the `pump_stdin` thread resumes past the
//!      SIGSTOP. We re-enter raw mode, `podman unpause`, continue.
//!
//! SIGWINCH on the host tty → self-pipe → control thread → TIOCSWINSZ
//! on the pty master, so guest-side resize keeps working.
//!
//! Fallback: non-tty stdin (piped/CI) skips the pty entirely. There's
//! no shell job control to integrate with there — just spawn podman
//! directly.

use std::ffi::{CString, OsStr, OsString};
use std::io::{IsTerminal, Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::process::{Command, ExitCode};
use std::ptr;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI32, Ordering};
use std::thread;

use crate::Error;

/// Run podman with the given argv, integrating ctrl+z with the shell's
/// job control when stdin is a tty. On non-tty stdin, falls back to a
/// direct `Command::status` — no interactive UX to preserve there.
pub fn run(
    podman_args: &[OsString],
    sandbox_name: &str,
    proxy_name: Option<&str>,
) -> Result<ExitCode, Error> {
    if !std::io::stdin().is_terminal() {
        return run_direct(podman_args);
    }
    run_with_pty(podman_args, sandbox_name, proxy_name)
}

fn run_direct(podman_args: &[OsString]) -> Result<ExitCode, Error> {
    let status = Command::new("podman")
        .args(podman_args)
        .status()
        .map_err(|e| -> Error { format!("failed to spawn `podman run`: {e}").into() })?;
    match status.code() {
        Some(c) if (0..=255).contains(&c) => Ok(ExitCode::from(c as u8)),
        _ => Ok(ExitCode::from(1)),
    }
}

// ----------------------------------------------------------------- globals

struct Globals {
    sandbox: String,
    proxy: Option<String>,
    /// Host tty termios as it was before we went raw. Restored on
    /// suspend (so the shell comes back cooked) and on shutdown.
    saved_termios: libc::termios,
}

static GLOBALS: OnceLock<Globals> = OnceLock::new();

/// Write end of the SIGWINCH self-pipe. `-1` until installed. Signal
/// handler writes one byte here; control thread reads it.
static WINCH_PIPE_W: AtomicI32 = AtomicI32::new(-1);

fn globals() -> &'static Globals {
    GLOBALS.get().expect("GLOBALS not initialized")
}

// --------------------------------------------------------------- pty run

fn run_with_pty(
    podman_args: &[OsString],
    sandbox_name: &str,
    proxy_name: Option<&str>,
) -> Result<ExitCode, Error> {
    let (master, slave) = openpty()?;

    // Seed the slave size from the host before fork so early claude
    // output isn't laid out at 80×24.
    if let Some(ws) = get_winsize(libc::STDIN_FILENO) {
        let _ = set_winsize(master.as_raw_fd(), &ws);
    }

    let slave_fd = slave.as_raw_fd();
    let child_pid = fork_podman(podman_args, slave_fd)?;
    // Parent must drop its slave reference: otherwise the master won't
    // see EOF when the child exits (slave refcount stays >0).
    drop(slave);

    // Raw mode AFTER fork — if fork failed we'd have nothing to restore.
    let saved = enter_raw_mode(libc::STDIN_FILENO)?;

    // Install globals so pump/ctl threads can read them. `set` is
    // idempotent across the one-process lifetime we care about.
    let _ = GLOBALS.set(Globals {
        sandbox: sandbox_name.to_string(),
        proxy: proxy_name.map(String::from),
        saved_termios: saved,
    });

    // Master fd is passed by value into threads; we close it explicitly
    // in the shutdown path after both the out-pump and ctl thread have
    // exited. Dropping `OwnedFd` would close too early for the ctl
    // thread's TIOCSWINSZ, so we take ownership into a raw fd instead.
    let master_fd = master.into_raw_fd();

    // Self-pipe for SIGWINCH → control thread.
    let (ctl_r, ctl_w) = pipe_cloexec()?;
    WINCH_PIPE_W.store(ctl_w, Ordering::Release);
    unsafe { install_sigaction(libc::SIGWINCH, on_winch) };

    let ctl_thread = thread::Builder::new()
        .name("pty-ctl".into())
        .spawn(move || ctl_loop(ctl_r, master_fd))
        .expect("spawn pty-ctl thread");

    // Reader side: pty master → host stdout.
    let out_thread = thread::Builder::new()
        .name("pty-out".into())
        .spawn(move || pump_master_to_stdout(master_fd))
        .expect("spawn pty-out thread");

    // Writer side: host stdin → pty master, with VSUSP intercept.
    // Detached on purpose: stdin.read() cannot be cancelled portably,
    // so we let the OS reap the thread when main() returns.
    let vsusp = saved.c_cc[libc::VSUSP];
    let _ = thread::Builder::new()
        .name("pty-in".into())
        .spawn(move || pump_stdin_to_master(master_fd, vsusp))
        .expect("spawn pty-in thread");

    // Block until podman exits.
    let exit = waitpid_blocking(child_pid);

    // Child is gone → slave refcount drops to 0 → master reads EOF →
    // out_thread exits on its own. Wait for that, then tell ctl to
    // shut down.
    let _ = out_thread.join();
    poke_winch_pipe(b'X');
    let _ = ctl_thread.join();

    // Safe to close master now; nobody's reading it.
    unsafe { libc::close(master_fd) };

    // Close winch pipe writer (reader was closed by ctl_loop).
    let w = WINCH_PIPE_W.swap(-1, Ordering::AcqRel);
    if w >= 0 {
        unsafe { libc::close(w) };
    }

    restore_termios(libc::STDIN_FILENO, &globals().saved_termios);
    // Match run.rs's previous terminal hygiene: reset SGR + show cursor.
    let _ = std::io::stdout().write_all(b"\x1b[0m\x1b[?25h");
    let _ = std::io::stdout().flush();

    match exit {
        Some(c) if (0..=255).contains(&c) => Ok(ExitCode::from(c as u8)),
        _ => Ok(ExitCode::from(1)),
    }
}

// -------------------------------------------------------- openpty/termios

struct Winsize {
    rows: u16,
    cols: u16,
}

fn get_winsize(fd: RawFd) -> Option<Winsize> {
    let mut ws = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCGWINSZ writes into `ws` only when fd is a tty; on
    // failure returns non-zero and leaves it zero, which we discard.
    let rc = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) };
    if rc != 0 {
        return None;
    }
    Some(Winsize {
        rows: ws.ws_row,
        cols: ws.ws_col,
    })
}

fn set_winsize(fd: RawFd, ws: &Winsize) -> std::io::Result<()> {
    let w = libc::winsize {
        ws_row: ws.rows,
        ws_col: ws.cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCSWINSZ reads `w`; fd validity is the caller's duty.
    let rc = unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &w) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn openpty() -> Result<(OwnedFd, OwnedFd), Error> {
    let mut master: RawFd = -1;
    let mut slave: RawFd = -1;
    // SAFETY: openpty writes two fds on success; leaves them -1 otherwise.
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            ptr::null_mut(),
            ptr::null(),
            ptr::null(),
        )
    };
    if rc != 0 {
        return Err(format!("openpty failed: {}", std::io::Error::last_os_error()).into());
    }
    // Master gets CLOEXEC so later forks don't leak it. Slave stays
    // non-CLOEXEC: the child needs to inherit it for dup2 into stdio,
    // and we dup2 *before* exec so CLOEXEC wouldn't fire anyway — but
    // keeping it inheritable costs us nothing and matches openpty(3)'s
    // default.
    // SAFETY: master is a freshly-opened fd we own.
    unsafe {
        let flags = libc::fcntl(master, libc::F_GETFD);
        if flags >= 0 {
            libc::fcntl(master, libc::F_SETFD, flags | libc::FD_CLOEXEC);
        }
    }
    // SAFETY: both fds are valid and owned by us now.
    Ok((unsafe { OwnedFd::from_raw_fd(master) }, unsafe {
        OwnedFd::from_raw_fd(slave)
    }))
}

fn enter_raw_mode(fd: RawFd) -> Result<libc::termios, Error> {
    let mut saved = MaybeUninit::<libc::termios>::uninit();
    // SAFETY: tcgetattr writes a full termios on success.
    let rc = unsafe { libc::tcgetattr(fd, saved.as_mut_ptr()) };
    if rc != 0 {
        return Err(format!("tcgetattr failed: {}", std::io::Error::last_os_error()).into());
    }
    let saved = unsafe { saved.assume_init() };

    let mut raw = saved;
    // SAFETY: cfmakeraw modifies in place.
    unsafe { libc::cfmakeraw(&mut raw) };
    // SAFETY: tcsetattr reads `raw`; fd validity checked above.
    let rc = unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) };
    if rc != 0 {
        return Err(format!("tcsetattr failed: {}", std::io::Error::last_os_error()).into());
    }
    Ok(saved)
}

fn restore_termios(fd: RawFd, t: &libc::termios) {
    // SAFETY: tcsetattr reads `t`; nothing else.
    unsafe { libc::tcsetattr(fd, libc::TCSANOW, t) };
}

// --------------------------------------------------------------- fork/exec

fn fork_podman(args: &[OsString], slave_fd: RawFd) -> Result<libc::pid_t, Error> {
    // Build argv in the parent; the child only does async-signal-safe
    // work after fork (no allocations, no format!).
    let prog = CString::new("podman").expect("static str");
    let mut cargs: Vec<CString> = Vec::with_capacity(args.len() + 1);
    cargs.push(prog.clone());
    for a in args {
        let bytes = <OsStr as OsStrExt>::as_bytes(a.as_os_str());
        cargs.push(
            CString::new(bytes)
                .map_err(|e| -> Error { format!("podman arg contains NUL: {e}").into() })?,
        );
    }
    let mut cptrs: Vec<*const libc::c_char> = cargs.iter().map(|s| s.as_ptr()).collect();
    cptrs.push(ptr::null());

    // SAFETY: fork() is well-defined. Child path below is restricted to
    // async-signal-safe calls (setsid, ioctl, dup2, close, execvp,
    // write, _exit). No allocations, no locks, no Rust std I/O.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(format!("fork failed: {}", std::io::Error::last_os_error()).into());
    }
    if pid == 0 {
        // -- child --
        unsafe {
            // New session + set slave as controlling tty. Puts podman in
            // its own session so SIGSTOP/SIGCONT targeted at the launcher
            // don't leak into it.
            libc::setsid();
            libc::ioctl(slave_fd, libc::TIOCSCTTY, 0);

            // Slave → stdin/stdout/stderr.
            libc::dup2(slave_fd, 0);
            libc::dup2(slave_fd, 1);
            libc::dup2(slave_fd, 2);
            if slave_fd > 2 {
                libc::close(slave_fd);
            }

            libc::execvp(prog.as_ptr(), cptrs.as_ptr());
            // exec only returns on failure. Async-signal-safe error report.
            let msg: &[u8] = b"claude-sandboxed: execvp(podman) failed\n";
            let _ = libc::write(2, msg.as_ptr().cast(), msg.len());
            libc::_exit(127);
        }
    }
    Ok(pid)
}

/// Block on the given child. Returns the exit code on clean exit, or
/// `None` for signalled / wait errors — callers collapse those to 1.
fn waitpid_blocking(pid: libc::pid_t) -> Option<i32> {
    let mut status: libc::c_int = 0;
    loop {
        // SAFETY: waitpid on a child pid we own.
        let rc = unsafe { libc::waitpid(pid, &mut status, 0) };
        if rc == pid {
            break;
        }
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return None;
        }
    }
    if libc::WIFEXITED(status) {
        Some(libc::WEXITSTATUS(status))
    } else {
        None
    }
}

// -------------------------------------------------------- signals + pipe

fn pipe_cloexec() -> Result<(RawFd, RawFd), Error> {
    let mut fds: [RawFd; 2] = [-1, -1];
    // SAFETY: pipe2 writes exactly two fds.
    let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
    if rc != 0 {
        return Err(format!("pipe2 failed: {}", std::io::Error::last_os_error()).into());
    }
    Ok((fds[0], fds[1]))
}

unsafe fn install_sigaction(sig: libc::c_int, handler: extern "C" fn(libc::c_int)) {
    let mut act = MaybeUninit::<libc::sigaction>::zeroed().assume_init();
    act.sa_sigaction = handler as usize;
    // SA_RESTART so blocking syscalls (our pumps' read/write) resume
    // cleanly after the handler returns instead of spraying EINTR.
    act.sa_flags = libc::SA_RESTART;
    libc::sigemptyset(&mut act.sa_mask);
    libc::sigaction(sig, &act, ptr::null_mut());
}

extern "C" fn on_winch(_: libc::c_int) {
    poke_winch_pipe(b'W');
}

/// Async-signal-safe pipe poke. Single `write(2)` of one byte; short
/// writes aren't possible at length 1 and errors are ignored.
fn poke_winch_pipe(byte: u8) {
    let fd = WINCH_PIPE_W.load(Ordering::Acquire);
    if fd < 0 {
        return;
    }
    // SAFETY: fd is an open pipe; buffer is a 1-byte local.
    unsafe {
        let _ = libc::write(fd, (&byte as *const u8).cast(), 1);
    }
}

// ----------------------------------------------------------- control thread

fn ctl_loop(read_fd: RawFd, master_fd: RawFd) {
    let mut buf = [0u8; 1];
    loop {
        // SAFETY: fd is owned by this thread; 1-byte read has no short-read issue.
        let n = unsafe { libc::read(read_fd, buf.as_mut_ptr().cast(), 1) };
        if n <= 0 {
            break;
        }
        match buf[0] {
            b'W' => forward_winch(master_fd),
            b'X' => break,
            _ => {}
        }
    }
    // SAFETY: we own the read end.
    unsafe { libc::close(read_fd) };
}

fn forward_winch(master_fd: RawFd) {
    if let Some(ws) = get_winsize(libc::STDIN_FILENO) {
        let _ = set_winsize(master_fd, &ws);
    }
}

// ------------------------------------------------------------------ pumps

fn pump_master_to_stdout(master: RawFd) {
    let mut buf = [0u8; 8192];
    // Hot path: acquire the stdout lock once for the whole pump, not
    // per read. Nothing else in the launcher writes to stdout after
    // setup, so exclusive hold is fine.
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    loop {
        // SAFETY: master fd is owned for the lifetime of the pump
        // thread (main joins us before closing).
        let n = unsafe { libc::read(master, buf.as_mut_ptr().cast(), buf.len()) };
        if n <= 0 {
            // EOF (slave closed) or EIO (master hung up) — done.
            break;
        }
        if lock.write_all(&buf[..n as usize]).is_err() {
            break;
        }
        if lock.flush().is_err() {
            break;
        }
    }
}

fn pump_stdin_to_master(master: RawFd, vsusp: u8) {
    let mut buf = [0u8; 4096];
    let stdin = std::io::stdin();
    loop {
        let n = {
            let mut lock = stdin.lock();
            match lock.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            }
        };
        // Byte-wise scan is safe even with UTF-8 input: VSUSP (typically
        // 0x1A) is a C0 control code and can never appear mid-codepoint
        // — UTF-8 continuation bytes are always 0x80..=0xBF.
        match buf[..n].iter().position(|&b| b == vsusp) {
            Some(idx) => {
                if idx > 0 && !write_all_fd(master, &buf[..idx]) {
                    break;
                }
                suspend_and_wait(master);
                // Everything after VSUSP flows through as normal input.
                if idx + 1 < n && !write_all_fd(master, &buf[idx + 1..n]) {
                    break;
                }
            }
            None => {
                if !write_all_fd(master, &buf[..n]) {
                    break;
                }
            }
        }
    }
}

fn write_all_fd(fd: RawFd, data: &[u8]) -> bool {
    let mut off = 0;
    while off < data.len() {
        // SAFETY: fd ownership same as read side; buf is a valid slice.
        let rc = unsafe { libc::write(fd, data[off..].as_ptr().cast(), data.len() - off) };
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return false;
        }
        if rc == 0 {
            return false;
        }
        off += rc as usize;
    }
    true
}

// -------------------------------------------------------------- suspend

/// Pause containers, restore termios, SIGSTOP self, and on resume
/// re-enter raw + unpause. Runs synchronously on the pump-stdin thread;
/// SIGSTOP freezes all launcher threads together.
fn suspend_and_wait(master_fd: RawFd) {
    let g = globals();

    // Pause first so no in-container syscall slips between "^Z seen"
    // and "process frozen".
    podman_cmd(&["pause", &g.sandbox]);
    if let Some(p) = &g.proxy {
        podman_cmd(&["pause", p]);
    }

    // Shell sees cooked mode on return from SIGSTOP.
    restore_termios(libc::STDIN_FILENO, &g.saved_termios);
    // Finish the current line and show the cursor so the shell's
    // "[N]+ Stopped" lands cleanly under whatever the TUI left.
    let _ = std::io::stdout().write_all(b"\r\x1b[0m\x1b[?25h\n");
    let _ = std::io::stdout().flush();

    // Uncatchable, unblockable — the shell's job-control machinery
    // drives the rest from here.
    // SAFETY: kill() with a valid pid and signal.
    unsafe { libc::kill(libc::getpid(), libc::SIGSTOP) };

    // --- resumed by SIGCONT (fg / bg) ---

    // Re-enter raw mode; we need it again for byte-level pumping.
    let mut raw = g.saved_termios;
    // SAFETY: cfmakeraw modifies in place; tcsetattr reads it.
    unsafe {
        libc::cfmakeraw(&mut raw);
        libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw);
    }

    // Unpause in reverse order — proxy first so the sandbox has
    // something upstream the moment it's live.
    if let Some(p) = &g.proxy {
        podman_cmd(&["unpause", p]);
    }
    podman_cmd(&["unpause", &g.sandbox]);

    // Terminal might have been resized while we were stopped (we don't
    // receive SIGWINCH during a stop). Push current size through so the
    // TUI redraws at the right geometry on resume.
    forward_winch(master_fd);
}

fn podman_cmd(args: &[&str]) {
    let _ = Command::new("podman")
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

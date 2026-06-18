//! Generate the container-side `notebook-entrypoint.sh` for `--marimo` mode.
//!
//! Instead of dropping into the Claude TUI, the notebook mode runs two
//! processes inside the sandbox:
//!   * a `stdio-to-ws` bridge wrapping `claude-code-acp`, exposing the Claude
//!     Code ACP agent over a WebSocket on [`crate::constants::ACP_PORT`];
//!   * `marimo edit` on a workspace file, serving the notebook UI on
//!     [`crate::constants::MARIMO_PORT`].
//!
//! Both ports are published to host loopback by `run.rs` so the user drives
//! the notebook (and its agent panel) from a browser outside the sandbox. The
//! ACP sidecar's own Claude requests inherit `ANTHROPIC_BASE_URL` + the stub
//! creds already present in the container, so they flow through the auth proxy
//! like the normal Claude TUI.
//!
//! The firewall script still execs this script as its final argument (after
//! dropping caps), so the cap-drop / nftables isolation is identical to the
//! Claude TUI path.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path};

/// Resolve a `--notebook-file` value (workspace-relative) into the container
/// path under `/workspace`, rejecting anything that could escape the workspace
/// or break out of the single-quoted shell literal we embed it in.
///
/// `None` defaults to `notebook.py`. Returns the absolute in-container path
/// (e.g. `/workspace/sub/nb.py`).
pub fn container_path(file: Option<&Path>) -> Result<String, crate::Error> {
    let rel = file.unwrap_or_else(|| Path::new("notebook.py"));

    if rel.is_absolute() {
        return Err(format!(
            "--notebook-file must be relative to the workspace, got absolute path: {}",
            rel.display()
        )
        .into());
    }
    for comp in rel.components() {
        match comp {
            Component::Normal(_) => {}
            Component::CurDir => {}
            _ => {
                return Err(format!(
                    "--notebook-file must not contain `..` or rooted components: {}",
                    rel.display()
                )
                .into());
            }
        }
    }
    let s = rel
        .to_str()
        .ok_or_else(|| -> crate::Error { "--notebook-file is not valid UTF-8".into() })?;
    if s.is_empty() {
        return Err("--notebook-file is empty".into());
    }
    // We embed the path in a single-quoted shell literal; a `'` would break out
    // of it. Reject rather than attempt to escape.
    if s.contains('\'') {
        return Err("--notebook-file must not contain single quotes".into());
    }
    Ok(format!("/workspace/{s}"))
}

/// Write the notebook entrypoint script to `path` with mode `0755`.
///
/// `notebook_path` must be a validated in-container path from
/// [`container_path`]. `acp_port` / `marimo_port` are the in-container listen
/// ports (published to host loopback by `run.rs`).
pub fn write_script(
    path: &Path,
    notebook_path: &str,
    acp_port: u16,
    marimo_port: u16,
) -> Result<(), crate::Error> {
    let script = format!(
        "#!/bin/bash\n\
         set -e\n\
         # One writable venv per sandbox at /workspace/.venv. The nix-store\n\
         # interpreter ($NOTEBOOK_PYTHON, set in the notebook image's env) is\n\
         # read-only / externally-managed, so `uv pip install` from a notebook\n\
         # cell would fail against it. --system-site-packages lets the venv\n\
         # borrow marimo (and its closure) from that interpreter while keeping\n\
         # installs writable into the venv. Built once; reused on relaunch.\n\
         VENV=/workspace/.venv\n\
         if [ ! -x \"$VENV/bin/python\" ]; then\n\
         uv venv --system-site-packages --python \"$NOTEBOOK_PYTHON\" \"$VENV\"\n\
         fi\n\
         # Activate the venv for everything launched below: marimo's kernel AND\n\
         # the claude sidecar, so a package installed from a cell is importable\n\
         # by both. Exported before the sidecar starts so it inherits them.\n\
         export VIRTUAL_ENV=\"$VENV\"\n\
         export PATH=\"$VENV/bin:$PATH\"\n\
         # Seed marimo config (skip if one was already provided):\n\
         #   * experimental.external_agents = true  -> the ACP agent panel is\n\
         #     enabled out of the box (no manual Lab toggle); marimo's frontend\n\
         #     then connects to the claude-code-acp bridge at ws://localhost:{acp_port}.\n\
         #   * package_management.manager = uv  -> uv (on PATH) instead of pip,\n\
         #     which the bare interpreter doesn't ship.\n\
         if [ ! -f \"$HOME/.config/marimo/marimo.toml\" ]; then\n\
         mkdir -p \"$HOME/.config/marimo\"\n\
         printf '[experimental]\\nexternal_agents = true\\n\\n[package_management]\\nmanager = \"uv\"\\n' > \"$HOME/.config/marimo/marimo.toml\"\n\
         fi\n\
         # ACP sidecar: bridge claude-code-acp's stdio onto a WebSocket the\n\
         # browser-side marimo agent panel connects to.\n\
         stdio-to-ws \"claude-code-acp\" --port {acp_port} &\n\
         # Notebook server in the foreground (owns the PTY), run as the venv\n\
         # python so its kernel installs/imports land in the venv. --host\n\
         # 0.0.0.0 so the published loopback port reaches it; --proxy\n\
         # localhost:{marimo_port} so the banner prints the host-reachable URL\n\
         # (published on host loopback at this port) instead of the container's\n\
         # 0.0.0.0 / LAN IPs; --headless to not open a browser in the container.\n\
         exec \"$VENV/bin/python\" -m marimo edit '{notebook_path}' --host 0.0.0.0 --port {marimo_port} --proxy localhost:{marimo_port} --headless\n",
    );

    let mut f = fs::File::create(path)?;
    f.write_all(script.as_bytes())?;
    f.flush()?;
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn default_file_is_notebook_py() {
        assert_eq!(container_path(None).unwrap(), "/workspace/notebook.py");
    }

    #[test]
    fn relative_subdir_ok() {
        assert_eq!(
            container_path(Some(Path::new("sub/nb.py"))).unwrap(),
            "/workspace/sub/nb.py"
        );
    }

    #[test]
    fn rejects_absolute() {
        assert!(container_path(Some(Path::new("/etc/passwd"))).is_err());
    }

    #[test]
    fn rejects_parent_traversal() {
        assert!(container_path(Some(Path::new("../escape.py"))).is_err());
        assert!(container_path(Some(Path::new("sub/../../escape.py"))).is_err());
    }

    #[test]
    fn rejects_single_quote() {
        assert!(container_path(Some(Path::new("a'b.py"))).is_err());
    }

    #[test]
    fn script_has_both_processes_and_is_executable() {
        let f = NamedTempFile::new().unwrap();
        write_script(f.path(), "/workspace/notebook.py", 3017, 2718).unwrap();
        let s = fs::read_to_string(f.path()).unwrap();
        assert!(s.contains("stdio-to-ws \"claude-code-acp\" --port 3017"));
        assert!(s.contains("-m marimo edit '/workspace/notebook.py'"));
        assert!(s.contains("--port 2718"));
        assert!(s.contains("--proxy localhost:2718"));
        // per-sandbox venv at /workspace/.venv, run marimo as the venv python
        assert!(s.contains("VENV=/workspace/.venv"));
        assert!(s.contains("uv venv --system-site-packages --python \"$NOTEBOOK_PYTHON\""));
        assert!(s.contains("export VIRTUAL_ENV=\"$VENV\""));
        assert!(s.contains("export PATH=\"$VENV/bin:$PATH\""));
        assert!(s.contains("exec \"$VENV/bin/python\" -m marimo edit"));
        // seeds marimo config: external-agents panel + uv package manager
        assert!(s.contains("[experimental]"));
        assert!(s.contains("external_agents = true"));
        assert!(s.contains("[package_management]"));
        assert!(s.contains("manager = \"uv\""));
        // venv created before sidecar; sidecar backgrounded before marimo exec.
        let venv = s.find("uv venv").unwrap();
        let acp = s.find("stdio-to-ws").unwrap();
        let marimo = s.find("-m marimo edit").unwrap();
        assert!(venv < acp);
        assert!(acp < marimo);
        let mode = fs::metadata(f.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
    }
}

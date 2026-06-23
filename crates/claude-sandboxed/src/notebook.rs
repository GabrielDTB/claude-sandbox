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
//! The Python environment is provisioned by `pixi`: the entrypoint declares a
//! `[tool.pixi]` environment in the workspace `pyproject.toml` (seeding it, and
//! `marimo` from PyPI, if absent), creates the per-sandbox env at
//! `/workspace/.pixi`, and runs marimo inside it so an in-cell `pixi add`
//! reaches the live kernel. Pixi's lockfile reconciles dependency adds/removals,
//! so there is no manual change-gating here.
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
         cd /workspace\n\
         # One pixi environment per sandbox at /workspace/.pixi, declared in the\n\
         # workspace pyproject.toml's [tool.pixi] tables. `pixi init` augments an\n\
         # existing pyproject (or creates one); any PEP 621 [project.dependencies]\n\
         # already present are picked up by pixi as PyPI deps. marimo runs INSIDE\n\
         # this env (not borrowed from a read-only nix interpreter as the old uv\n\
         # path did), so an in-cell `pixi add` reaches the live kernel.\n\
         if ! grep -q '\\[tool\\.pixi' pyproject.toml 2>/dev/null; then\n\
         pixi init --format pyproject .\n\
         fi\n\
         # Base interpreter (conda) + marimo (PyPI). Idempotent: each `pixi add`\n\
         # is skipped when the dep is already declared, so a committed manifest\n\
         # is left untouched.\n\
         grep -q '^python' pyproject.toml || pixi add python\n\
         grep -q 'marimo' pyproject.toml || pixi add --pypi marimo\n\
         # Solve + materialize the env. Pixi's lockfile reconciles adds/removals,\n\
         # so removing a dependency takes effect without any manual rebuild.\n\
         echo 'claude-sandboxed: provisioning pixi environment' >&2\n\
         pixi install\n\
         # Activate the env for everything launched below: marimo's kernel AND\n\
         # the claude sidecar, so a package installed from a cell is importable\n\
         # by both. Sourced before the sidecar starts so it inherits the env.\n\
         eval \"$(pixi shell-hook)\"\n\
         # Seed marimo config (skip if one was already provided):\n\
         #   * experimental.external_agents = true  -> the ACP agent panel is\n\
         #     enabled out of the box (no manual Lab toggle); marimo's frontend\n\
         #     then connects to the claude-code-acp bridge at ws://localhost:{acp_port}.\n\
         #   * package_management.manager = pixi  -> in-cell installs go through\n\
         #     pixi (which owns the active env) instead of pip.\n\
         if [ ! -f \"$HOME/.config/marimo/marimo.toml\" ]; then\n\
         mkdir -p \"$HOME/.config/marimo\"\n\
         printf '[experimental]\\nexternal_agents = true\\n\\n[package_management]\\nmanager = \"pixi\"\\n' > \"$HOME/.config/marimo/marimo.toml\"\n\
         fi\n\
         # ACP sidecar: bridge claude-code-acp's stdio onto a WebSocket the\n\
         # browser-side marimo agent panel connects to.\n\
         stdio-to-ws \"claude-code-acp\" --port {acp_port} &\n\
         # Notebook server in the foreground (owns the PTY), run inside the pixi\n\
         # env so its kernel installs/imports land there. --host 0.0.0.0 so the\n\
         # published loopback port reaches it; --proxy localhost:{marimo_port} so\n\
         # the banner prints the host-reachable URL (published on host loopback at\n\
         # this port) instead of the container's 0.0.0.0 / LAN IPs; --headless to\n\
         # not open a browser in the container.\n\
         exec marimo edit '{notebook_path}' --host 0.0.0.0 --port {marimo_port} --proxy localhost:{marimo_port} --headless\n",
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
        assert!(s.contains("marimo edit '/workspace/notebook.py'"));
        assert!(s.contains("--port 2718"));
        assert!(s.contains("--proxy localhost:2718"));
        // per-sandbox pixi env at /workspace/.pixi, marimo run inside it
        assert!(s.contains("cd /workspace"));
        assert!(s.contains("pixi init --format pyproject ."));
        assert!(s.contains("pixi add --pypi marimo"));
        assert!(s.contains("pixi install"));
        assert!(s.contains("eval \"$(pixi shell-hook)\""));
        assert!(s.contains("exec marimo edit"));
        // environment is declared in the workspace pyproject.toml's [tool.pixi]
        assert!(s.contains("grep -q '\\[tool\\.pixi' pyproject.toml"));
        // seeds marimo config: external-agents panel + pixi package manager
        assert!(s.contains("[experimental]"));
        assert!(s.contains("external_agents = true"));
        assert!(s.contains("[package_management]"));
        assert!(s.contains("manager = \"pixi\""));
        // env provisioned + activated, then sidecar, then marimo exec.
        let install = s.find("pixi install").unwrap();
        let activate = s.find("pixi shell-hook").unwrap();
        let acp = s.find("stdio-to-ws").unwrap();
        let marimo = s.find("exec marimo edit").unwrap();
        assert!(install < activate);
        assert!(activate < acp);
        assert!(acp < marimo);
        let mode = fs::metadata(f.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
    }
}

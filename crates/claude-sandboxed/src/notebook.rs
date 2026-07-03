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
//! Marimo's own AI features ("generate with AI", AI chat) are wired to the
//! same auth proxy: the seeded config registers an OpenAI-compatible custom
//! provider pointing at `$ANTHROPIC_BASE_URL/v1/` (Anthropic's OpenAI-compat
//! endpoint, reachable through the proxy's `/v1/` allowlist) using the stub
//! sandbox-to-proxy token as the api_key, since the openai client transmits it
//! as the `Authorization: Bearer` header the proxy authenticates.
//!
//! A `pixi` shim is placed on the kernel's PATH to work around rattler pinning
//! directory mtimes to 1980: CPython's `FileFinder` only re-scans a directory
//! whose mtime changed, so without the shim (which touches the env's
//! site-packages after each pixi call) an in-cell install stays invisible to
//! the running kernel until restart.
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
         # openai: marimo drives OpenAI-compatible providers (our proxy, below)\n\
         # through the openai client package.\n\
         grep -q 'openai' pyproject.toml || pixi add --pypi openai\n\
         # Solve + materialize the env. Pixi's lockfile reconciles adds/removals,\n\
         # so removing a dependency takes effect without any manual rebuild.\n\
         echo 'claude-sandboxed: provisioning pixi environment' >&2\n\
         pixi install\n\
         # Activate the env for everything launched below: marimo's kernel AND\n\
         # the claude sidecar, so a package installed from a cell is importable\n\
         # by both. Sourced before the sidecar starts so it inherits the env.\n\
         # Resolve the real pixi binary first: the shell-hook defines a `pixi`\n\
         # shell function, after which `command -v pixi` no longer returns a path.\n\
         pixi_real=\"$(command -v pixi)\"\n\
         eval \"$(pixi shell-hook)\"\n\
         # Shim pixi for marimo's kernel: rattler pins directory mtimes to\n\
         # 1980-01-01, and CPython's FileFinder only re-lists a directory when\n\
         # its mtime CHANGES -- so a package installed by an in-cell `pixi add`\n\
         # stays invisible to the live kernel until restart (pip avoids this by\n\
         # bumping site-packages' mtime as a side effect of writing into it).\n\
         # The shim bumps the env's site-packages mtimes after every pixi call,\n\
         # so the kernel's next import attempt re-scans and finds the package.\n\
         # Marimo spawns `pixi` via PATH lookup, which resolves to this shim.\n\
         shim_dir=\"$HOME/.cache/claude-sandboxed/pixi-shim\"\n\
         mkdir -p \"$shim_dir\"\n\
         cat > \"$shim_dir/pixi\" <<PIXI_SHIM\n\
         #!/bin/bash\n\
         \"$pixi_real\" \"\\$@\"\n\
         rc=\\$?\n\
         touch /workspace/.pixi/envs/*/lib/python*/site-packages 2>/dev/null\n\
         exit \\$rc\n\
         PIXI_SHIM\n\
         chmod +x \"$shim_dir/pixi\"\n\
         export PATH=\"$shim_dir:$PATH\"\n\
         # Seed marimo config (skip if one was already provided):\n\
         #   * experimental.external_agents = true  -> the ACP agent panel is\n\
         #     enabled out of the box (no manual Lab toggle); marimo's frontend\n\
         #     then connects to the claude-code-acp bridge at ws://localhost:{acp_port}.\n\
         #   * package_management.manager = pixi  -> in-cell installs go through\n\
         #     pixi (which owns the active env) instead of pip.\n\
         #   * ai.custom_providers.claude-proxy -> \"generate with AI\" / AI chat\n\
         #     via the auth proxy: Anthropic's OpenAI-compat /v1/chat/completions\n\
         #     lives under the proxy's allowed /v1/ prefix, and the openai client\n\
         #     sends its api_key as `Authorization: Bearer ...`, which is exactly\n\
         #     the sandbox-to-proxy token scheme -- so the stub accessToken from\n\
         #     ~/.claude/.credentials.json doubles as the provider api_key. No\n\
         #     autocomplete_model on purpose: inline completion fires per\n\
         #     keystroke and would chew through subscription quota.\n\
         if [ ! -f \"$HOME/.config/marimo/marimo.toml\" ]; then\n\
         mkdir -p \"$HOME/.config/marimo\"\n\
         printf '[experimental]\\nexternal_agents = true\\n\\n[package_management]\\nmanager = \"pixi\"\\n' > \"$HOME/.config/marimo/marimo.toml\"\n\
         proxy_token=\"$(python -c 'import json,os;print(json.load(open(os.path.expanduser(\"~/.claude/.credentials.json\")))[\"claudeAiOauth\"][\"accessToken\"])' 2>/dev/null || true)\"\n\
         if [ -n \"$proxy_token\" ] && [ -n \"${{ANTHROPIC_BASE_URL-}}\" ]; then\n\
         printf '\\n[ai.models]\\nchat_model = \"claude-proxy/claude-sonnet-4-6\"\\nedit_model = \"claude-proxy/claude-sonnet-4-6\"\\ncustom_models = [\"claude-proxy/claude-opus-4-8\", \"claude-proxy/claude-sonnet-4-6\", \"claude-proxy/claude-haiku-4-5-20251001\"]\\n\\n[ai.custom_providers.claude-proxy]\\napi_key = \"%s\"\\nbase_url = \"%s/v1/\"\\n' \"$proxy_token\" \"$ANTHROPIC_BASE_URL\" >> \"$HOME/.config/marimo/marimo.toml\"\n\
         else\n\
         echo 'claude-sandboxed: no proxy creds/base-url found; skipping marimo AI provider config' >&2\n\
         fi\n\
         fi\n\
         # ACP sidecar: bridge claude-code-acp's stdio onto a WebSocket the\n\
         # browser-side marimo agent panel auto-connects to at\n\
         # ws://<host>:{acp_port}/message. Resolve both vendored binaries up\n\
         # front so a missing/renamed binary fails LOUDLY here instead of\n\
         # silently dying in the background (a backgrounded crash under `set -e`\n\
         # is invisible and leaves marimo's panel stuck on \"connect to an\n\
         # agent\"). PATH still contains the nix /bin after the shell-hook's\n\
         # prepend, so command -v resolves the store symlinks.\n\
         acp_bridge=\"$(command -v stdio-to-ws || true)\"\n\
         acp_agent=\"$(command -v claude-code-acp || true)\"\n\
         if [ -z \"$acp_bridge\" ] || [ -z \"$acp_agent\" ]; then\n\
         echo \"claude-sandboxed: ERROR: ACP bridge missing (stdio-to-ws='$acp_bridge' claude-code-acp='$acp_agent'); marimo agent panel will not connect.\" >&2\n\
         else\n\
         acp_log=\"$HOME/.cache/claude-sandboxed/acp.log\"\n\
         mkdir -p \"$HOME/.cache/claude-sandboxed\"\n\
         # Supervise so a transient crash doesn't permanently disconnect the\n\
         # panel. The adapter is passed to stdio-to-ws by absolute path so its\n\
         # child spawn doesn't depend on PATH; bridge + adapter output is\n\
         # appended to $acp_log for post-mortem. `&& rc=0 || rc=$?` both\n\
         # neutralizes `set -e` and captures the real exit code.\n\
         ( while true; do\n\
         \"$acp_bridge\" \"$acp_agent\" --port {acp_port} >>\"$acp_log\" 2>&1 && rc=0 || rc=$?\n\
         echo \"claude-sandboxed: ACP bridge exited (rc=$rc), restarting in 2s; see $acp_log\" >&2\n\
         sleep 2\n\
         done ) &\n\
         echo \"claude-sandboxed: ACP bridge supervised (pid $!) on port {acp_port}, logging to $acp_log\" >&2\n\
         fi\n\
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
        // ACP bridge: binaries resolved up front, supervised, logged.
        assert!(s.contains("command -v stdio-to-ws"));
        assert!(s.contains("command -v claude-code-acp"));
        assert!(s.contains("\"$acp_bridge\" \"$acp_agent\" --port 3017"));
        assert!(s.contains(".cache/claude-sandboxed/acp.log"));
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
        // pixi shim: real binary resolved BEFORE the shell-hook defines a
        // `pixi` function, shim bumps site-packages mtimes (rattler pins them
        // to 1980, defeating FileFinder's mtime-based re-scan), and PATH is
        // prepended so marimo's kernel resolves the shim.
        assert!(s.contains("pixi_real=\"$(command -v pixi)\""));
        assert!(s.contains("touch /workspace/.pixi/envs/*/lib/python*/site-packages"));
        assert!(s.contains("export PATH=\"$shim_dir:$PATH\""));
        let resolve = s.find("pixi_real=").unwrap();
        let hook = s.find("pixi shell-hook").unwrap();
        let shim = s.find("shim_dir=").unwrap();
        assert!(resolve < hook);
        assert!(hook < shim);
        // environment is declared in the workspace pyproject.toml's [tool.pixi]
        assert!(s.contains("grep -q '\\[tool\\.pixi' pyproject.toml"));
        // seeds marimo config: external-agents panel + pixi package manager
        assert!(s.contains("[experimental]"));
        assert!(s.contains("external_agents = true"));
        assert!(s.contains("[package_management]"));
        assert!(s.contains("manager = \"pixi\""));
        // AI provider through the auth proxy: openai package installed, custom
        // provider seeded from the stub token + ANTHROPIC_BASE_URL (guarded so
        // a missing token degrades to a working notebook without AI).
        assert!(s.contains("pixi add --pypi openai"));
        assert!(s.contains("[ai.custom_providers.claude-proxy]"));
        assert!(s.contains("api_key = \"%s\""));
        assert!(s.contains("base_url = \"%s/v1/\""));
        assert!(s.contains("edit_model = \"claude-proxy/claude-sonnet-4-6\""));
        assert!(s.contains("claudeAiOauth"));
        assert!(s.contains("skipping marimo AI provider config"));
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

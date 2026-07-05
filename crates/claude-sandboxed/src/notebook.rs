//! Generate the container-side `notebook-entrypoint.sh` for `--marimo` mode.
//!
//! Instead of dropping into the Claude TUI, the notebook mode runs two
//! processes inside the sandbox:
//!   * a `stdio-to-ws` bridge wrapping `claude-agent-acp`, exposing the Claude
//!     Code ACP agent over a WebSocket on [`crate::constants::ACP_PORT`];
//!   * `marimo edit` on a workspace file, serving the notebook UI on
//!     [`crate::constants::MARIMO_PORT`].
//!
//! Both ports are published to host loopback by `run.rs` so the user drives
//! the notebook (and its agent panel) from a browser outside the sandbox. The
//! ACP sidecar's own Claude requests inherit `ANTHROPIC_BASE_URL` + the stub
//! creds already present in the container, so they flow through the auth proxy
//! like the normal Claude TUI. `--permissive` is honored too: `run.rs` sets
//! `CLAUDE_ACP_PERMISSION_MODE=bypassPermissions` in the container env, which
//! the vendored `claude-agent-acp` (patched in `container.nix`) reads as the
//! starting permission mode for every ACP session.
//!
//! The Python environment is provisioned by `pixi`: the entrypoint declares a
//! `[tool.pixi]` environment in the workspace `pyproject.toml` (seeding it, and
//! `marimo` from PyPI, if absent), creates the per-sandbox env at
//! `/workspace/.pixi`, and runs marimo inside it so an in-cell `pixi add`
//! reaches the live kernel. Pixi's lockfile reconciles dependency adds/removals,
//! so there is no manual change-gating here. The workspace itself is never
//! installed into the env: `pixi init` registers it as an editable PyPI package
//! (`<name> = { path = ".", editable = true }`), which turns `pixi install`
//! into a wheel build of `/workspace` — fatal for any workspace that isn't a
//! well-formed python package — so the entrypoint strips that dep right after
//! init (and drops the stale one from workspaces provisioned before the strip
//! existed, when its package dir is missing and the build could never succeed).
//!
//! Marimo's own AI features ("generate with AI", AI chat) are wired to the
//! same auth proxy: the seeded config registers an OpenAI-compatible custom
//! provider pointing at `$ANTHROPIC_BASE_URL/v1/` (Anthropic's OpenAI-compat
//! endpoint, reachable through the proxy's `/v1/` allowlist) using the stub
//! sandbox-to-proxy token as the api_key, since the openai client transmits it
//! as the `Authorization: Bearer` header the proxy authenticates. That chat
//! model list is fetched live from `/v1/models` at startup.
//!
//! The ACP agent panel is a separate surface whose model list comes from the
//! sidecar SDK's bundled catalog, not `/v1/models`. To give it the same
//! completeness, the entrypoint writes that same live `/v1/models` list into
//! `~/.claude/settings.json`'s `availableModels`, which `claude-agent-acp`
//! consumes as an allowlist and surfaces in its picker. On a failed fetch it
//! leaves the key unset and the adapter falls back to the SDK catalog.
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
         # already present are picked up by pixi as PyPI deps. The workspace is\n\
         # treated as an ENVIRONMENT, not an installable package -- see the\n\
         # self-install strip below. marimo runs INSIDE this env (not borrowed\n\
         # from a read-only nix interpreter as the old uv path did), so an\n\
         # in-cell `pixi add` reaches the live kernel.\n\
         if ! grep -q '\\[tool\\.pixi' pyproject.toml 2>/dev/null; then\n\
         had_pyproject=0\n\
         [ -f pyproject.toml ] && had_pyproject=1\n\
         pixi init --format pyproject .\n\
         # pixi init also registers the workspace itself as an editable PyPI\n\
         # package (`<name> = {{ path = \".\", editable = true }}`), which makes\n\
         # `pixi install` BUILD /workspace as a python package. Most workspaces\n\
         # aren't one, and the build backend then kills provisioning (\"unable to\n\
         # determine which files to ship\"). The env doesn't need it either way:\n\
         # pixi picks up [project.dependencies] directly. Drop the dep pixi just\n\
         # added ([tool.pixi] was absent a moment ago, so it can't be the user's).\n\
         sed -i '/^[^ ]* = {{ path = \"\\.\", editable = true }}$/d' pyproject.toml\n\
         # On a fresh pyproject, pixi also scaffolds src/workspace/__init__.py\n\
         # for that self-install; remove the now-pointless empty stub (rmdir\n\
         # only reaps the dirs if nothing else lives there).\n\
         if [ \"$had_pyproject\" = 0 ] && [ ! -s src/workspace/__init__.py ]; then\n\
         rm -f src/workspace/__init__.py\n\
         rmdir src/workspace src 2>/dev/null || true\n\
         fi\n\
         fi\n\
         # Recover workspaces poisoned by a pixi init that predates the strip\n\
         # above: an editable self-dep named \"workspace\" (always the generated\n\
         # name -- the mount point) whose package dir is gone can never build.\n\
         # Only that exact generated line is dropped, and only when the build is\n\
         # guaranteed to fail.\n\
         if grep -q '^workspace = {{ path = \"\\.\", editable = true }}$' pyproject.toml 2>/dev/null \\\n\
         && [ ! -e workspace/__init__.py ] && [ ! -e src/workspace/__init__.py ]; then\n\
         echo 'claude-sandboxed: dropping stale editable self-install of /workspace (package dir missing; pixi install would fail)' >&2\n\
         sed -i '/^workspace = {{ path = \"\\.\", editable = true }}$/d' pyproject.toml\n\
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
         #     then connects to the claude-agent-acp bridge at ws://localhost:{acp_port}.\n\
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
         # The advertised model list is fetched live from /v1/models at startup:\n\
         # marimo cannot list models from an OpenAI-compatible provider itself\n\
         # (Anthropic's endpoint wants an `anthropic-version` header the openai\n\
         # client never sends), and a hardcoded list goes stale as models ship.\n\
         # The python below is flat (single-line suites) because this template\n\
         # strips leading whitespace; it degrades to a static fallback list if\n\
         # the listing fails, and to an AI-less config if creds are missing.\n\
         if [ ! -f \"$HOME/.config/marimo/marimo.toml\" ]; then\n\
         mkdir -p \"$HOME/.config/marimo\"\n\
         printf '[experimental]\\nexternal_agents = true\\n\\n[package_management]\\nmanager = \"pixi\"\\n' > \"$HOME/.config/marimo/marimo.toml\"\n\
         python - <<'MARIMO_AI' >> \"$HOME/.config/marimo/marimo.toml\" || true\n\
         import json, os, sys, urllib.request\n\
         try: tok = json.load(open(os.path.expanduser(\"~/.claude/.credentials.json\")))[\"claudeAiOauth\"][\"accessToken\"]\n\
         except Exception: sys.stderr.write(\"claude-sandboxed: no proxy creds; skipping marimo AI provider config\\n\"); sys.exit(0)\n\
         base = os.environ.get(\"ANTHROPIC_BASE_URL\", \"\")\n\
         if not base: sys.stderr.write(\"claude-sandboxed: ANTHROPIC_BASE_URL unset; skipping marimo AI provider config\\n\"); sys.exit(0)\n\
         models = [\"claude-opus-4-8\", \"claude-sonnet-4-6\", \"claude-haiku-4-5-20251001\"]\n\
         req = urllib.request.Request(base + \"/v1/models?limit=100\")\n\
         req.add_header(\"Authorization\", \"Bearer \" + tok)\n\
         req.add_header(\"anthropic-version\", \"2023-06-01\")\n\
         try: models = list(m[\"id\"] for m in json.load(urllib.request.urlopen(req, timeout=15))[\"data\"]) or models\n\
         except Exception as e: sys.stderr.write(\"claude-sandboxed: model listing failed (%s); advertising fallback models\\n\" % e)\n\
         default = next((m for m in models if m.startswith(\"claude-sonnet\")), models[0])\n\
         cm = \", \".join('\"claude-proxy/%s\"' % m for m in models)\n\
         sys.stdout.write('\\n[ai.models]\\nchat_model = \"claude-proxy/%s\"\\nedit_model = \"claude-proxy/%s\"\\ncustom_models = [%s]\\n\\n[ai.custom_providers.claude-proxy]\\napi_key = \"%s\"\\nbase_url = \"%s/v1/\"\\n' % (default, default, cm, tok, base))\n\
         MARIMO_AI\n\
         fi\n\
         # Seed the ACP agent panel's model picker from the SAME live /v1/models\n\
         # list the chat provider uses, so it lists every model the account\n\
         # exposes instead of the sidecar SDK's bundled (and eventually stale)\n\
         # catalog. claude-agent-acp reads settings.json's `availableModels` as an\n\
         # allowlist and surfaces exactly those ids, each shown by its own id to\n\
         # match the chat picker (the adapter's fuzzy name/description relabeling\n\
         # is patched out in container.nix; effort levels are still inherited from\n\
         # the SDK match). A `default` entry is always kept. Only written when the\n\
         # key is absent (a host/user-set allowlist wins) and only on a\n\
         # successful fetch -- otherwise the adapter falls back to its SDK\n\
         # catalog. Reuses the proxy token + creds plumbing\n\
         # from the block above; the adapter watches settings.json, so this is\n\
         # picked up whether it lands before or after the sidecar starts.\n\
         python - <<'ACP_MODELS' || true\n\
         import json, os, sys, urllib.request\n\
         sp = os.path.expanduser(\"~/.claude/settings.json\")\n\
         try: tok = json.load(open(os.path.expanduser(\"~/.claude/.credentials.json\")))[\"claudeAiOauth\"][\"accessToken\"]\n\
         except Exception: sys.stderr.write(\"claude-sandboxed: no proxy creds; leaving agent-panel models to the sidecar SDK catalog\\n\"); sys.exit(0)\n\
         base = os.environ.get(\"ANTHROPIC_BASE_URL\", \"\")\n\
         if not base: sys.exit(0)\n\
         try: settings = json.load(open(sp))\n\
         except Exception: settings = dict()\n\
         if not isinstance(settings, dict) or \"availableModels\" in settings: sys.exit(0)\n\
         req = urllib.request.Request(base + \"/v1/models?limit=100\")\n\
         req.add_header(\"Authorization\", \"Bearer \" + tok)\n\
         req.add_header(\"anthropic-version\", \"2023-06-01\")\n\
         try: ids = list(m[\"id\"] for m in json.load(urllib.request.urlopen(req, timeout=15))[\"data\"])\n\
         except Exception as e: sys.stderr.write(\"claude-sandboxed: agent-panel model listing failed (%s); using sidecar SDK catalog\\n\" % e); sys.exit(0)\n\
         if not ids: sys.exit(0)\n\
         settings[\"availableModels\"] = ids\n\
         try: json.dump(settings, open(sp, \"w\"), indent=2)\n\
         except Exception as e: sys.stderr.write(\"claude-sandboxed: could not write availableModels to settings.json (%s)\\n\" % e)\n\
         ACP_MODELS\n\
         # ACP sidecar: bridge claude-agent-acp's stdio onto a WebSocket the\n\
         # browser-side marimo agent panel auto-connects to at\n\
         # ws://<host>:{acp_port}/message. Resolve both vendored binaries up\n\
         # front so a missing/renamed binary fails LOUDLY here instead of\n\
         # silently dying in the background (a backgrounded crash under `set -e`\n\
         # is invisible and leaves marimo's panel stuck on \"connect to an\n\
         # agent\"). PATH still contains the nix /bin after the shell-hook's\n\
         # prepend, so command -v resolves the store symlinks.\n\
         acp_bridge=\"$(command -v stdio-to-ws || true)\"\n\
         acp_agent=\"$(command -v claude-agent-acp || true)\"\n\
         if [ -z \"$acp_bridge\" ] || [ -z \"$acp_agent\" ]; then\n\
         echo \"claude-sandboxed: ERROR: ACP bridge missing (stdio-to-ws='$acp_bridge' claude-agent-acp='$acp_agent'); marimo agent panel will not connect.\" >&2\n\
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
        assert!(s.contains("command -v claude-agent-acp"));
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
        // The workspace is never installed into the env as an editable
        // package: pixi init's self-dep is stripped right after init, the
        // fresh-pyproject scaffold stub is removed, and a stale self-dep
        // from a pre-strip provisioning is dropped when its package dir is
        // gone (the wheel build could never succeed).
        assert!(s.contains(r#"sed -i '/^[^ ]* = { path = "\.", editable = true }$/d' pyproject.toml"#));
        assert!(s.contains("rm -f src/workspace/__init__.py"));
        assert!(s.contains("rmdir src/workspace src 2>/dev/null || true"));
        assert!(s.contains(r#"grep -q '^workspace = { path = "\.", editable = true }$' pyproject.toml"#));
        assert!(s.contains("dropping stale editable self-install"));
        // Both strips happen before the env is solved.
        let strip = s.find(r#"sed -i '/^workspace = "#).unwrap();
        assert!(strip < s.find("\npixi install\n").unwrap());
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
        // provider seeded from the stub token + ANTHROPIC_BASE_URL, model list
        // fetched live from /v1/models (with the anthropic-version header the
        // openai client can't send). Missing creds degrade to a working
        // notebook without AI; a failed listing degrades to fallback models.
        assert!(s.contains("pixi add --pypi openai"));
        assert!(s.contains("[ai.custom_providers.claude-proxy]"));
        assert!(s.contains("api_key = \"%s\""));
        assert!(s.contains("base_url = \"%s/v1/\""));
        assert!(s.contains("edit_model = \"claude-proxy/%s\""));
        assert!(s.contains("/v1/models?limit=100"));
        assert!(s.contains("anthropic-version"));
        assert!(s.contains("claudeAiOauth"));
        assert!(s.contains("skipping marimo AI provider config"));
        assert!(s.contains("advertising fallback models"));
        // Agent panel model picker: the same live /v1/models list is written to
        // settings.json's `availableModels` (the claude-agent-acp allowlist), so
        // the picker matches the chat provider's completeness. Merged into the
        // seeded settings.json, only when the key is absent, degrading to the
        // sidecar SDK catalog on a failed fetch.
        assert!(s.contains(r#"settings["availableModels"] = ids"#));
        assert!(s.contains(r#""availableModels" in settings"#));
        assert!(s.contains(".claude/settings.json"));
        assert!(s.contains("using sidecar SDK catalog"));
        // Written before the sidecar starts (the adapter also watches the file,
        // but ordering it first avoids a first-session race).
        let avail = s.find(r#"settings["availableModels"] = ids"#).unwrap();
        assert!(avail < s.find("stdio-to-ws").unwrap());
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


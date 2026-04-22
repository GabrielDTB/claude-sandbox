//! Documentation drift tests: assertions that keep `README.md` in lockstep
//! with the actual code surface.
//!
//! These tests are pure string/filesystem comparisons with zero runtime
//! dependencies — they run as part of `cargo test -p claude-sandboxed` and
//! fail loudly the first time a flag is added (or a file is renamed) without
//! a matching README update.
//!
//! The patterns this module enforces:
//!
//! 1. Every `#[arg(long = "...")]` on `Cli` has a matching row in the
//!    "## `claude-sandboxed` CLI" flag table.
//! 2. Every `.rs` file under the two crate `src/` dirs appears in the
//!    "## Project layout" ASCII tree, and vice versa.

#![cfg(test)]

use std::collections::{BTreeMap, BTreeSet};

/// README.md lives at the repo root. From `crates/claude-sandboxed/src/`
/// that's three levels up. `include_str!` is resolved at compile time
/// relative to this file, so this path is checked by the compiler itself —
/// if the file moves, the build breaks loudly before any test runs.
const README: &str = include_str!("../../../README.md");

// ---------------------------------------------------------------------------
// Flag table ↔ clap CLI surface
// ---------------------------------------------------------------------------

/// Extract every `--long-flag` referenced in the first column of the flag
/// table under the "## `claude-sandboxed` CLI" heading.
///
/// Strategy: find the fenced flag-table block (rows starting with `|`),
/// look at each data row (skip the header and the `| --- | --- | --- |`
/// separator), and pull every backtick-quoted token that starts with `--`
/// out of the first column. A single cell can list more than one flag
/// (`--copy-git` / `--no-copy-git`) — all of them are collected.
fn readme_long_flags() -> BTreeSet<String> {
    // The flag table is the first markdown table inside the
    // `## `claude-sandboxed` CLI` section. Anchor on the heading text so
    // other tables in the README (e.g. the flake-outputs table) can't be
    // picked up by accident.
    let section_start = README
        .find("## `claude-sandboxed` CLI")
        .expect("README missing the `claude-sandboxed` CLI section");
    let section = &README[section_start..];
    // End the section at the next `---` horizontal rule or the next `## `
    // heading, whichever comes first.
    let section_end = section[1..]
        .find("\n## ")
        .map(|i| i + 1)
        .unwrap_or(section.len());
    let section = &section[..section_end];

    let mut flags = BTreeSet::new();
    for line in section.lines() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with('|') {
            continue;
        }
        // Skip the header row and the `| --- | --- | --- |` separator.
        // We detect both by checking whether the first cell contains any
        // code-span — only data rows do, header/separator don't.
        let first_cell = trimmed
            .trim_start_matches('|')
            .split('|')
            .next()
            .unwrap_or("");
        if !first_cell.contains('`') {
            continue;
        }
        // Walk the cell picking up every backtick-quoted token that starts
        // with `--`.
        let mut rest = first_cell;
        while let Some(open) = rest.find('`') {
            rest = &rest[open + 1..];
            let Some(close) = rest.find('`') else { break };
            let tok = &rest[..close];
            rest = &rest[close + 1..];
            if let Some(flag_name) = tok.strip_prefix("--") {
                // The cell writes tokens like `--bind SRC:DST` — strip the
                // value placeholder, keep just the flag name.
                let flag = flag_name
                    .split_whitespace()
                    .next()
                    .unwrap_or(flag_name)
                    .to_string();
                if !flag.is_empty() {
                    flags.insert(format!("--{flag}"));
                }
            }
        }
    }
    flags
}

/// Every `#[arg(long = "...")]` on `Cli`, as the same `--flag` string form
/// the README uses. Positional args (no `long`) are filtered out by
/// `get_long()` returning `None`.
fn clap_long_flags() -> BTreeSet<String> {
    use clap::CommandFactory;
    let cmd = <crate::cli::Cli as CommandFactory>::command();
    cmd.get_arguments()
        .filter_map(|a| a.get_long().map(|s| format!("--{s}")))
        .collect()
}

#[test]
fn readme_flag_table_matches_clap_cli() {
    let readme = readme_long_flags();
    let clap = clap_long_flags();

    let missing_from_readme: Vec<&String> = clap.difference(&readme).collect();
    let missing_from_clap: Vec<&String> = readme.difference(&clap).collect();

    if !missing_from_readme.is_empty() || !missing_from_clap.is_empty() {
        let mut msg = String::from("README flag table and clap CLI drifted:\n");
        if !missing_from_readme.is_empty() {
            msg.push_str("  present in clap, MISSING from README table:\n");
            for f in &missing_from_readme {
                msg.push_str(&format!("    {f}\n"));
            }
        }
        if !missing_from_clap.is_empty() {
            msg.push_str("  present in README table, MISSING from clap:\n");
            for f in &missing_from_clap {
                msg.push_str(&format!("    {f}\n"));
            }
        }
        panic!("{msg}");
    }
}

// ---------------------------------------------------------------------------
// Project layout tree ↔ filesystem
// ---------------------------------------------------------------------------

/// Parse the "## Project layout" ASCII tree and return, per crate, the set
/// of `.rs` files listed under it.
///
/// The tree is a plain fenced code block. We walk it line by line; a line
/// whose first non-tree-drawing token matches `<crate>/` (where `<crate>`
/// starts with `claude-`) switches the "current crate" state, and subsequent
/// `.rs` lines are attributed to that crate until the next crate marker.
/// A `.rs` line encountered before any crate marker is a bug in the tree
/// (or in this parser) and panics — no silent bucket.
///
/// Inline annotations after the filename (`main.rs   # entrypoint, ...`)
/// are ignored by taking only the first whitespace-separated token.
fn readme_tree_rs_files_per_crate() -> BTreeMap<String, BTreeSet<String>> {
    let section_start = README
        .find("## Project layout")
        .expect("README missing the Project layout section");
    let after_heading = &README[section_start..];
    let fence_open = after_heading
        .find("```")
        .expect("Project layout section missing its code fence");
    let body = &after_heading[fence_open + 3..];
    let body_start = body.find('\n').map(|i| i + 1).unwrap_or(0);
    let body = &body[body_start..];
    let fence_close = body
        .find("```")
        .expect("Project layout code fence never closes");
    let body = &body[..fence_close];

    let mut result: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut current_crate: Option<String> = None;

    for line in body.lines() {
        // Strip tree-drawing characters, leading whitespace, and bullet glyphs.
        let content = line.trim_start_matches(|c: char| {
            c.is_whitespace()
                || c == '│'
                || c == '├'
                || c == '└'
                || c == '─'
                || c == '|'
                || c == '`'
        });
        let Some(first_tok) = content.split_whitespace().next() else {
            continue;
        };

        // Crate-directory marker: `claude-<name>/`. Any other `*/` token
        // (e.g. `src/`) is ignored — we key only on the crate boundary.
        if let Some(dir_name) = first_tok.strip_suffix('/') {
            if dir_name.starts_with("claude-") {
                current_crate = Some(dir_name.to_string());
                result.entry(dir_name.to_string()).or_default();
                continue;
            }
        }

        // `.rs` file: attribute to the current crate.
        if let Some(base) = first_tok.strip_suffix(".rs") {
            // Guard against a stray match on a token that contains `.rs`
            // but isn't a bare filename (the token is already the first
            // whitespace-separated chunk, so this is belt-and-braces).
            if base.is_empty() || base.contains('/') {
                continue;
            }
            let filename = format!("{base}.rs");
            let Some(owner) = current_crate.as_ref() else {
                panic!(
                    "README project-layout tree lists `{filename}` outside any \
                     crate subsection — the parser expects every .rs line to \
                     appear beneath a `claude-<name>/` directory marker"
                );
            };
            result
                .get_mut(owner)
                .expect("current_crate entry was inserted when we set it")
                .insert(filename);
        }
    }

    result
}

/// Enumerate every crate under `crates/` and return a map of crate name →
/// set of `.rs` files in that crate's `src/`.
///
/// Discovery is dynamic: we walk `CARGO_MANIFEST_DIR/..` (the `crates/` dir)
/// and include every subdirectory that has a `src/`. So adding a new crate
/// to the workspace automatically enrolls it into this drift check with no
/// edit to this function — forcing the README tree to pick it up too.
fn fs_rs_files_per_crate() -> BTreeMap<String, BTreeSet<String>> {
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let crates_dir = manifest_dir
        .parent()
        .expect("CARGO_MANIFEST_DIR has a parent (the crates/ dir)");

    let mut result = BTreeMap::new();
    for entry in std::fs::read_dir(crates_dir)
        .unwrap_or_else(|e| panic!("reading {}: {e}", crates_dir.display()))
    {
        let entry = entry.expect("bad dir entry");
        let path = entry.path();
        if !entry.file_type().expect("file_type").is_dir() {
            continue;
        }
        let src_dir = path.join("src");
        if !src_dir.is_dir() {
            continue;
        }
        let Some(crate_name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        result.insert(crate_name.to_string(), rs_files_in_dir(&src_dir));
    }
    result
}

/// List every `.rs` file (basename only) in `dir`.
fn rs_files_in_dir(dir: &std::path::Path) -> BTreeSet<String> {
    let mut files = BTreeSet::new();
    for entry in
        std::fs::read_dir(dir).unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()))
    {
        let entry = entry.expect("bad dir entry");
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("rs") {
            if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                files.insert(name.to_string());
            }
        }
    }
    files
}

#[test]
fn readme_project_layout_matches_filesystem() {
    let readme = readme_tree_rs_files_per_crate();
    let fs = fs_rs_files_per_crate();

    let mut msg = String::new();

    // Crates listed in one but not the other.
    let readme_crates: BTreeSet<&String> = readme.keys().collect();
    let fs_crates: BTreeSet<&String> = fs.keys().collect();
    for crate_name in fs_crates.difference(&readme_crates) {
        msg.push_str(&format!(
            "  crate present on disk, MISSING from README tree: {crate_name}\n"
        ));
    }
    for crate_name in readme_crates.difference(&fs_crates) {
        msg.push_str(&format!(
            "  crate present in README tree, MISSING on disk: {crate_name}\n"
        ));
    }

    // For crates in both, per-crate file diff.
    for crate_name in readme_crates.intersection(&fs_crates) {
        let r = &readme[*crate_name];
        let f = &fs[*crate_name];
        let missing_from_readme: Vec<&String> = f.difference(r).collect();
        let missing_from_fs: Vec<&String> = r.difference(f).collect();
        if !missing_from_readme.is_empty() {
            msg.push_str(&format!(
                "  [{crate_name}] present on disk, MISSING from README tree:\n"
            ));
            for file in &missing_from_readme {
                msg.push_str(&format!("    {file}\n"));
            }
        }
        if !missing_from_fs.is_empty() {
            msg.push_str(&format!(
                "  [{crate_name}] present in README tree, MISSING on disk:\n"
            ));
            for file in &missing_from_fs {
                msg.push_str(&format!("    {file}\n"));
            }
        }
    }

    if !msg.is_empty() {
        panic!("README project-layout tree drifted from src/*.rs:\n{msg}");
    }
}

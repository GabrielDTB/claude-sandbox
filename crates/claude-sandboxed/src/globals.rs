//! Inherited "globals" — skills and memory files shared across sandboxes.
//!
//! Host layout:
//!
//! ```text
//! $XDG_DATA_HOME/claude-sandboxed/
//!   skills/<tag>/<subtag>/.../<file>
//!   memory/<tag>/<subtag>/.../<file>
//! ```
//!
//! The directory chain between the kind root (`skills/` or `memory/`) and
//! the file becomes the file's implicit tag — e.g. `skills/languages/python/typing.md`
//! carries the tag `languages/python`. Tag matching is prefix-at-segment-boundary:
//! `languages` matches `languages/python` but not `languages-extended`.
//!
//! ## Layered configuration
//!
//! Selection is assembled from up to three layers (from outermost to
//! innermost):
//!
//! 1. top-level `[skills]` / `[memory]` — defaults for every launch
//! 2. profile-level `[profiles.<name>]` — shared across both kinds
//! 3. profile-kind-level `[profiles.<name>.skills]` / `[profiles.<name>.memory]`
//!
//! At each layer, `tags` and `extra_files` **override** the resolved value
//! from above (last-specified wins), while `extra_tags` and
//! `extra_extra_files` are always **unioned** with whatever came before.
//! CLI flags then stack additively on top of the resolved config values.
//!
//! Per-file (not per-directory) bind mounts are intentional: the containing
//! directories remain the sandbox's rw `.claude` mount, so the agent can
//! still create siblings — only the inherited files themselves are read-only.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;

/// One layer of tag/file selection for a single kind (skills or memory).
///
/// `tags` and `extra_files` have OVERRIDE semantics (`None` means "inherit
/// from above", `Some(v)` replaces — even `Some(vec![])` clears).
/// `extra_tags` and `extra_extra_files` have ADDITIVE semantics — they are
/// always unioned with whatever layer(s) above contributed.
#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Section {
    /// Override. Replaces the inherited tag list entirely when present.
    pub tags: Option<Vec<String>>,
    /// Additive. Always unioned on top of the resolved `tags`.
    #[serde(default)]
    pub extra_tags: Vec<String>,
    /// Override. Paths of individual files to include, relative to the kind's
    /// content directory (e.g. `misc/readme-style.md` under `skills/`).
    /// Absolute paths and `..` components are rejected at resolve time.
    pub extra_files: Option<Vec<PathBuf>>,
    /// Additive. Always unioned on top of the resolved `extra_files`.
    #[serde(default)]
    pub extra_extra_files: Vec<PathBuf>,
}

/// A named profile declared in the user config under `[profiles.<name>]`.
///
/// The profile's own `tags`/`extra_tags`/`extra_files`/`extra_extra_files`
/// fields are SHARED between skills and memory — they form the middle
/// layer in the override chain between the top-level `[skills]`/`[memory]`
/// sections and the innermost per-kind `[profiles.<name>.skills]` /
/// `[profiles.<name>.memory]` subsections.
#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// Shared override. Applies to both skills and memory resolution.
    pub tags: Option<Vec<String>>,
    /// Shared additive. Applies to both skills and memory resolution.
    #[serde(default)]
    pub extra_tags: Vec<String>,
    /// Shared override. Applies to both skills and memory resolution.
    pub extra_files: Option<Vec<PathBuf>>,
    /// Shared additive. Applies to both skills and memory resolution.
    #[serde(default)]
    pub extra_extra_files: Vec<PathBuf>,
    pub skills: Option<Section>,
    pub memory: Option<Section>,
}

/// Zero-copy borrow of one layer's selection fields, used internally by the
/// layer-walking resolver so both `Section` and the shared fields of
/// `Profile` can be fed through the same routine.
#[derive(Copy, Clone)]
struct SectionView<'a> {
    tags: Option<&'a [String]>,
    extra_tags: &'a [String],
    extra_files: Option<&'a [PathBuf]>,
    extra_extra_files: &'a [PathBuf],
}

impl Section {
    fn view(&self) -> SectionView<'_> {
        SectionView {
            tags: self.tags.as_deref(),
            extra_tags: &self.extra_tags,
            extra_files: self.extra_files.as_deref(),
            extra_extra_files: &self.extra_extra_files,
        }
    }
}

impl Profile {
    fn shared_view(&self) -> SectionView<'_> {
        SectionView {
            tags: self.tags.as_deref(),
            extra_tags: &self.extra_tags,
            extra_files: self.extra_files.as_deref(),
            extra_extra_files: &self.extra_extra_files,
        }
    }
}

/// Resolved set of files to mount into the sandbox.
///
/// Each entry is `(host_abs_path, relpath_within_kind)`. Relpaths use `/`
/// as the separator (stored as `PathBuf`, emitted as `Display` in the
/// mount target).
#[derive(Debug, Default)]
pub struct Selected {
    pub skills: Vec<(PathBuf, PathBuf)>,
    pub memory: Vec<(PathBuf, PathBuf)>,
}


/// Resolve the globals root: `$XDG_DATA_HOME/claude-sandboxed` with a
/// fallback of `$HOME/.local/share/claude-sandboxed`. Returns `None` when
/// neither is set — matches the same asymmetry `config::config_path` has.
pub fn globals_root() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(xdg).join("claude-sandboxed"));
    }
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(|h| PathBuf::from(h).join(".local").join("share").join("claude-sandboxed"))
}

/// Resolve the union of config layers + CLI additions into concrete files.
///
/// The three config layers for each kind are walked in order
/// (top-level → profile-shared → profile-kind). `tags` / `extra_files`
/// overrides are last-wins; `extra_tags` / `extra_extra_files` are
/// accumulated across every layer. CLI lists are appended last.
///
/// `root` is typically `globals_root().as_deref()`. When it is `None`, or
/// when the `skills/` / `memory/` subdirectory of `root` does not exist,
/// any tag/file requests for that kind are a hard error — if the user
/// asked for something concrete we refuse to silently do nothing. With
/// no requests at all the function returns an empty `Selected`.
#[allow(clippy::too_many_arguments)]
pub fn select(
    root: Option<&Path>,
    top_skills: Option<&Section>,
    top_memory: Option<&Section>,
    profile: Option<&Profile>,
    skill_tags_cli: &[String],
    memory_tags_cli: &[String],
    skill_files_cli: &[PathBuf],
    memory_files_cli: &[PathBuf],
) -> Result<Selected, crate::Error> {
    // Assemble the per-kind layer stacks (outermost first). `flatten()` in
    // `resolve_layers` skips absent layers.
    let profile_shared = profile.map(Profile::shared_view);
    let skill_layers: [Option<SectionView<'_>>; 3] = [
        top_skills.map(Section::view),
        profile_shared,
        profile.and_then(|p| p.skills.as_ref()).map(Section::view),
    ];
    let memory_layers: [Option<SectionView<'_>>; 3] = [
        top_memory.map(Section::view),
        profile_shared,
        profile.and_then(|p| p.memory.as_ref()).map(Section::view),
    ];

    let (mut skill_tags, mut skill_files) = resolve_layers(&skill_layers);
    let (mut memory_tags, mut memory_files) = resolve_layers(&memory_layers);

    // CLI flags are additive — same semantics as `extra_*` accumulators,
    // just applied after all config layers are merged.
    skill_tags.extend(skill_tags_cli.iter().cloned());
    skill_files.extend(skill_files_cli.iter().cloned());
    memory_tags.extend(memory_tags_cli.iter().cloned());
    memory_files.extend(memory_files_cli.iter().cloned());

    for t in skill_tags.iter().chain(memory_tags.iter()) {
        if t.is_empty() {
            return Err("empty tag string is not allowed".into());
        }
    }

    let skill_tag_refs: Vec<&str> = skill_tags.iter().map(String::as_str).collect();
    let skill_file_refs: Vec<&Path> = skill_files.iter().map(PathBuf::as_path).collect();
    let memory_tag_refs: Vec<&str> = memory_tags.iter().map(String::as_str).collect();
    let memory_file_refs: Vec<&Path> = memory_files.iter().map(PathBuf::as_path).collect();

    let skills = resolve_kind(root, "skills", &skill_tag_refs, &skill_file_refs)?;
    let memory = resolve_kind(root, "memory", &memory_tag_refs, &memory_file_refs)?;

    Ok(Selected { skills, memory })
}

/// Reduce a stack of layers into a single `(tags, files)` pair.
///
/// Pass 1 resolves override fields (`tags`, `extra_files`) — each present
/// layer replaces whatever was accumulated so far, so the deepest-specified
/// value wins. Pass 2 accumulates the additive fields (`extra_tags`,
/// `extra_extra_files`) in layer order. Per-layer absence (`None` in the
/// outer slice) means the layer doesn't contribute at all.
fn resolve_layers(layers: &[Option<SectionView<'_>>]) -> (Vec<String>, Vec<PathBuf>) {
    let mut tags: Vec<String> = Vec::new();
    let mut files: Vec<PathBuf> = Vec::new();
    for layer in layers.iter().flatten() {
        if let Some(t) = layer.tags {
            tags = t.to_vec();
        }
        if let Some(f) = layer.extra_files {
            files = f.to_vec();
        }
    }
    for layer in layers.iter().flatten() {
        tags.extend(layer.extra_tags.iter().cloned());
        files.extend(layer.extra_extra_files.iter().cloned());
    }
    (tags, files)
}

fn resolve_kind(
    root: Option<&Path>,
    kind: &str,
    tags: &[&str],
    extra_files: &[&Path],
) -> Result<Vec<(PathBuf, PathBuf)>, crate::Error> {
    if tags.is_empty() && extra_files.is_empty() {
        return Ok(Vec::new());
    }
    let Some(root) = root else {
        return Err(format!(
            "cannot resolve {kind} globals: no $XDG_DATA_HOME or $HOME in environment"
        )
        .into());
    };
    let kind_root = root.join(kind);
    if !kind_root.is_dir() {
        return Err(format!(
            "cannot resolve {kind} globals: {} is not a directory",
            kind_root.display()
        )
        .into());
    }

    // Dedupe via BTreeMap keyed on relpath → host_path. BTreeMap keeps the
    // output deterministic and stable (alphabetical by relpath).
    let mut out: BTreeMap<PathBuf, PathBuf> = BTreeMap::new();

    if !tags.is_empty() {
        walk_and_match(&kind_root, &kind_root, tags, &mut out)?;
    }

    for rel in extra_files {
        validate_relpath(rel, kind)?;
        let host = kind_root.join(rel);
        if !host.is_file() {
            return Err(format!(
                "{kind} extra_file not found or not a regular file: {} (under {})",
                rel.display(),
                kind_root.display()
            )
            .into());
        }
        out.insert(rel.to_path_buf(), host);
    }

    Ok(out.into_iter().map(|(rel, host)| (host, rel)).collect())
}

/// Recursively walk `dir`, matching files against `tags` (prefix-at-segment-boundary)
/// and inserting hits into `out` keyed on their relpath from `root`.
///
/// Symlinked directories are skipped to avoid escaping `root`. Symlinked
/// regular files are followed (they can only name something accessible to
/// the user, same as a hardlink would).
fn walk_and_match(
    root: &Path,
    dir: &Path,
    tags: &[&str],
    out: &mut BTreeMap<PathBuf, PathBuf>,
) -> Result<(), crate::Error> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| -> crate::Error { format!("reading {}: {e}", dir.display()).into() })?;
    for entry in entries {
        let entry = entry
            .map_err(|e| -> crate::Error { format!("iterating {}: {e}", dir.display()).into() })?;
        let path = entry.path();
        // `file_type()` on an entry inspects the symlink itself; follow manually
        // only for regular files (skip symlinked dirs).
        let ft = entry
            .file_type()
            .map_err(|e| -> crate::Error { format!("stat {}: {e}", path.display()).into() })?;
        if ft.is_dir() {
            walk_and_match(root, &path, tags, out)?;
        } else if ft.is_symlink() {
            // Only follow if the target is a regular file.
            match std::fs::metadata(&path) {
                Ok(m) if m.is_file() => {
                    insert_if_match(root, &path, tags, out)?;
                }
                _ => {} // broken symlink or symlinked dir — skip.
            }
        } else if ft.is_file() {
            insert_if_match(root, &path, tags, out)?;
        }
    }
    Ok(())
}

fn insert_if_match(
    root: &Path,
    file: &Path,
    tags: &[&str],
    out: &mut BTreeMap<PathBuf, PathBuf>,
) -> Result<(), crate::Error> {
    let rel = file
        .strip_prefix(root)
        .map_err(|_| -> crate::Error {
            format!(
                "walk produced path {} outside root {}",
                file.display(),
                root.display()
            )
            .into()
        })?;
    let dir_chain = rel
        .parent()
        .map(|p| p.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/"))
        .unwrap_or_default();
    if tags.iter().any(|t| tag_matches(&dir_chain, t)) {
        out.insert(rel.to_path_buf(), file.to_path_buf());
    }
    Ok(())
}

/// Prefix-at-segment-boundary match.
///
/// `dir_chain` is the slash-joined directory chain from the kind root to
/// the file (e.g. `languages/python` for a file at `skills/languages/python/typing.md`).
/// Returns true iff `dir_chain == tag` or `dir_chain` starts with `tag/`.
fn tag_matches(dir_chain: &str, tag: &str) -> bool {
    if dir_chain == tag {
        return true;
    }
    if let Some(rest) = dir_chain.strip_prefix(tag) {
        return rest.starts_with('/');
    }
    false
}

/// Reject absolute paths and any `..` / `.` components in an `extra_files` entry.
fn validate_relpath(rel: &Path, kind: &str) -> Result<(), crate::Error> {
    if rel.is_absolute() {
        return Err(format!(
            "{kind} extra_file must be relative, got absolute path: {}",
            rel.display()
        )
        .into());
    }
    for c in rel.components() {
        match c {
            Component::Normal(_) => {}
            Component::CurDir => {}
            _ => {
                return Err(format!(
                    "{kind} extra_file must not contain `..` or root components: {}",
                    rel.display()
                )
                .into());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // ---- Tag-match primitive ------------------------------------------------

    #[test]
    fn tag_match_exact() {
        assert!(tag_matches("languages/python", "languages/python"));
    }

    #[test]
    fn tag_match_prefix_at_boundary() {
        assert!(tag_matches("languages/python", "languages"));
        assert!(tag_matches("languages/python/typing", "languages/python"));
    }

    #[test]
    fn tag_match_not_mid_segment() {
        assert!(!tag_matches("languages-extended", "languages"));
        assert!(!tag_matches("lang", "languages"));
        assert!(!tag_matches("languages", "languages/python"));
    }

    #[test]
    fn tag_match_empty_dir_chain() {
        // A file directly in `skills/` has no tag — nothing should match
        // unless the caller supplied an empty tag, which select() rejects.
        assert!(!tag_matches("", "any"));
    }

    // ---- `select` behavior --------------------------------------------------

    fn select_simple(
        root: Option<&Path>,
        profile: Option<&Profile>,
        skill_tags: &[&str],
        skill_files: &[&str],
    ) -> Result<Selected, crate::Error> {
        let tags: Vec<String> = skill_tags.iter().map(|s| s.to_string()).collect();
        let files: Vec<PathBuf> = skill_files.iter().map(PathBuf::from).collect();
        select(root, None, None, profile, &tags, &[], &files, &[])
    }

    #[test]
    fn empty_tag_rejected() {
        let err = select_simple(None, None, &[""], &[]).unwrap_err().to_string();
        assert!(err.contains("empty tag"), "got: {err}");
    }

    #[test]
    fn nothing_requested_is_empty_noop_even_without_root() {
        let s = select_simple(None, None, &[], &[]).unwrap();
        assert!(s.skills.is_empty() && s.memory.is_empty());
    }

    #[test]
    fn request_without_root_errors() {
        let err = select_simple(None, None, &["foo"], &[]).unwrap_err().to_string();
        assert!(err.contains("no $XDG_DATA_HOME"), "got: {err}");
    }

    fn fixture() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let skills = tmp.path().join("skills");
        fs::create_dir_all(skills.join("languages/python")).unwrap();
        fs::create_dir_all(skills.join("languages/rust")).unwrap();
        fs::create_dir_all(skills.join("cli/clap")).unwrap();
        fs::create_dir_all(skills.join("misc")).unwrap();
        fs::write(skills.join("languages/python/typing.md"), "py typing").unwrap();
        fs::write(skills.join("languages/rust/traits.md"), "rust traits").unwrap();
        fs::write(skills.join("cli/clap/derive.md"), "clap derive").unwrap();
        fs::write(skills.join("misc/readme.md"), "readme").unwrap();
        let memory = tmp.path().join("memory");
        fs::create_dir_all(memory.join("python/testing")).unwrap();
        fs::write(memory.join("python/testing/pytest.md"), "pytest").unwrap();
        tmp
    }

    fn relpaths(v: &[(PathBuf, PathBuf)]) -> Vec<String> {
        v.iter().map(|(_, r)| r.to_string_lossy().replace('\\', "/").to_string()).collect()
    }

    #[test]
    fn walk_selects_tag_subtree() {
        let tmp = fixture();
        let s = select_simple(Some(tmp.path()), None, &["languages"], &[]).unwrap();
        let got = relpaths(&s.skills);
        assert_eq!(
            got,
            vec!["languages/python/typing.md", "languages/rust/traits.md"]
        );
        assert!(s.memory.is_empty());
    }

    #[test]
    fn exact_tag_only_matches_that_subtree() {
        let tmp = fixture();
        let s = select_simple(Some(tmp.path()), None, &["languages/python"], &[]).unwrap();
        assert_eq!(relpaths(&s.skills), vec!["languages/python/typing.md"]);
    }

    #[test]
    fn extra_files_resolved_and_deduped_against_tags() {
        let tmp = fixture();
        let s = select_simple(
            Some(tmp.path()),
            None,
            &["languages/python"],
            &["languages/python/typing.md", "misc/readme.md"],
        )
        .unwrap();
        let got = relpaths(&s.skills);
        // typing.md came from both the tag walk and extra_files; must appear once.
        assert_eq!(got, vec!["languages/python/typing.md", "misc/readme.md"]);
    }

    #[test]
    fn missing_extra_file_errors() {
        let tmp = fixture();
        let err = select_simple(Some(tmp.path()), None, &[], &["does/not/exist.md"])
            .unwrap_err()
            .to_string();
        assert!(err.contains("not found"), "got: {err}");
    }

    #[test]
    fn absolute_extra_file_rejected() {
        let tmp = fixture();
        let err = select_simple(Some(tmp.path()), None, &[], &["/etc/passwd"])
            .unwrap_err()
            .to_string();
        assert!(err.contains("absolute"), "got: {err}");
    }

    #[test]
    fn parent_dir_traversal_rejected() {
        let tmp = fixture();
        let err = select_simple(Some(tmp.path()), None, &[], &["../outside.md"])
            .unwrap_err()
            .to_string();
        assert!(err.contains(".."), "got: {err}");
    }

    #[test]
    fn missing_kind_dir_with_request_errors() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("skills/foo")).unwrap();
        fs::write(tmp.path().join("skills/foo/x.md"), "").unwrap();
        let err = select(
            Some(tmp.path()),
            None,
            None,
            None,
            &[],
            &["anything".into()],
            &[],
            &[],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("not a directory"), "got: {err}");
    }

    #[test]
    fn missing_kind_dir_without_request_is_fine() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("skills/foo")).unwrap();
        fs::write(tmp.path().join("skills/foo/x.md"), "").unwrap();
        let s = select_simple(Some(tmp.path()), None, &["foo"], &[]).unwrap();
        assert_eq!(relpaths(&s.skills), vec!["foo/x.md"]);
        assert!(s.memory.is_empty());
    }

    // ---- Layered override / extra_tag accumulation --------------------------

    #[test]
    fn top_level_section_applies_when_no_profile() {
        let tmp = fixture();
        let top = Section {
            tags: Some(vec!["languages/python".into()]),
            ..Section::default()
        };
        let s = select(
            Some(tmp.path()),
            Some(&top),
            None,
            None,
            &[],
            &[],
            &[],
            &[],
        )
        .unwrap();
        assert_eq!(relpaths(&s.skills), vec!["languages/python/typing.md"]);
    }

    #[test]
    fn profile_shared_tags_override_top_level_tags() {
        let tmp = fixture();
        let top = Section {
            tags: Some(vec!["languages/python".into()]),
            ..Section::default()
        };
        let prof = Profile {
            tags: Some(vec!["cli".into()]),
            ..Profile::default()
        };
        let s = select(
            Some(tmp.path()),
            Some(&top),
            None,
            Some(&prof),
            &[],
            &[],
            &[],
            &[],
        )
        .unwrap();
        // Profile `tags` replaced top-level `tags` entirely — no python.
        assert_eq!(relpaths(&s.skills), vec!["cli/clap/derive.md"]);
    }

    #[test]
    fn profile_kind_tags_override_profile_shared_tags() {
        let tmp = fixture();
        let prof = Profile {
            tags: Some(vec!["cli".into()]),
            skills: Some(Section {
                tags: Some(vec!["languages/rust".into()]),
                ..Section::default()
            }),
            ..Profile::default()
        };
        let s = select(
            Some(tmp.path()),
            None,
            None,
            Some(&prof),
            &[],
            &[],
            &[],
            &[],
        )
        .unwrap();
        // For skills, profile.skills.tags overrides profile.tags ("cli" dropped).
        assert_eq!(relpaths(&s.skills), vec!["languages/rust/traits.md"]);
    }

    #[test]
    fn profile_kind_absent_inherits_profile_shared_tags_for_that_kind() {
        let tmp = fixture();
        let prof = Profile {
            tags: Some(vec!["cli".into()]),
            // skills layer absent → inherit "cli" from shared
            // memory layer absent → same, but no memory/cli/... → empty
            ..Profile::default()
        };
        let s = select(
            Some(tmp.path()),
            None,
            None,
            Some(&prof),
            &[],
            &[],
            &[],
            &[],
        )
        .unwrap();
        assert_eq!(relpaths(&s.skills), vec!["cli/clap/derive.md"]);
        assert!(s.memory.is_empty());
    }

    #[test]
    fn extra_tags_accumulate_across_all_layers() {
        let tmp = fixture();
        let top = Section {
            extra_tags: vec!["languages/python".into()],
            ..Section::default()
        };
        let prof = Profile {
            extra_tags: vec!["cli".into()],
            skills: Some(Section {
                extra_tags: vec!["languages/rust".into()],
                ..Section::default()
            }),
            ..Profile::default()
        };
        let s = select(
            Some(tmp.path()),
            Some(&top),
            None,
            Some(&prof),
            &[],
            &[],
            &[],
            &[],
        )
        .unwrap();
        assert_eq!(
            relpaths(&s.skills),
            vec![
                "cli/clap/derive.md",
                "languages/python/typing.md",
                "languages/rust/traits.md",
            ]
        );
    }

    #[test]
    fn override_tags_then_accumulate_extra_tags() {
        let tmp = fixture();
        let top = Section {
            tags: Some(vec!["languages".into()]), // both python+rust initially
            ..Section::default()
        };
        let prof = Profile {
            // Profile overrides base tags to just cli/clap, then adds rust via extra_tags.
            skills: Some(Section {
                tags: Some(vec!["cli".into()]),
                extra_tags: vec!["languages/rust".into()],
                ..Section::default()
            }),
            ..Profile::default()
        };
        let s = select(
            Some(tmp.path()),
            Some(&top),
            None,
            Some(&prof),
            &[],
            &[],
            &[],
            &[],
        )
        .unwrap();
        // Top "languages" dropped by profile.skills override; cli + rust remain.
        assert_eq!(
            relpaths(&s.skills),
            vec!["cli/clap/derive.md", "languages/rust/traits.md"]
        );
    }

    #[test]
    fn cli_flags_stack_additively_on_top() {
        let tmp = fixture();
        let prof = Profile {
            skills: Some(Section {
                tags: Some(vec!["languages/python".into()]),
                ..Section::default()
            }),
            ..Profile::default()
        };
        let s = select(
            Some(tmp.path()),
            None,
            None,
            Some(&prof),
            &["cli".into()],
            &[],
            &[],
            &[],
        )
        .unwrap();
        assert_eq!(
            relpaths(&s.skills),
            vec!["cli/clap/derive.md", "languages/python/typing.md"]
        );
    }

    #[test]
    fn extra_files_override_then_extra_extra_files_accumulate() {
        let tmp = fixture();
        let top = Section {
            extra_files: Some(vec![PathBuf::from("misc/readme.md")]),
            extra_extra_files: vec![PathBuf::from("cli/clap/derive.md")],
            ..Section::default()
        };
        let prof = Profile {
            skills: Some(Section {
                // Override drops top's misc/readme.md → only typing.md from override.
                extra_files: Some(vec![PathBuf::from("languages/python/typing.md")]),
                // Additive — layered on top of everyone's extra_extra_files.
                extra_extra_files: vec![PathBuf::from("languages/rust/traits.md")],
                ..Section::default()
            }),
            ..Profile::default()
        };
        let s = select(
            Some(tmp.path()),
            Some(&top),
            None,
            Some(&prof),
            &[],
            &[],
            &[],
            &[],
        )
        .unwrap();
        let got = relpaths(&s.skills);
        assert_eq!(
            got,
            vec![
                "cli/clap/derive.md",            // from top.extra_extra_files
                "languages/python/typing.md",    // from profile.skills.extra_files (override)
                "languages/rust/traits.md",      // from profile.skills.extra_extra_files (add)
            ]
        );
    }

    #[test]
    fn empty_override_tags_clears_inherited() {
        let tmp = fixture();
        let top = Section {
            tags: Some(vec!["languages".into()]),
            ..Section::default()
        };
        let prof = Profile {
            skills: Some(Section {
                tags: Some(vec![]), // explicit empty → clear
                ..Section::default()
            }),
            ..Profile::default()
        };
        let s = select(
            Some(tmp.path()),
            Some(&top),
            None,
            Some(&prof),
            &[],
            &[],
            &[],
            &[],
        )
        .unwrap();
        assert!(s.skills.is_empty());
    }

    #[test]
    fn profile_memory_section_routes_to_memory_only() {
        let tmp = fixture();
        let prof = Profile {
            memory: Some(Section {
                tags: Some(vec!["python/testing".into()]),
                ..Section::default()
            }),
            ..Profile::default()
        };
        let s = select(
            Some(tmp.path()),
            None,
            None,
            Some(&prof),
            &[],
            &[],
            &[],
            &[],
        )
        .unwrap();
        assert!(s.skills.is_empty());
        assert_eq!(relpaths(&s.memory), vec!["python/testing/pytest.md"]);
    }

    #[test]
    fn profile_shared_tags_apply_to_both_kinds() {
        let tmp = fixture();
        let prof = Profile {
            // Shared tag "cli" matches skills/cli but no memory/cli exists.
            // And "python/testing" matches memory but no skills/python/testing
            // either — each layer is a pure union on its kind's tree.
            extra_tags: vec!["cli".into(), "python/testing".into()],
            ..Profile::default()
        };
        let s = select(
            Some(tmp.path()),
            None,
            None,
            Some(&prof),
            &[],
            &[],
            &[],
            &[],
        )
        .unwrap();
        assert_eq!(relpaths(&s.skills), vec!["cli/clap/derive.md"]);
        assert_eq!(relpaths(&s.memory), vec!["python/testing/pytest.md"]);
    }
}

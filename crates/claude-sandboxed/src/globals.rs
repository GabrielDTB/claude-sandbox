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
//! Profiles in the user config name a set of tags (and optional explicit
//! files) per kind; callers can also mix in CLI-level tags/files additively.
//! [`select`] resolves the union into concrete (host_path, relpath) pairs
//! which `run.rs` then mounts one-by-one, read-only, into
//! `/home/user/.claude/{skills,memory}/<relpath>`.
//!
//! Per-file (not per-directory) bind mounts are intentional: the containing
//! directories remain the sandbox's rw `.claude` mount, so the agent can
//! still create siblings — only the inherited files themselves are read-only.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;

/// A named profile declared in the user config under `[profiles.<name>]`.
///
/// The shared `tags` apply to both skills and memory; per-kind `skills` /
/// `memory` subsections can add more tags and explicit files on top.
#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// Tags applied to both skills and memory.
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub skills: Option<Section>,
    #[serde(default)]
    pub memory: Option<Section>,
}

/// Per-kind subsection of a profile (`[profiles.<name>.skills]` or
/// `[profiles.<name>.memory]`).
#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Section {
    #[serde(default)]
    pub tags: Vec<String>,
    /// Paths of individual files to include, relative to the kind's content
    /// directory (e.g. `misc/readme-style.md` under `skills/`). Absolute
    /// paths and `..` components are rejected.
    #[serde(default)]
    pub extra_files: Vec<PathBuf>,
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

/// Resolve the union of profile + CLI additions into concrete files.
///
/// `root` is typically `globals_root().as_deref()`. When it is `None`, or
/// when the `skills/` / `memory/` subdirectory of `root` does not exist,
/// any tag/file requests for that kind are a hard error — if the user
/// asked for something concrete we refuse to silently do nothing. With
/// no requests at all the function returns an empty `Selected`.
pub fn select(
    root: Option<&Path>,
    profile: Option<&Profile>,
    skill_tags: &[String],
    memory_tags: &[String],
    skill_files: &[PathBuf],
    memory_files: &[PathBuf],
) -> Result<Selected, crate::Error> {
    // Union shared profile tags with kind-specific tags and CLI additions.
    let (prof_shared, prof_skills, prof_memory) = match profile {
        Some(p) => (p.tags.as_slice(), p.skills.as_ref(), p.memory.as_ref()),
        None => (&[][..], None, None),
    };

    let mut all_skill_tags: Vec<&str> = prof_shared.iter().map(String::as_str).collect();
    if let Some(s) = prof_skills {
        all_skill_tags.extend(s.tags.iter().map(String::as_str));
    }
    all_skill_tags.extend(skill_tags.iter().map(String::as_str));

    let mut all_memory_tags: Vec<&str> = prof_shared.iter().map(String::as_str).collect();
    if let Some(m) = prof_memory {
        all_memory_tags.extend(m.tags.iter().map(String::as_str));
    }
    all_memory_tags.extend(memory_tags.iter().map(String::as_str));

    let mut all_skill_files: Vec<&Path> = skill_files.iter().map(PathBuf::as_path).collect();
    if let Some(s) = prof_skills {
        all_skill_files.extend(s.extra_files.iter().map(PathBuf::as_path));
    }

    let mut all_memory_files: Vec<&Path> = memory_files.iter().map(PathBuf::as_path).collect();
    if let Some(m) = prof_memory {
        all_memory_files.extend(m.extra_files.iter().map(PathBuf::as_path));
    }

    for t in all_skill_tags.iter().chain(all_memory_tags.iter()) {
        if t.is_empty() {
            return Err("empty tag string is not allowed".into());
        }
    }

    let skills = resolve_kind(root, "skills", &all_skill_tags, &all_skill_files)?;
    let memory = resolve_kind(root, "memory", &all_memory_tags, &all_memory_files)?;

    Ok(Selected { skills, memory })
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

    #[test]
    fn empty_tag_rejected() {
        let err = select(None, None, &["".into()], &[], &[], &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("empty tag"), "got: {err}");
    }

    #[test]
    fn nothing_requested_is_empty_noop_even_without_root() {
        let s = select(None, None, &[], &[], &[], &[]).unwrap();
        assert!(s.skills.is_empty() && s.memory.is_empty());
    }

    #[test]
    fn request_without_root_errors() {
        let err = select(None, None, &["foo".into()], &[], &[], &[])
            .unwrap_err()
            .to_string();
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
        let s = select(
            Some(tmp.path()),
            None,
            &["languages".into()],
            &[],
            &[],
            &[],
        )
        .unwrap();
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
        let s = select(
            Some(tmp.path()),
            None,
            &["languages/python".into()],
            &[],
            &[],
            &[],
        )
        .unwrap();
        assert_eq!(relpaths(&s.skills), vec!["languages/python/typing.md"]);
    }

    #[test]
    fn extra_files_resolved_and_deduped_against_tags() {
        let tmp = fixture();
        let s = select(
            Some(tmp.path()),
            None,
            &["languages/python".into()],
            &[],
            &[PathBuf::from("languages/python/typing.md"), PathBuf::from("misc/readme.md")],
            &[],
        )
        .unwrap();
        let got = relpaths(&s.skills);
        // typing.md came from both the tag walk and extra_files; must appear once.
        assert_eq!(got, vec!["languages/python/typing.md", "misc/readme.md"]);
    }

    #[test]
    fn profile_shared_and_section_tags_union() {
        let tmp = fixture();
        let prof = Profile {
            tags: vec!["cli".into()],
            skills: Some(Section {
                tags: vec!["languages/rust".into()],
                extra_files: vec![],
            }),
            memory: None,
        };
        let s = select(Some(tmp.path()), Some(&prof), &[], &[], &[], &[]).unwrap();
        let got = relpaths(&s.skills);
        assert_eq!(got, vec!["cli/clap/derive.md", "languages/rust/traits.md"]);
    }

    #[test]
    fn missing_extra_file_errors() {
        let tmp = fixture();
        let err = select(
            Some(tmp.path()),
            None,
            &[],
            &[],
            &[PathBuf::from("does/not/exist.md")],
            &[],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("not found"), "got: {err}");
    }

    #[test]
    fn absolute_extra_file_rejected() {
        let tmp = fixture();
        let err = select(
            Some(tmp.path()),
            None,
            &[],
            &[],
            &[PathBuf::from("/etc/passwd")],
            &[],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("absolute"), "got: {err}");
    }

    #[test]
    fn parent_dir_traversal_rejected() {
        let tmp = fixture();
        let err = select(
            Some(tmp.path()),
            None,
            &[],
            &[],
            &[PathBuf::from("../outside.md")],
            &[],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains(".."), "got: {err}");
    }

    #[test]
    fn missing_kind_dir_with_request_errors() {
        // No `memory/` under root.
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("skills/foo")).unwrap();
        fs::write(tmp.path().join("skills/foo/x.md"), "").unwrap();
        let err = select(
            Some(tmp.path()),
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
        // Only request skills; memory/ missing but that's fine.
        let s = select(
            Some(tmp.path()),
            None,
            &["foo".into()],
            &[],
            &[],
            &[],
        )
        .unwrap();
        assert_eq!(relpaths(&s.skills), vec!["foo/x.md"]);
        assert!(s.memory.is_empty());
    }

    #[test]
    fn profile_memory_section_routes_to_memory() {
        let tmp = fixture();
        let prof = Profile {
            tags: vec![],
            skills: None,
            memory: Some(Section {
                tags: vec!["python/testing".into()],
                extra_files: vec![],
            }),
        };
        let s = select(Some(tmp.path()), Some(&prof), &[], &[], &[], &[]).unwrap();
        assert!(s.skills.is_empty());
        assert_eq!(relpaths(&s.memory), vec!["python/testing/pytest.md"]);
    }
}

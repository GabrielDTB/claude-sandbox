//! Inherited "globals" — skill directories shared across sandboxes.
//!
//! Host layout:
//!
//! ```text
//! $XDG_DATA_HOME/claude-sandboxed/
//!   skills/<tag>/<subtag>/.../<skill-name>/
//!     SKILL.md               # required — makes the dir a skill
//!     ...supporting files...
//! ```
//!
//! A **skill** is any directory containing a `SKILL.md` sibling. Its
//! identity inside the sandbox is the single final path component
//! (`<skill-name>`), which is also its mount target
//! (`/home/user/.claude/skills/<skill-name>/`). The directory chain
//! between `skills/` and the skill's parent becomes the skill's implicit
//! tag chain — e.g. `skills/languages/python/typing-helper/SKILL.md`
//! carries the tag `languages/python`. `SKILL.md` may also declare
//! additional tags via YAML frontmatter:
//!
//! ```text
//! ---
//! tags: [cli/clap, general]
//! description: Helper for clap derive structs
//! ---
//!
//! # body
//! ```
//!
//! A skill matches a configured tag if the tag prefix-at-segment-boundary
//! matches *any* of the skill's chains — the dir chain OR any frontmatter
//! entry. Tag matching is prefix-at-segment-boundary: `languages` matches
//! `languages/python` but not `languages-extended`.
//!
//! ## Layered configuration
//!
//! Selection is assembled from up to three layers (from outermost to
//! innermost):
//!
//! 1. top-level `[skills]` — defaults applied to every launch
//! 2. profile-level `[profiles.<name>]` — shared across every future kind
//! 3. profile-kind-level `[profiles.<name>.skills]`
//!
//! At each layer, `tags` and `extra_files` **override** the resolved value
//! from above (last-specified wins), while `extra_tags` and
//! `extra_extra_files` are always **unioned** with whatever came before.
//! CLI flags stack additively on top of the resolved config values.
//!
//! The `Section` / `Profile` / layered-resolver machinery is deliberately
//! generic — a future kind (e.g. hooks) can plug in by adding another
//! `Option<Section>` field to `Profile` and writing a kind-specific
//! resolver alongside [`resolve_skills`].

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;

/// One layer of tag/file selection for a single kind (currently only
/// skills; hooks and friends will reuse this).
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
    /// Override. Paths to include, relative to the kind's content
    /// directory. For skills, these name **skill directories** (the one
    /// containing `SKILL.md`), not individual files — e.g.
    /// `languages/python/my-skill`. Absolute paths and `..` components are
    /// rejected at resolve time.
    pub extra_files: Option<Vec<PathBuf>>,
    /// Additive. Always unioned on top of the resolved `extra_files`.
    #[serde(default)]
    pub extra_extra_files: Vec<PathBuf>,
}

/// A named profile declared in the user config under `[profiles.<name>]`.
///
/// The profile's own `tags`/`extra_tags`/`extra_files`/`extra_extra_files`
/// fields are SHARED across every kind — they form the middle layer in
/// the override chain between the top-level `[skills]` section and the
/// innermost per-kind `[profiles.<name>.skills]` subsection.
#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// Shared override. Applies to every kind's resolution.
    pub tags: Option<Vec<String>>,
    /// Shared additive. Applies to every kind's resolution.
    #[serde(default)]
    pub extra_tags: Vec<String>,
    /// Shared override. Applies to every kind's resolution.
    pub extra_files: Option<Vec<PathBuf>>,
    /// Shared additive. Applies to every kind's resolution.
    #[serde(default)]
    pub extra_extra_files: Vec<PathBuf>,
    pub skills: Option<Section>,
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

/// A single skill picked for the launch.
///
/// `host_path` is an absolute path to the host directory containing
/// `SKILL.md`. `name` is the final path component of `host_path` — it is
/// also the mount-target name inside the sandbox
/// (`/home/user/.claude/skills/<name>/`) and the key used to detect
/// collisions across the selected set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedSkill {
    pub host_path: PathBuf,
    pub name: String,
}

/// Resolved set of skills to mount into the sandbox. Emitted in
/// deterministic alphabetical order by `name`.
#[derive(Debug, Default)]
pub struct Selected {
    pub skills: Vec<SelectedSkill>,
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

/// Resolve the union of config layers + CLI additions into concrete skills.
///
/// The three config layers are walked in order
/// (top-level → profile-shared → profile-skills). `tags` / `extra_files`
/// overrides are last-wins; `extra_tags` / `extra_extra_files` are
/// accumulated across every layer. CLI lists are appended last.
///
/// `root` is typically `globals_root().as_deref()`. When it is `None`, or
/// when the `skills/` subdirectory of `root` does not exist, any
/// tag/file requests are a hard error — if the user asked for something
/// concrete we refuse to silently do nothing. With no requests at all the
/// function returns an empty `Selected`.
pub fn select(
    root: Option<&Path>,
    top_skills: Option<&Section>,
    profile: Option<&Profile>,
    skill_tags_cli: &[String],
    skill_files_cli: &[PathBuf],
) -> Result<Selected, crate::Error> {
    // Assemble the layer stack (outermost first). `flatten()` in
    // `resolve_layers` skips absent layers.
    let profile_shared = profile.map(Profile::shared_view);
    let skill_layers: [Option<SectionView<'_>>; 3] = [
        top_skills.map(Section::view),
        profile_shared,
        profile.and_then(|p| p.skills.as_ref()).map(Section::view),
    ];

    let (mut skill_tags, mut skill_files) = resolve_layers(&skill_layers);

    // CLI flags are additive — same semantics as `extra_*` accumulators,
    // just applied after all config layers are merged.
    skill_tags.extend(skill_tags_cli.iter().cloned());
    skill_files.extend(skill_files_cli.iter().cloned());

    for t in &skill_tags {
        if t.is_empty() {
            return Err("empty tag string is not allowed".into());
        }
    }

    let skill_tag_refs: Vec<&str> = skill_tags.iter().map(String::as_str).collect();
    let skill_file_refs: Vec<&Path> = skill_files.iter().map(PathBuf::as_path).collect();

    let skills = resolve_skills(root, &skill_tag_refs, &skill_file_refs)?;

    Ok(Selected { skills })
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

fn resolve_skills(
    root: Option<&Path>,
    tags: &[&str],
    extra_files: &[&Path],
) -> Result<Vec<SelectedSkill>, crate::Error> {
    if tags.is_empty() && extra_files.is_empty() {
        return Ok(Vec::new());
    }
    let Some(root) = root else {
        return Err(
            "cannot resolve skills globals: no $XDG_DATA_HOME or $HOME in environment".into(),
        );
    };
    let kind_root = root.join("skills");
    if !kind_root.is_dir() {
        return Err(format!(
            "cannot resolve skills globals: {} is not a directory",
            kind_root.display()
        )
        .into());
    }

    // Keyed on skill name → host_path. BTreeMap keeps the output
    // deterministic (alphabetical by name) and lets us detect duplicate
    // names as collisions rather than silent overwrites.
    let mut out: BTreeMap<String, PathBuf> = BTreeMap::new();

    if !tags.is_empty() {
        walk_skills(&kind_root, "", tags, &mut out)?;
    }

    for rel in extra_files {
        validate_relpath(rel)?;
        let host = kind_root.join(rel);
        if !host.is_dir() {
            return Err(format!(
                "skill extra_file must point at a directory containing SKILL.md: {} (under {})",
                rel.display(),
                kind_root.display()
            )
            .into());
        }
        if !host.join("SKILL.md").is_file() {
            return Err(format!(
                "skill extra_file {} is a directory but has no SKILL.md",
                rel.display()
            )
            .into());
        }
        let Some(name) = host.file_name().and_then(|n| n.to_str()).map(str::to_string) else {
            return Err(format!(
                "skill extra_file has no valid UTF-8 final component: {}",
                rel.display()
            )
            .into());
        };
        insert_unique(&mut out, name, host)?;
    }

    Ok(out
        .into_iter()
        .map(|(name, host_path)| SelectedSkill { host_path, name })
        .collect())
}

/// Walk `dir` (relative path from kind root = `dir_chain`) looking for
/// directories that contain `SKILL.md`. A match halts descent — nested
/// subdirectories under a skill are the skill's own assets, not separate
/// skills. Files at non-skill levels are ignored.
///
/// Symlinked directories are skipped to avoid escaping the kind root.
/// Symlinked regular files inside a skill dir are the skill's business,
/// not the walker's.
fn walk_skills(
    kind_root: &Path,
    dir_chain: &str,
    tags: &[&str],
    out: &mut BTreeMap<String, PathBuf>,
) -> Result<(), crate::Error> {
    let dir = if dir_chain.is_empty() {
        kind_root.to_path_buf()
    } else {
        kind_root.join(dir_chain)
    };

    // If *this* directory itself is a skill, emit and stop.
    if dir != kind_root && dir.join("SKILL.md").is_file() {
        let Some(name) = dir.file_name().and_then(|n| n.to_str()).map(str::to_string) else {
            return Err(format!(
                "skill directory has a non-UTF-8 name: {}",
                dir.display()
            )
            .into());
        };
        let fm_tags = read_frontmatter_tags(&dir.join("SKILL.md"))?;
        let matched = tags.iter().any(|t| {
            (!dir_chain.is_empty() && tag_matches(dir_chain, t))
                || fm_tags.iter().any(|fm| tag_matches(fm, t))
        });
        if matched {
            insert_unique(out, name, dir)?;
        }
        return Ok(());
    }

    // Otherwise, descend into subdirs only.
    let entries = std::fs::read_dir(&dir)
        .map_err(|e| -> crate::Error { format!("reading {}: {e}", dir.display()).into() })?;
    for entry in entries {
        let entry = entry
            .map_err(|e| -> crate::Error { format!("iterating {}: {e}", dir.display()).into() })?;
        let ft = entry.file_type().map_err(|e| -> crate::Error {
            format!("stat {}: {e}", entry.path().display()).into()
        })?;
        // Only recurse into real subdirectories. Symlinked-dirs and files
        // at this level are ignored — they're either noise or a loop risk.
        if !ft.is_dir() || ft.is_symlink() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue; // non-UTF-8 dir name — skip silently, can't tag it anyway.
        };
        let child_chain = if dir_chain.is_empty() {
            name
        } else {
            format!("{dir_chain}/{name}")
        };
        walk_skills(kind_root, &child_chain, tags, out)?;
    }
    Ok(())
}

fn insert_unique(
    out: &mut BTreeMap<String, PathBuf>,
    name: String,
    host: PathBuf,
) -> Result<(), crate::Error> {
    use std::collections::btree_map::Entry;
    match out.entry(name) {
        Entry::Vacant(v) => {
            v.insert(host);
            Ok(())
        }
        Entry::Occupied(o) => {
            // Re-selecting the exact same host path (e.g. extra_files entry
            // that also matched by tag) is a dedupe, not a collision.
            if o.get() == &host {
                return Ok(());
            }
            Err(format!(
                "skill name `{}` is selected from both {} and {} — skill names must be unique in the mounted set",
                o.key(),
                o.get().display(),
                host.display(),
            )
            .into())
        }
    }
}

/// Cap on how much of a file we'll read while hunting for the closing
/// frontmatter fence. Frontmatter blocks are conventionally tiny (a few
/// KiB at most); 64 KiB is generous while still bounding pathological
/// inputs that declare `---` but never close it.
const MAX_FRONTMATTER_BYTES: usize = 64 * 1024;

/// Parse YAML frontmatter at the head of a `SKILL.md` file and return the
/// `tags` field.
///
/// Returns an empty vec when the file has no frontmatter at all (doesn't
/// start with `---\n` / `---\r\n`). Returns an error when frontmatter is
/// opened but never closed, when the YAML fails to parse, when a tag is
/// empty, or when a tag has a leading/trailing `/` (which would break
/// prefix-at-boundary matching). Unknown sibling fields (`description`,
/// `model`, etc.) are silently ignored — we coexist with Claude Code's
/// other frontmatter conventions.
fn read_frontmatter_tags(path: &Path) -> Result<Vec<String>, crate::Error> {
    let text = match std::fs::read_to_string(path) {
        Ok(s) => s,
        // Non-UTF-8 files can't have YAML frontmatter anyway — treat as
        // "no frontmatter" rather than a hard error.
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => return Ok(Vec::new()),
        Err(e) => return Err(format!("reading {}: {e}", path.display()).into()),
    };

    let after_open = match text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
    {
        Some(rest) => rest,
        None => return Ok(Vec::new()),
    };

    // Walk lines looking for a closing `---` or `...` on its own. Cap the
    // scan at MAX_FRONTMATTER_BYTES so a file that opens `---` but never
    // closes can't force us to read gigabytes.
    let scan_end = after_open.len().min(MAX_FRONTMATTER_BYTES);
    let scan_region = &after_open[..scan_end];
    let mut yaml_end: Option<usize> = None;
    let mut offset = 0;
    for line in scan_region.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(&['\n', '\r'][..]);
        if trimmed == "---" || trimmed == "..." {
            yaml_end = Some(offset);
            break;
        }
        offset += line.len();
    }
    let yaml_str = match yaml_end {
        Some(end) => &after_open[..end],
        None => {
            return Err(format!(
                "frontmatter of {} opens with `---` but has no closing `---` or `...` within {}KB",
                path.display(),
                MAX_FRONTMATTER_BYTES / 1024,
            )
            .into());
        }
    };

    #[derive(serde::Deserialize, Default)]
    struct Front {
        #[serde(default)]
        tags: Vec<String>,
    }

    let front: Front = serde_yaml::from_str(yaml_str).map_err(|e| -> crate::Error {
        format!("parsing YAML frontmatter of {}: {e}", path.display()).into()
    })?;

    for t in &front.tags {
        if t.is_empty() {
            return Err(format!(
                "frontmatter of {} has an empty tag string",
                path.display()
            )
            .into());
        }
        if t.starts_with('/') || t.ends_with('/') {
            return Err(format!(
                "frontmatter tag {:?} in {} must not have leading or trailing `/`",
                t,
                path.display()
            )
            .into());
        }
    }

    Ok(front.tags)
}

/// Prefix-at-segment-boundary match.
///
/// `dir_chain` is the slash-joined directory chain from the kind root to
/// the skill's parent (e.g. `languages/python` for a skill at
/// `skills/languages/python/my-skill/SKILL.md`). Returns true iff
/// `dir_chain == tag` or `dir_chain` starts with `tag/`.
fn tag_matches(dir_chain: &str, tag: &str) -> bool {
    if dir_chain == tag {
        return true;
    }
    if let Some(rest) = dir_chain.strip_prefix(tag) {
        return rest.starts_with('/');
    }
    false
}

/// Reject absolute paths and any `..` / root components in an `extra_files` entry.
fn validate_relpath(rel: &Path) -> Result<(), crate::Error> {
    if rel.is_absolute() {
        return Err(format!(
            "skill extra_file must be relative, got absolute path: {}",
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
                    "skill extra_file must not contain `..` or root components: {}",
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
        assert!(!tag_matches("", "any"));
    }

    // ---- Test helpers -------------------------------------------------------

    fn select_simple(
        root: Option<&Path>,
        profile: Option<&Profile>,
        skill_tags: &[&str],
        skill_files: &[&str],
    ) -> Result<Selected, crate::Error> {
        let tags: Vec<String> = skill_tags.iter().map(|s| s.to_string()).collect();
        let files: Vec<PathBuf> = skill_files.iter().map(PathBuf::from).collect();
        select(root, None, profile, &tags, &files)
    }

    fn names(v: &[SelectedSkill]) -> Vec<String> {
        v.iter().map(|s| s.name.clone()).collect()
    }

    /// Write a minimal SKILL.md at `dir/SKILL.md` (creating parents).
    fn mk_skill(dir: &Path, body: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("SKILL.md"), body).unwrap();
    }

    fn fixture() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let skills = tmp.path().join("skills");
        mk_skill(&skills.join("languages/python/py-typing"), "# py typing\n");
        mk_skill(&skills.join("languages/rust/rust-traits"), "# rust traits\n");
        mk_skill(&skills.join("cli/clap/clap-derive"), "# clap derive\n");
        mk_skill(&skills.join("misc/readme-style"), "# readme\n");
        tmp
    }

    // ---- `select` behavior --------------------------------------------------

    #[test]
    fn empty_tag_rejected() {
        let err = select_simple(None, None, &[""], &[]).unwrap_err().to_string();
        assert!(err.contains("empty tag"), "got: {err}");
    }

    #[test]
    fn nothing_requested_is_empty_noop_even_without_root() {
        let s = select_simple(None, None, &[], &[]).unwrap();
        assert!(s.skills.is_empty());
    }

    #[test]
    fn request_without_root_errors() {
        let err = select_simple(None, None, &["foo"], &[]).unwrap_err().to_string();
        assert!(err.contains("no $XDG_DATA_HOME"), "got: {err}");
    }

    #[test]
    fn walk_selects_tag_subtree() {
        let tmp = fixture();
        let s = select_simple(Some(tmp.path()), None, &["languages"], &[]).unwrap();
        assert_eq!(names(&s.skills), vec!["py-typing", "rust-traits"]);
    }

    #[test]
    fn exact_tag_only_matches_that_subtree() {
        let tmp = fixture();
        let s = select_simple(Some(tmp.path()), None, &["languages/python"], &[]).unwrap();
        assert_eq!(names(&s.skills), vec!["py-typing"]);
    }

    #[test]
    fn extra_files_resolved_and_deduped_against_tags() {
        let tmp = fixture();
        let s = select_simple(
            Some(tmp.path()),
            None,
            &["languages/python"],
            &["languages/python/py-typing", "misc/readme-style"],
        )
        .unwrap();
        // py-typing hit both the tag walk and extra_files — dedupe keeps it
        // once.
        assert_eq!(names(&s.skills), vec!["py-typing", "readme-style"]);
    }

    #[test]
    fn extra_files_pointing_at_non_dir_errors() {
        let tmp = fixture();
        // A regular file (SKILL.md), not a skill dir itself.
        let err = select_simple(
            Some(tmp.path()),
            None,
            &[],
            &["languages/python/py-typing/SKILL.md"],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("directory containing SKILL.md"), "got: {err}");
    }

    #[test]
    fn extra_files_pointing_at_dir_without_skill_md_errors() {
        let tmp = fixture();
        // A bare dir — no SKILL.md inside.
        fs::create_dir_all(tmp.path().join("skills/bare/empty-dir")).unwrap();
        let err = select_simple(Some(tmp.path()), None, &[], &["bare/empty-dir"])
            .unwrap_err()
            .to_string();
        assert!(err.contains("no SKILL.md"), "got: {err}");
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
        let err = select_simple(Some(tmp.path()), None, &[], &["../outside"])
            .unwrap_err()
            .to_string();
        assert!(err.contains(".."), "got: {err}");
    }

    #[test]
    fn missing_kind_dir_with_request_errors() {
        // Tmp has no skills/ subdir at all.
        let tmp = tempfile::tempdir().unwrap();
        let err = select_simple(Some(tmp.path()), None, &["anything"], &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a directory"), "got: {err}");
    }

    #[test]
    fn stray_md_at_root_is_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("skills")).unwrap();
        fs::write(tmp.path().join("skills/loose.md"), "not a skill\n").unwrap();
        mk_skill(&tmp.path().join("skills/cat/real"), "# real\n");
        let s = select_simple(Some(tmp.path()), None, &["cat"], &[]).unwrap();
        assert_eq!(names(&s.skills), vec!["real"]);
    }

    #[test]
    fn nested_skill_md_not_emitted_separately() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("skills");
        // Outer skill:
        mk_skill(&root.join("cli/outer"), "# outer\n");
        // "Nested skill" — actually just an asset dir of outer. Walk must
        // stop descending once it finds outer/SKILL.md.
        mk_skill(&root.join("cli/outer/nested"), "# nested\n");
        let s = select_simple(Some(tmp.path()), None, &["cli"], &[]).unwrap();
        assert_eq!(names(&s.skills), vec!["outer"]);
    }

    #[test]
    fn top_level_skill_md_skipped() {
        // skills/SKILL.md has no distinct name — skip.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("skills");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("SKILL.md"), "# weird\n").unwrap();
        mk_skill(&root.join("cat/ok"), "# ok\n");
        let s = select_simple(Some(tmp.path()), None, &["cat"], &[]).unwrap();
        assert_eq!(names(&s.skills), vec!["ok"]);
    }

    #[test]
    fn skill_name_collision_across_tag_dirs_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("skills");
        mk_skill(&root.join("a/dup"), "# a\n");
        mk_skill(&root.join("b/dup"), "# b\n");
        let err = select_simple(Some(tmp.path()), None, &["a", "b"], &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("`dup`") && err.contains("unique"), "got: {err}");
    }

    // ---- Layered override / extra_tag accumulation --------------------------

    #[test]
    fn top_level_section_applies_when_no_profile() {
        let tmp = fixture();
        let top = Section {
            tags: Some(vec!["languages/python".into()]),
            ..Section::default()
        };
        let s = select(Some(tmp.path()), Some(&top), None, &[], &[]).unwrap();
        assert_eq!(names(&s.skills), vec!["py-typing"]);
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
        let s = select(Some(tmp.path()), Some(&top), Some(&prof), &[], &[]).unwrap();
        // Profile `tags` replaced top-level `tags` entirely — no python.
        assert_eq!(names(&s.skills), vec!["clap-derive"]);
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
        let s = select(Some(tmp.path()), None, Some(&prof), &[], &[]).unwrap();
        // profile.skills.tags overrides profile.tags — cli dropped.
        assert_eq!(names(&s.skills), vec!["rust-traits"]);
    }

    #[test]
    fn profile_kind_absent_inherits_profile_shared_tags() {
        let tmp = fixture();
        let prof = Profile {
            tags: Some(vec!["cli".into()]),
            ..Profile::default()
        };
        let s = select(Some(tmp.path()), None, Some(&prof), &[], &[]).unwrap();
        assert_eq!(names(&s.skills), vec!["clap-derive"]);
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
        let s = select(Some(tmp.path()), Some(&top), Some(&prof), &[], &[]).unwrap();
        assert_eq!(
            names(&s.skills),
            vec!["clap-derive", "py-typing", "rust-traits"]
        );
    }

    #[test]
    fn override_tags_then_accumulate_extra_tags() {
        let tmp = fixture();
        let top = Section {
            tags: Some(vec!["languages".into()]),
            ..Section::default()
        };
        let prof = Profile {
            skills: Some(Section {
                tags: Some(vec!["cli".into()]),
                extra_tags: vec!["languages/rust".into()],
                ..Section::default()
            }),
            ..Profile::default()
        };
        let s = select(Some(tmp.path()), Some(&top), Some(&prof), &[], &[]).unwrap();
        // Top "languages" dropped by profile.skills override; cli + rust remain.
        assert_eq!(names(&s.skills), vec!["clap-derive", "rust-traits"]);
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
            Some(&prof),
            &["cli".into()],
            &[],
        )
        .unwrap();
        assert_eq!(names(&s.skills), vec!["clap-derive", "py-typing"]);
    }

    #[test]
    fn extra_files_override_then_extra_extra_files_accumulate() {
        let tmp = fixture();
        let top = Section {
            extra_files: Some(vec![PathBuf::from("misc/readme-style")]),
            extra_extra_files: vec![PathBuf::from("cli/clap/clap-derive")],
            ..Section::default()
        };
        let prof = Profile {
            skills: Some(Section {
                // Override drops top's misc/readme-style → only py-typing
                // from the override.
                extra_files: Some(vec![PathBuf::from("languages/python/py-typing")]),
                // Additive on top of top.extra_extra_files.
                extra_extra_files: vec![PathBuf::from("languages/rust/rust-traits")],
                ..Section::default()
            }),
            ..Profile::default()
        };
        let s = select(Some(tmp.path()), Some(&top), Some(&prof), &[], &[]).unwrap();
        assert_eq!(
            names(&s.skills),
            vec!["clap-derive", "py-typing", "rust-traits"]
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
                tags: Some(vec![]),
                ..Section::default()
            }),
            ..Profile::default()
        };
        let s = select(Some(tmp.path()), Some(&top), Some(&prof), &[], &[]).unwrap();
        assert!(s.skills.is_empty());
    }

    // ---- Frontmatter-driven tags ------------------------------------------

    #[test]
    fn frontmatter_tag_selects_skill_outside_its_dir_chain() {
        let tmp = tempfile::tempdir().unwrap();
        mk_skill(
            &tmp.path().join("skills/misc/general-helper"),
            "---\ntags: [general, cli/clap]\ndescription: stuff\n---\n\nbody\n",
        );
        mk_skill(
            &tmp.path().join("skills/misc/block-style"),
            "---\ntags:\n  - general\n  - deep/nested\n---\n\nbody\n",
        );
        // Skill without frontmatter — only dir chain can select it.
        mk_skill(
            &tmp.path().join("skills/languages/python/py-no-fm"),
            "# no frontmatter\n",
        );
        let s = select_simple(Some(tmp.path()), None, &["general"], &[]).unwrap();
        assert_eq!(names(&s.skills), vec!["block-style", "general-helper"]);
    }

    #[test]
    fn frontmatter_tag_prefix_match_works() {
        let tmp = tempfile::tempdir().unwrap();
        mk_skill(
            &tmp.path().join("skills/misc/general-helper"),
            "---\ntags: [cli/clap, general]\n---\n\nbody\n",
        );
        let s = select_simple(Some(tmp.path()), None, &["cli"], &[]).unwrap();
        assert_eq!(names(&s.skills), vec!["general-helper"]);
    }

    #[test]
    fn unclosed_frontmatter_errors() {
        let tmp = tempfile::tempdir().unwrap();
        mk_skill(
            &tmp.path().join("skills/misc/bad"),
            "---\ntags: [x]\n# no closing\n",
        );
        let err = select_simple(Some(tmp.path()), None, &["misc"], &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("no closing"), "got: {err}");
    }

    #[test]
    fn malformed_yaml_frontmatter_errors() {
        let tmp = tempfile::tempdir().unwrap();
        mk_skill(
            &tmp.path().join("skills/misc/bad"),
            "---\ntags: [unclosed,\n---\n\nbody\n",
        );
        let err = select_simple(Some(tmp.path()), None, &["misc"], &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("parsing YAML frontmatter"), "got: {err}");
    }

    #[test]
    fn frontmatter_empty_tag_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        mk_skill(
            &tmp.path().join("skills/misc/bad"),
            "---\ntags: [\"\"]\n---\n\nbody\n",
        );
        let err = select_simple(Some(tmp.path()), None, &["misc"], &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("empty tag string"), "got: {err}");
    }

    #[test]
    fn frontmatter_tag_with_leading_slash_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        mk_skill(
            &tmp.path().join("skills/misc/bad"),
            "---\ntags: [\"/bad\"]\n---\n\nbody\n",
        );
        let err = select_simple(Some(tmp.path()), None, &["misc"], &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("leading or trailing"), "got: {err}");
    }

    #[test]
    fn frontmatter_unknown_sibling_fields_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        mk_skill(
            &tmp.path().join("skills/misc/ok"),
            "---\ndescription: hi\nmodel: sonnet-3.5\ntags: [general]\n---\n\nbody\n",
        );
        let s = select_simple(Some(tmp.path()), None, &["general"], &[]).unwrap();
        assert_eq!(names(&s.skills), vec!["ok"]);
    }

    #[test]
    fn frontmatter_no_tags_field_is_fine() {
        let tmp = tempfile::tempdir().unwrap();
        mk_skill(
            &tmp.path().join("skills/misc/silent"),
            "---\ndescription: no tags here\n---\n\nbody\n",
        );
        let s = select_simple(Some(tmp.path()), None, &["misc"], &[]).unwrap();
        assert_eq!(names(&s.skills), vec!["silent"]);
    }

    #[test]
    fn content_with_nonframe_dashes_treated_as_no_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        mk_skill(
            &tmp.path().join("skills/misc/mid-dashes"),
            "# Title\n\nintro\n\n---\n\nmore\n",
        );
        let s = select_simple(Some(tmp.path()), None, &["misc"], &[]).unwrap();
        assert_eq!(names(&s.skills), vec!["mid-dashes"]);
    }
}

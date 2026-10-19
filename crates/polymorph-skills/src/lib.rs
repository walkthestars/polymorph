//! `polymorph-skills` — Agent Skills (agentskills.io) conformance (J2b).
//!
//! Implements the spec's progressive disclosure in three levels:
//! 1. metadata (`SkillMeta`: name + description, frontmatter only),
//! 2. instructions (`Skill`: full body on activation),
//! 3. resources (files under the skill dir, loaded on demand by callers).
//!
//! Frontmatter parsing covers a DOCUMENTED SUBSET of YAML (flat `key:
//! value` scalars plus one indented level under `metadata:`) — enough for
//! every spec field (required + optional). Full YAML is explicitly out:
//! no yaml dep vendored for a flat grammar, and exotic YAML is refused
//! loudly rather than misread. Unknown top-level fields are IGNORED
//! (forward-compat: the spec labels fields like `allowed-tools`
//! experimental for a reason).
//!
//! Discovery is STRICT (first fault errors, naming path + rule) per the
//! manifest-gate precedent: skill drift fails loud, never silent.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Crate identifier. Used by smoke tests to prove the crate links.
pub fn crate_name() -> &'static str {
    "polymorph-skills"
}

/// Level-1 metadata: loaded at startup for every skill (~100 tokens).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillMeta {
    pub name: String,
    pub description: String,
    pub dir: PathBuf,
}

/// Level-2 skill: metadata + optional spec fields + instruction body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub meta: SkillMeta,
    pub license: Option<String>,
    pub compatibility: Option<String>,
    pub metadata: BTreeMap<String, String>,
    pub allowed_tools: Vec<String>,
    pub body: String,
}

/// Typed refusal. Every variant names the skill path and the spec rule.
#[derive(Debug, PartialEq, thiserror::Error)]
pub enum SkillError {
    #[error("{path}: missing SKILL.md")]
    MissingFile { path: String },
    #[error("{path}: unreadable: {detail}")]
    Unreadable { path: String, detail: String },
    #[error("{path}: frontmatter must open with `---`: {detail}")]
    BadFence { path: String, detail: String },
    #[error("{path}: malformed frontmatter line {line}: {detail}")]
    BadLine {
        path: String,
        line: usize,
        detail: String,
    },
    #[error("{path}: missing required field `{field}`")]
    MissingField { path: String, field: &'static str },
    #[error("{path}: invalid name `{name}`: {rule}")]
    BadName {
        path: String,
        name: String,
        rule: &'static str,
    },
    #[error("{path}: name `{name}` must match parent directory `{dir}`")]
    NameDirMismatch {
        path: String,
        name: String,
        dir: String,
    },
    #[error("{path}: invalid description: {rule}")]
    BadDescription { path: String, rule: &'static str },
    #[error("{path}: `compatibility` exceeds 500 characters")]
    BadCompatibility { path: String },
    #[error("{path}: not a directory")]
    NotADirectory { path: String },
}

/// Discover every skill under `dir`: each child directory containing
/// `SKILL.md` is validated strictly (first fault errors).
pub fn discover(dir: &Path) -> Result<Vec<SkillMeta>, SkillError> {
    let root = dir.to_string_lossy().to_string();
    if !dir.is_dir() {
        return Err(SkillError::NotADirectory { path: root });
    }
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| SkillError::Unreadable {
            path: root.clone(),
            detail: e.to_string(),
        })?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();
    entries.sort();
    let mut metas = Vec::new();
    for child in entries {
        let skill_md = child.join("SKILL.md");
        if !skill_md.is_file() {
            continue; // plain directories are not skills (not a fault)
        }
        metas.push(validate(&child)?.meta);
    }
    Ok(metas)
}

/// Strict validation of one skill directory (naming names the fault).
pub fn validate(dir: &Path) -> Result<Skill, SkillError> {
    let path = dir.to_string_lossy().to_string();
    let text = std::fs::read_to_string(dir.join("SKILL.md")).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            SkillError::MissingFile { path: path.clone() }
        } else {
            SkillError::Unreadable {
                path: path.clone(),
                detail: e.to_string(),
            }
        }
    })?;
    let Frontmatter {
        fields,
        metadata: meta_map,
        body,
    } = split_frontmatter(&path, &text)?;
    let name = required(&path, &fields, "name")?;
    check_name(&path, name, dir)?;
    let description = required(&path, &fields, "description")?;
    check_description(&path, description)?;
    if let Some(compat) = fields.get("compatibility") {
        if compat.chars().count() > 500 {
            return Err(SkillError::BadCompatibility { path });
        }
    }
    Ok(Skill {
        meta: SkillMeta {
            name: name.clone(),
            description: description.clone(),
            dir: dir.to_path_buf(),
        },
        license: fields.get("license").cloned(),
        compatibility: fields.get("compatibility").cloned(),
        metadata: meta_map,
        allowed_tools: fields
            .get("allowed-tools")
            .map(|s| s.split_whitespace().map(str::to_string).collect())
            .unwrap_or_default(),
        body,
    })
}

/// Load a skill's full instructions (level 2, on activation).
pub fn load(meta: &SkillMeta) -> Result<Skill, SkillError> {
    validate(&meta.dir)
}

/// Render canonical `SKILL.md` text (portability OUT: parse(render(x)) == x
/// for spec-shaped skills).
pub fn render(skill: &Skill) -> String {
    let mut out = String::from("---\n");
    out.push_str(&format!("name: {}\n", skill.meta.name));
    out.push_str(&format!("description: {}\n", skill.meta.description));
    if let Some(license) = &skill.license {
        out.push_str(&format!("license: {license}\n"));
    }
    if let Some(compat) = &skill.compatibility {
        out.push_str(&format!("compatibility: {compat}\n"));
    }
    if !skill.metadata.is_empty() {
        out.push_str("metadata:\n");
        for (k, v) in &skill.metadata {
            out.push_str(&format!("  {k}: {v}\n"));
        }
    }
    if !skill.allowed_tools.is_empty() {
        out.push_str(&format!(
            "allowed-tools: {}\n",
            skill.allowed_tools.join(" ")
        ));
    }
    out.push_str("---\n");
    out.push_str(&skill.body);
    out
}

fn required<'a>(
    path: &str,
    fields: &'a BTreeMap<String, String>,
    field: &'static str,
) -> Result<&'a String, SkillError> {
    fields.get(field).ok_or_else(|| SkillError::MissingField {
        path: path.to_string(),
        field,
    })
}

fn check_name(path: &str, name: &str, dir: &Path) -> Result<(), SkillError> {
    let rule = if name.is_empty() || name.chars().count() > 64 {
        Some("must be 1-64 characters")
    } else if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        Some("lowercase letters, numbers, and hyphens only")
    } else if name.starts_with('-') || name.ends_with('-') {
        Some("must not start or end with a hyphen")
    } else if name.contains("--") {
        Some("must not contain consecutive hyphens")
    } else {
        None
    };
    if let Some(rule) = rule {
        return Err(SkillError::BadName {
            path: path.to_string(),
            name: name.to_string(),
            rule,
        });
    }
    let dir_name = dir.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    if dir_name != name {
        return Err(SkillError::NameDirMismatch {
            path: path.to_string(),
            name: name.to_string(),
            dir: dir_name.to_string(),
        });
    }
    Ok(())
}

fn check_description(path: &str, description: &str) -> Result<(), SkillError> {
    let len = description.chars().count();
    if description.trim().is_empty() {
        return Err(SkillError::BadDescription {
            path: path.to_string(),
            rule: "must be non-empty",
        });
    }
    if len > 1024 {
        return Err(SkillError::BadDescription {
            path: path.to_string(),
            rule: "must be 1-1024 characters",
        });
    }
    Ok(())
}

/// Split frontmatter: flat fields + the one-level `metadata:` map + body.
struct Frontmatter {
    fields: BTreeMap<String, String>,
    metadata: BTreeMap<String, String>,
    body: String,
}
fn split_frontmatter(path: &str, text: &str) -> Result<Frontmatter, SkillError> {
    let mut lines = text.lines();
    match lines.next() {
        Some(first) if first.trim() == "---" => {}
        _ => {
            return Err(SkillError::BadFence {
                path: path.to_string(),
                detail: "first line must be `---`".to_string(),
            })
        }
    }
    let mut fields = BTreeMap::new();
    let mut meta_map = BTreeMap::new();
    let mut in_meta = false;
    let mut fence_line = 0usize;
    for (i, line) in lines.by_ref().enumerate() {
        let line_no = i + 2; // 1-based, past the opening fence
        if line.trim() == "---" {
            fence_line = line_no;
            break;
        }
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        // One indented level under `metadata:` (subset boundary, documented).
        if line.starts_with(' ') || line.starts_with('\t') {
            if !in_meta {
                return Err(SkillError::BadLine {
                    path: path.to_string(),
                    line: line_no,
                    detail: "indented lines only under `metadata:`".to_string(),
                });
            }
            let (k, v) = split_kv(line.trim()).map_err(|detail| SkillError::BadLine {
                path: path.to_string(),
                line: line_no,
                detail,
            })?;
            meta_map.insert(k, v);
            continue;
        }
        in_meta = false;
        if line.trim_end().ends_with(':') && !line.contains(": ") {
            // A `key:` header with no inline value — only `metadata:` opens
            // the subset map; anything else is refused (not guessed).
            let key = line.trim().trim_end_matches(':').to_string();
            if key == "metadata" {
                in_meta = true;
                continue;
            }
            return Err(SkillError::BadLine {
                path: path.to_string(),
                line: line_no,
                detail: format!("nested maps only under `metadata:`, got `{key}:`"),
            });
        }
        let (k, v) = split_kv(line).map_err(|detail| SkillError::BadLine {
            path: path.to_string(),
            line: line_no,
            detail,
        })?;
        if k == "metadata" {
            return Err(SkillError::BadLine {
                path: path.to_string(),
                line: line_no,
                detail: "`metadata:` takes an indented map, not an inline value".to_string(),
            });
        }
        fields.insert(k, v);
    }
    if fence_line == 0 {
        return Err(SkillError::BadFence {
            path: path.to_string(),
            detail: "no closing `---`".to_string(),
        });
    }
    let body: String = lines.collect::<Vec<_>>().join("\n");
    Ok(Frontmatter {
        fields,
        metadata: meta_map,
        body,
    })
}

/// Split one `key: value` line on the FIRST colon (values may contain
/// colons, e.g. URLs). Strips one layer of matching quotes.
fn split_kv(line: &str) -> Result<(String, String), String> {
    let (k, v) = line
        .split_once(':')
        .ok_or_else(|| format!("no `:` in `{line}`"))?;
    let key = k.trim().to_string();
    if key.is_empty() {
        return Err("empty key".to_string());
    }
    let value = v.trim().to_string();
    Ok((key, strip_quotes(&value)))
}

fn strip_quotes(value: &str) -> String {
    let bytes = value.as_bytes();
    if bytes.len() >= 2 {
        let (first, last) = (bytes[0], bytes[bytes.len() - 1]);
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return value[1..value.len() - 1].to_string();
        }
    }
    value.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build one skill dir in a temp root (generated fixtures, labeled).
    fn skill_dir(root: &tempfile::TempDir, name: &str, frontmatter: &str, body: &str) -> PathBuf {
        let dir = root.path().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), format!("{frontmatter}\n{body}")).unwrap();
        dir
    }

    #[test]
    fn spec_minimal_sample_admits() {
        // The spec's own minimal example (names adjusted: the example name
        // must equal its directory — validated here, not assumed).
        let root = tempfile::tempdir().unwrap();
        let dir = skill_dir(&root, "skill-name", "---\nname: skill-name\ndescription: A description of what this skill does and when to use it.\n---", "Do the thing.");
        let skill = validate(&dir).unwrap();
        assert_eq!(skill.meta.name, "skill-name");
        assert_eq!(skill.body, "Do the thing.");
    }

    #[test]
    fn optional_fields_parse() {
        let root = tempfile::tempdir().unwrap();
        let fm = "---\nname: pdf-processing\ndescription: Extract PDF text, fill forms, merge files. Use when handling PDFs.\nlicense: Apache-2.0\ncompatibility: Requires Python 3.14+\nmetadata:\n  author: example-org\n  version: \"1.0\"\nallowed-tools: Bash(git:*) Read\n---";
        let dir = skill_dir(&root, "pdf-processing", fm, "Steps.");
        let skill = validate(&dir).unwrap();
        assert_eq!(skill.license.as_deref(), Some("Apache-2.0"));
        assert_eq!(
            skill.compatibility.as_deref(),
            Some("Requires Python 3.14+")
        );
        assert_eq!(
            skill.metadata.get("author").map(String::as_str),
            Some("example-org")
        );
        assert_eq!(
            skill.metadata.get("version").map(String::as_str),
            Some("1.0")
        );
        assert_eq!(skill.allowed_tools, vec!["Bash(git:*)", "Read"]);
    }

    #[test]
    fn name_rules_reject_each_class() {
        let root = tempfile::tempdir().unwrap();
        for (dir_name, name, _rule) in [
            ("UPPER", "PDF-Processing", "uppercase"),
            ("-pdf", "-pdf", "leading hyphen"),
            ("pdf-", "pdf-", "trailing hyphen"),
            ("pdf--x", "pdf--x", "consecutive hyphens"),
        ] {
            let dir = root.path().join(dir_name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: Ddddd use when x.\n---\nB."),
            )
            .unwrap();
            let err = validate(&dir).unwrap_err().to_string();
            assert!(err.contains("invalid name"), "{name}: {err}");
        }
        // 65-char name refuses on length (64 = max).
        let long = "a".repeat(65);
        let dir = root.path().join("long");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {long}\ndescription: Ddddd use when x.\n---\nB."),
        )
        .unwrap();
        assert!(validate(&dir).unwrap_err().to_string().contains("1-64"));
    }

    #[test]
    fn name_must_match_parent_dir() {
        let root = tempfile::tempdir().unwrap();
        let dir = skill_dir(
            &root,
            "other",
            "---\nname: pdf-processing\ndescription: Ddddd use when x.\n---",
            "B.",
        );
        let err = validate(&dir).unwrap_err().to_string();
        assert!(err.contains("must match parent directory"), "{err}");
    }

    #[test]
    fn description_rules() {
        let root = tempfile::tempdir().unwrap();
        // Empty refuses.
        let d1 = skill_dir(&root, "e1", "---\nname: e1\ndescription: \n---", "B.");
        assert!(validate(&d1).unwrap_err().to_string().contains("non-empty"));
        // Missing refuses naming the field.
        let d2 = skill_dir(&root, "e2", "---\nname: e2\n---", "B.");
        assert!(validate(&d2)
            .unwrap_err()
            .to_string()
            .contains("`description`"));
        // 1025 chars refuses.
        let big = "x".repeat(1025);
        let d3 = skill_dir(
            &root,
            "e3",
            &format!("---\nname: e3\ndescription: {big}\n---"),
            "B.",
        );
        assert!(validate(&d3).unwrap_err().to_string().contains("1-1024"));
    }

    #[test]
    fn fence_and_line_faults_name_lines() {
        let root = tempfile::tempdir().unwrap();
        let d1 = skill_dir(&root, "f1", "name: f1\ndescription: D use x.\n---", "B.");
        assert!(validate(&d1)
            .unwrap_err()
            .to_string()
            .contains("first line must be"));
        let d2 = skill_dir(&root, "f2", "---\nname: f2\ndescription: D use x.", "k: v");
        assert!(validate(&d2)
            .unwrap_err()
            .to_string()
            .contains("no closing"));
        let d3 = skill_dir(
            &root,
            "f3",
            "---\nname f3\ndescription: D use x.\n---",
            "B.",
        );
        assert!(validate(&d3)
            .unwrap_err()
            .to_string()
            .contains("malformed frontmatter line 2"));
    }

    #[test]
    fn discover_lists_valid_skips_plain_dirs_strict_on_first_fault() {
        let root = tempfile::tempdir().unwrap();
        skill_dir(
            &root,
            "good-one",
            "---\nname: good-one\ndescription: Ddddd use when x.\n---",
            "B.",
        );
        std::fs::create_dir_all(root.path().join("plain-dir")).unwrap();
        let metas = discover(root.path()).unwrap();
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].name, "good-one");
        // A faulted skill fails the whole discovery (strict, stated).
        skill_dir(&root, "bad", "---\nname: bad\ndescription: \n---", "B.");
        let err = discover(root.path()).unwrap_err().to_string();
        assert!(err.contains("bad"), "{err}");
    }

    #[test]
    fn render_round_trip() {
        // Portability OUT: canonical render re-parses to equal fields
        // under a same-named dir (the dir-match rule forces the rename).
        let root = tempfile::tempdir().unwrap();
        let dir = skill_dir(
            &root,
            "pdf-processing",
            "---\nname: pdf-processing\ndescription: Extract PDFs. Use when handling PDFs.\nlicense: Apache-2.0\nmetadata:\n  author: example-org\nallowed-tools: Read\n---",
            "Steps here.",
        );
        let skill = validate(&dir).unwrap();
        let root2 = tempfile::tempdir().unwrap();
        let dir2 = root2.path().join("pdf-processing");
        std::fs::create_dir_all(&dir2).unwrap();
        std::fs::write(dir2.join("SKILL.md"), render(&skill)).unwrap();
        let again = validate(&dir2).unwrap();
        assert_eq!(again.meta.name, skill.meta.name);
        assert_eq!(again.meta.description, skill.meta.description);
        assert_eq!(again.license, skill.license);
        assert_eq!(again.metadata, skill.metadata);
        assert_eq!(again.allowed_tools, skill.allowed_tools);
        assert_eq!(again.body, skill.body);
    }

    #[test]
    fn shipped_git_split_discovers_and_validates() {
        // The T2 skill ships in-repo (workspace `skills/`); manifest-
        // relative so the test holds from any cwd (harness-shell
        // precedent: never assume cwd).
        let shipped = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("skills")
            .join("git-split");
        let skill = validate(&shipped).unwrap();
        assert_eq!(skill.meta.name, "git-split");
        assert!(skill.body.contains("never creates"), "propose-only pinned");
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("skills");
        let metas = discover(&root).unwrap();
        assert!(metas.iter().any(|m| m.name == "git-split"));
    }

    #[test]
    fn load_reads_body_on_activation() {
        let root = tempfile::tempdir().unwrap();
        skill_dir(
            &root,
            "lazy-one",
            "---\nname: lazy-one\ndescription: Ddddd use when x.\n---",
            "Full instructions here.",
        );
        let metas = discover(root.path()).unwrap();
        assert_eq!(metas.len(), 1);
        let skill = load(&metas[0]).unwrap();
        assert_eq!(skill.body, "Full instructions here.");
    }
}

#[cfg(test)]
mod links_test {
    #[test]
    fn links() {
        assert_eq!(super::crate_name(), "polymorph-skills");
    }
}

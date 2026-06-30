//! Structured user-global constitution and its deterministic renderer (#3793).
//!
//! The guided constitution creator does **not** drop the user into a blank
//! Markdown editor. The normal output is structured data persisted under
//! `$CODEWHALE_HOME` (`constitution.json`), which this module renders into a
//! stable prose `<codewhale_user_constitution>` block for the model.
//!
//! Design rules enforced here:
//!
//! - **Deterministic render.** [`UserConstitution::render_body`] is a pure
//!   function of the struct, so the same data always produces the same prose and
//!   the same [`preview_hash`](UserConstitution::preview_hash). The hash does not
//!   depend on the home path, so a preview matches its saved form byte-for-byte.
//! - **Bounded freeform.** Free prose ([`notes`](UserConstitution::notes)) and
//!   list items are length-capped via [`UserConstitution::bounded`]; freeform is
//!   advisory and is never parsed as enforceable runtime policy.
//! - **Autonomy is guidance, not control.** [`AutonomyPreference`] renders as a
//!   recommendation explicitly labeled as not changing approval policy, sandbox,
//!   shell, network, trust, MCP permission, or default mode. This module has no
//!   path that mutates runtime config; applying posture is owned by #3406.
//! - **Full Markdown override stays expert-only.** This module models the
//!   guided structured form; the `prompts/constitution.md` escape hatch is
//!   handled separately in the prompt layer.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::persistence;
use crate::setup_state::ConstitutionValidity;

/// Current schema version of the structured user-global constitution.
pub const USER_CONSTITUTION_SCHEMA_VERSION: u32 = 1;

/// Filename of the structured user-global constitution under `$CODEWHALE_HOME`.
pub const USER_CONSTITUTION_FILE_NAME: &str = "constitution.json";

/// Maximum length of the free-prose `notes` field after bounding.
pub const MAX_NOTES_LEN: usize = 4000;
/// Maximum length of any single `about` string after bounding.
pub const MAX_ABOUT_LEN: usize = 1000;
/// Maximum number of items kept in a bounded list field.
pub const MAX_LIST_ITEMS: usize = 20;
/// Maximum length of a single bounded list item.
pub const MAX_ITEM_LEN: usize = 280;

/// Model-facing autonomy preference. **Guidance only** — it may recommend a
/// runtime posture but never applies one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutonomyPreference {
    /// No preference expressed.
    #[default]
    Unspecified,
    /// Prefers to confirm before acting.
    Cautious,
    /// Balanced: act on clear tasks, confirm on risk.
    Balanced,
    /// Prefers the agent to proceed autonomously wherever it is safe.
    Autonomous,
}

impl AutonomyPreference {
    /// The recommendation sentence rendered into the constitution block.
    /// Always framed as guidance that does not change runtime controls.
    #[must_use]
    fn guidance(self) -> Option<&'static str> {
        match self {
            AutonomyPreference::Unspecified => None,
            AutonomyPreference::Cautious => Some(
                "The user leans cautious: prefer to confirm before taking actions that change \
                 files, run commands, or are hard to reverse.",
            ),
            AutonomyPreference::Balanced => Some(
                "The user prefers a balanced approach: act directly on clear, low-risk tasks and \
                 confirm before risky, destructive, or ambiguous actions.",
            ),
            AutonomyPreference::Autonomous => Some(
                "The user prefers you to proceed autonomously wherever it is safe, batching work \
                 and surfacing decisions rather than pausing for routine confirmations.",
            ),
        }
    }
}

/// Structured user-global constitution. All content fields are optional so a
/// minimal file still parses and a future schema stays forward-compatible.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserConstitution {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    /// Language the prose is authored in (BCP-47-ish tag, e.g. `"en"`,
    /// `"zh-Hans"`). Localization metadata only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Short description of who the user is / their working context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub about: Option<String>,
    /// Preferred working style / communication preferences.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub working_style: Vec<String>,
    /// Standing priorities or values to weigh across projects.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub priorities: Vec<String>,
    /// Autonomy preference — model-facing guidance only.
    #[serde(default)]
    pub autonomy_preference: AutonomyPreference,
    /// Bounded free prose. Advisory; never parsed as enforceable policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

fn default_schema_version() -> u32 {
    USER_CONSTITUTION_SCHEMA_VERSION
}

impl Default for UserConstitution {
    fn default() -> Self {
        Self {
            schema_version: USER_CONSTITUTION_SCHEMA_VERSION,
            language: None,
            about: None,
            working_style: Vec::new(),
            priorities: Vec::new(),
            autonomy_preference: AutonomyPreference::default(),
            notes: None,
        }
    }
}

impl UserConstitution {
    /// True when the constitution carries no usable content (so callers can skip
    /// emitting an empty block and classify it as [`ConstitutionValidity::Empty`]).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        opt_blank(&self.about)
            && self.working_style.iter().all(|s| s.trim().is_empty())
            && self.priorities.iter().all(|s| s.trim().is_empty())
            && self.autonomy_preference == AutonomyPreference::Unspecified
            && opt_blank(&self.notes)
    }

    /// Classify validity for the setup-state record.
    #[must_use]
    pub fn validity(&self) -> ConstitutionValidity {
        if self.is_empty() {
            ConstitutionValidity::Empty
        } else {
            ConstitutionValidity::Valid
        }
    }

    /// Return a bounded copy: list fields capped to [`MAX_LIST_ITEMS`] items of
    /// [`MAX_ITEM_LEN`] chars, prose capped to its limit, blank entries dropped.
    /// Free prose is never expanded into structure — it is only length-limited.
    #[must_use]
    pub fn bounded(&self) -> Self {
        Self {
            schema_version: USER_CONSTITUTION_SCHEMA_VERSION,
            language: self.language.as_deref().and_then(non_blank),
            about: self
                .about
                .as_deref()
                .and_then(non_blank)
                .map(|s| truncate_chars(&s, MAX_ABOUT_LEN)),
            working_style: bound_list(&self.working_style),
            priorities: bound_list(&self.priorities),
            autonomy_preference: self.autonomy_preference,
            notes: self
                .notes
                .as_deref()
                .and_then(non_blank)
                .map(|s| truncate_chars(&s, MAX_NOTES_LEN)),
        }
    }

    /// Deterministic, source-path-independent render of the constitution body.
    /// This is the canonical content hashed by [`preview_hash`](Self::preview_hash).
    #[must_use]
    pub fn render_body(&self) -> String {
        let bounded = self.bounded();
        let mut body = String::new();

        if let Some(about) = bounded.about.as_deref() {
            body.push_str("About the user:\n");
            body.push_str(about.trim());
            body.push_str("\n\n");
        }

        if !bounded.working_style.is_empty() {
            body.push_str("Working style:\n");
            for item in &bounded.working_style {
                body.push_str(&format!("- {item}\n"));
            }
            body.push('\n');
        }

        if !bounded.priorities.is_empty() {
            body.push_str("Standing priorities:\n");
            for item in &bounded.priorities {
                body.push_str(&format!("- {item}\n"));
            }
            body.push('\n');
        }

        if let Some(guidance) = bounded.autonomy_preference.guidance() {
            body.push_str(
                "Autonomy preference (guidance only — does not change approval policy, sandbox, \
                 shell, network, trust, MCP permissions, or default mode):\n",
            );
            body.push_str(guidance);
            body.push_str("\n\n");
        }

        if let Some(notes) = bounded.notes.as_deref() {
            body.push_str("Additional notes (advisory, not enforceable policy):\n");
            body.push_str(notes.trim());
            body.push('\n');
        }

        body.trim_end().to_string()
    }

    /// Render the full model-facing `<codewhale_user_constitution>` block.
    ///
    /// `source` is included as an attribute for provenance but does not affect
    /// the body or the preview hash. Returns `None` when empty.
    #[must_use]
    pub fn render_block(&self, source: Option<&Path>) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let source_attr = source.map_or_else(
            || " source=\"user-global\"".to_string(),
            |p| format!(" source=\"{}\"", p.display()),
        );
        Some(format!(
            "<codewhale_user_constitution{source_attr}>\n\
             User-global standing preferences (personal law: subordinate to the current user \
             request and the global Constitution, but applies across all your projects). Treat as \
             durable guidance, not as enforceable runtime policy.\n\n\
             {}\n\
             </codewhale_user_constitution>",
            self.render_body()
        ))
    }

    /// Stable content hash (FNV-1a 64-bit, hex) of the rendered body. Used for
    /// preview/version tracking in the setup-state record. Deterministic across
    /// platforms and independent of the home path.
    #[must_use]
    pub fn preview_hash(&self) -> String {
        format!("{:016x}", fnv1a64(self.render_body().as_bytes()))
    }

    /// Path to the structured user-global constitution under `$CODEWHALE_HOME`.
    pub fn path() -> Result<PathBuf> {
        Ok(crate::codewhale_home()?.join(USER_CONSTITUTION_FILE_NAME))
    }

    /// Load the structured constitution from the home file, classifying the
    /// outcome so callers can record validity without re-reading the file.
    pub fn load() -> Result<UserConstitutionLoad> {
        Ok(Self::load_from(&Self::path()?))
    }

    /// Load from an explicit path (testable).
    #[must_use]
    pub fn load_from(path: &Path) -> UserConstitutionLoad {
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return UserConstitutionLoad::Missing;
            }
            Err(e) => return UserConstitutionLoad::Unreadable(e.to_string()),
        };
        if raw.trim().is_empty() {
            return UserConstitutionLoad::Empty;
        }
        match serde_json::from_str::<UserConstitution>(&raw) {
            Ok(c) if c.is_empty() => UserConstitutionLoad::Empty,
            Ok(c) => UserConstitutionLoad::Loaded(Box::new(c)),
            Err(e) => UserConstitutionLoad::Invalid(e.to_string()),
        }
    }

    /// Atomically persist the bounded form to the home file. Callers invoke this
    /// only on accept — preview must never reach this path.
    pub fn save(&self) -> Result<()> {
        self.save_to(&Self::path()?)
    }

    /// Atomically persist the bounded form to an explicit path (testable).
    pub fn save_to(&self, path: &Path) -> Result<()> {
        persistence::atomic_write_json(path, &self.bounded())
            .with_context(|| format!("failed to persist user constitution to {}", path.display()))
    }
}

/// Outcome of loading the user-global constitution, mapped to
/// [`ConstitutionValidity`] for the setup-state record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserConstitutionLoad {
    /// No file present.
    Missing,
    /// Present but blank / no usable policy.
    Empty,
    /// Present but could not be read.
    Unreadable(String),
    /// Present but failed to parse.
    Invalid(String),
    /// Parsed and usable.
    Loaded(Box<UserConstitution>),
}

impl UserConstitutionLoad {
    /// The [`ConstitutionValidity`] this outcome implies.
    #[must_use]
    pub fn validity(&self) -> ConstitutionValidity {
        match self {
            UserConstitutionLoad::Missing => ConstitutionValidity::Unknown,
            UserConstitutionLoad::Empty => ConstitutionValidity::Empty,
            UserConstitutionLoad::Unreadable(_) => ConstitutionValidity::Unreadable,
            UserConstitutionLoad::Invalid(_) => ConstitutionValidity::Invalid,
            UserConstitutionLoad::Loaded(_) => ConstitutionValidity::Valid,
        }
    }

    /// The loaded constitution, if parsing succeeded.
    #[must_use]
    pub fn constitution(&self) -> Option<&UserConstitution> {
        match self {
            UserConstitutionLoad::Loaded(c) => Some(&**c),
            _ => None,
        }
    }
}

fn opt_blank(s: &Option<String>) -> bool {
    s.as_deref().is_none_or(|s| s.trim().is_empty())
}

fn non_blank(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

fn bound_list(items: &[String]) -> Vec<String> {
    items
        .iter()
        .filter_map(|s| non_blank(s))
        .map(|s| truncate_chars(&s, MAX_ITEM_LEN))
        .take(MAX_LIST_ITEMS)
        .collect()
}

/// Truncate to at most `max` characters (not bytes), preserving UTF-8.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

/// FNV-1a 64-bit hash. Small, dependency-free, and deterministic across
/// platforms — adequate for content fingerprinting (not cryptographic).
fn fnv1a64(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> UserConstitution {
        UserConstitution {
            about: Some("Maintainer of CodeWhale.".to_string()),
            working_style: vec!["Be concise.".to_string(), "Show diffs.".to_string()],
            priorities: vec!["Correctness over speed.".to_string()],
            autonomy_preference: AutonomyPreference::Balanced,
            notes: Some("Prefer Rust idioms.".to_string()),
            ..UserConstitution::default()
        }
    }

    #[test]
    fn empty_constitution_renders_no_block() {
        let c = UserConstitution::default();
        assert!(c.is_empty());
        assert!(c.render_block(None).is_none());
        assert_eq!(c.validity(), ConstitutionValidity::Empty);
    }

    #[test]
    fn render_is_deterministic() {
        let c = sample();
        assert_eq!(c.render_body(), c.render_body());
        assert_eq!(c.preview_hash(), c.preview_hash());
    }

    #[test]
    fn render_block_contains_sections_and_tag() {
        let c = sample();
        let block = c.render_block(None).unwrap();
        assert!(block.starts_with("<codewhale_user_constitution"));
        assert!(block.ends_with("</codewhale_user_constitution>"));
        assert!(block.contains("About the user:"));
        assert!(block.contains("Working style:"));
        assert!(block.contains("Standing priorities:"));
        assert!(block.contains("Additional notes"));
    }

    #[test]
    fn autonomy_renders_as_guidance_not_runtime_control() {
        let c = UserConstitution {
            autonomy_preference: AutonomyPreference::Autonomous,
            ..UserConstitution::default()
        };
        let block = c.render_block(None).unwrap();
        // Rendered as guidance, explicitly disclaiming runtime mutation.
        assert!(block.contains("guidance only"));
        assert!(block.contains("does not change approval policy"));
        // It must never emit runtime config assignments.
        assert!(!block.contains("approval_policy ="));
        assert!(!block.contains("sandbox_mode ="));
        assert!(!block.contains("default_mode ="));
    }

    #[test]
    fn unspecified_autonomy_emits_nothing() {
        let c = UserConstitution {
            about: Some("x".to_string()),
            autonomy_preference: AutonomyPreference::Unspecified,
            ..UserConstitution::default()
        };
        let block = c.render_block(None).unwrap();
        assert!(!block.contains("Autonomy preference"));
    }

    #[test]
    fn freeform_notes_are_length_bounded() {
        let huge = "x".repeat(MAX_NOTES_LEN + 500);
        let c = UserConstitution {
            notes: Some(huge),
            ..UserConstitution::default()
        };
        let bounded = c.bounded();
        assert_eq!(
            bounded.notes.as_deref().unwrap().chars().count(),
            MAX_NOTES_LEN
        );
    }

    #[test]
    fn list_items_are_bounded_in_count_and_length() {
        let many: Vec<String> = (0..MAX_LIST_ITEMS + 10)
            .map(|i| format!("item {i}"))
            .collect();
        let long_item = "y".repeat(MAX_ITEM_LEN + 50);
        let c = UserConstitution {
            working_style: {
                let mut v = many;
                v.push(long_item);
                v
            },
            ..UserConstitution::default()
        };
        let bounded = c.bounded();
        assert_eq!(bounded.working_style.len(), MAX_LIST_ITEMS);
        assert!(
            bounded
                .working_style
                .iter()
                .all(|s| s.chars().count() <= MAX_ITEM_LEN)
        );
    }

    #[test]
    fn blank_entries_are_dropped() {
        let c = UserConstitution {
            working_style: vec!["  ".to_string(), "real".to_string(), "".to_string()],
            ..UserConstitution::default()
        };
        assert_eq!(c.bounded().working_style, vec!["real".to_string()]);
    }

    #[test]
    fn preview_hash_changes_with_content() {
        let mut c = sample();
        let h1 = c.preview_hash();
        c.priorities.push("New priority.".to_string());
        assert_ne!(h1, c.preview_hash());
    }

    #[test]
    fn preview_hash_is_independent_of_source_path() {
        let c = sample();
        let h = c.preview_hash();
        // render_block takes a source, but the hash is over render_body only,
        // so rendering with a path must not change the preview hash.
        let block = c.render_block(Some(Path::new("/some/home/constitution.json")));
        assert!(block.unwrap().contains("/some/home/constitution.json"));
        assert_eq!(h, c.preview_hash());
    }

    #[test]
    fn save_persists_bounded_form_and_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(USER_CONSTITUTION_FILE_NAME);
        let c = sample();
        c.save_to(&path).unwrap();

        match UserConstitution::load_from(&path) {
            UserConstitutionLoad::Loaded(loaded) => {
                assert_eq!(loaded.render_body(), c.render_body());
                assert_eq!(loaded.validity(), ConstitutionValidity::Valid);
            }
            other => panic!("expected Loaded, got {other:?}"),
        }
    }

    #[test]
    fn load_classifies_missing_invalid_and_empty() {
        let tmp = tempfile::tempdir().unwrap();

        let missing = tmp.path().join("none.json");
        assert_eq!(
            UserConstitution::load_from(&missing).validity(),
            ConstitutionValidity::Unknown
        );

        let invalid = tmp.path().join("bad.json");
        std::fs::write(&invalid, "{ not json").unwrap();
        assert_eq!(
            UserConstitution::load_from(&invalid).validity(),
            ConstitutionValidity::Invalid
        );

        let empty = tmp.path().join("empty.json");
        std::fs::write(&empty, "{}").unwrap();
        assert_eq!(
            UserConstitution::load_from(&empty).validity(),
            ConstitutionValidity::Empty
        );
    }

    #[test]
    fn saved_file_contains_no_runtime_policy_keys() {
        // A constitution may express autonomy preference, but the persisted form
        // must never carry runtime-control keys that #3406 owns.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(USER_CONSTITUTION_FILE_NAME);
        UserConstitution {
            autonomy_preference: AutonomyPreference::Autonomous,
            about: Some("x".to_string()),
            ..UserConstitution::default()
        }
        .save_to(&path)
        .unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        for forbidden in ["approval_policy", "sandbox_mode", "default_mode", "trust"] {
            assert!(
                !raw.contains(forbidden),
                "leaked runtime key {forbidden}: {raw}"
            );
        }
    }
}

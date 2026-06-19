use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    error::Error,
    paths,
    protocol::ControlRequest,
    state::{AccessMode, EntryRecord, WorkspaceSnapshot},
};

const EDIT_CONFIG_VERSION: u32 = 1;
const ERROR_MARKER: &str = "# workspace-portal edit error:";
const ERROR_FOOTER: &str = "# Fix the config below, then save and exit.";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EditableConfig {
    #[serde(default = "editable_config_version")]
    pub version: u32,

    #[serde(default)]
    pub immutable_segments: BTreeSet<String>,

    #[serde(default)]
    pub entries: BTreeMap<String, EditableEntry>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EditableEntry {
    pub target: PathBuf,

    #[serde(default)]
    pub mode: AccessMode,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub(crate) enum EditConfigError {
    #[error("toml parse error: {0}")]
    Parse(String),

    #[error("unsupported config version: {0}")]
    UnsupportedVersion(u32),

    #[error("invalid entry name: {0}")]
    InvalidEntryName(String),

    #[error("invalid immutable segment name: {0}")]
    InvalidImmutableSegmentName(String),

    #[error("empty target for entry: {0}")]
    EmptyTarget(String),
}

fn editable_config_version() -> u32 {
    EDIT_CONFIG_VERSION
}

impl EditableConfig {
    pub(crate) fn from_snapshot(snapshot: &WorkspaceSnapshot) -> Self {
        let entries = snapshot
            .entries
            .iter()
            .cloned()
            .map(|entry: EntryRecord| {
                (
                    entry.name,
                    EditableEntry {
                        target: entry.target,
                        mode: entry.mode,
                    },
                )
            })
            .collect();

        Self {
            version: EDIT_CONFIG_VERSION,
            immutable_segments: snapshot.immutable_segments.iter().cloned().collect(),
            entries,
        }
    }

    pub(crate) fn parse(input: &str) -> Result<Self, EditConfigError> {
        let config: Self =
            toml::from_str(input).map_err(|err| EditConfigError::Parse(err.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    pub(crate) fn validate(&self) -> Result<(), EditConfigError> {
        if self.version != EDIT_CONFIG_VERSION {
            return Err(EditConfigError::UnsupportedVersion(self.version));
        }

        for name in self.entries.keys() {
            paths::validate_entry_name(name)
                .map_err(|_| EditConfigError::InvalidEntryName(name.clone()))?;

            let target = &self.entries[name].target;
            if target.as_os_str().is_empty() || target_display_is_empty(target) {
                return Err(EditConfigError::EmptyTarget(name.clone()));
            }
        }

        for segment in &self.immutable_segments {
            paths::validate_immutable_segment_name(segment)
                .map_err(|_| EditConfigError::InvalidImmutableSegmentName(segment.clone()))?;
        }

        Ok(())
    }

    pub(crate) fn render(&self) -> String {
        let mut out = String::new();
        writeln!(out, "version = {}", self.version).expect("write to string");
        writeln!(
            out,
            "immutable_segments = {}",
            render_string_array(&self.immutable_segments)
        )
        .expect("write to string");

        if self.entries.is_empty() {
            out.push('\n');
            out.push_str("# [entries.docs]\n");
            out.push_str("# target = \"/path/to/docs\"\n");
            out.push_str("# mode = \"rw\"\n");
            return out;
        }

        for (name, entry) in &self.entries {
            out.push('\n');
            writeln!(out, "[entries.{}]", render_toml_key(name)).expect("write to string");
            writeln!(
                out,
                "target = {}",
                render_toml_string(&entry.target.display().to_string())
            )
            .expect("write to string");
            writeln!(
                out,
                "mode = {}",
                render_toml_string(render_access_mode(entry.mode))
            )
            .expect("write to string");
        }

        out
    }
}

#[cfg(test)]
impl EditableEntry {
    fn new(target: PathBuf, mode: AccessMode) -> Self {
        Self { target, mode }
    }
}

impl Default for EditableConfig {
    fn default() -> Self {
        Self {
            version: EDIT_CONFIG_VERSION,
            immutable_segments: BTreeSet::new(),
            entries: BTreeMap::new(),
        }
    }
}

impl Default for EditableEntry {
    fn default() -> Self {
        Self {
            target: PathBuf::new(),
            mode: AccessMode::default(),
        }
    }
}

impl From<EditConfigError> for Error {
    fn from(err: EditConfigError) -> Self {
        Self::Cli(err.to_string())
    }
}

pub(crate) fn plan_edit(before: &EditableConfig, after: &EditableConfig) -> Vec<ControlRequest> {
    let mut requests = Vec::new();

    for name in before.entries.keys() {
        if !after.entries.contains_key(name) {
            requests.push(ControlRequest::Remove { name: name.clone() });
        }
    }

    for (name, entry) in &after.entries {
        match before.entries.get(name) {
            Some(existing) if existing == entry => {}
            _ => requests.push(ControlRequest::Add {
                name: name.clone(),
                target: entry.target.clone(),
                mode: entry.mode,
                replace: true,
            }),
        }
    }

    for segment in before
        .immutable_segments
        .difference(&after.immutable_segments)
    {
        requests.push(ControlRequest::Thaw {
            segment: segment.clone(),
        });
    }

    for segment in after
        .immutable_segments
        .difference(&before.immutable_segments)
    {
        requests.push(ControlRequest::Freeze {
            segment: segment.clone(),
        });
    }

    requests
}

pub(crate) fn wrap_error_comment_block(buffer: &str, error: impl AsRef<str>) -> String {
    let body = strip_generated_error_comment_block(buffer);
    let mut out = String::new();
    out.push_str(ERROR_MARKER);
    out.push('\n');
    for line in error.as_ref().lines() {
        out.push_str("# ");
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(ERROR_FOOTER);
    out.push_str("\n\n");
    out.push_str(body);
    out
}

fn strip_generated_error_comment_block(buffer: &str) -> &str {
    if !buffer.starts_with(ERROR_MARKER) {
        return buffer;
    }

    let mut offset = 0usize;
    let mut saw_marker = false;

    for line in buffer.split_inclusive('\n') {
        offset += line.len();
        if !saw_marker {
            saw_marker = true;
            continue;
        }

        if line.trim_end_matches('\n').is_empty() {
            return &buffer[offset..];
        }
    }

    ""
}

fn render_string_array(values: &BTreeSet<String>) -> String {
    let items = values
        .iter()
        .map(|value| toml::Value::String(value.clone()))
        .collect();
    toml::Value::Array(items).to_string()
}

fn render_toml_key(value: &str) -> String {
    toml::Value::String(value.to_owned()).to_string()
}

fn render_toml_string(value: &str) -> String {
    toml::Value::String(value.to_owned()).to_string()
}

fn render_access_mode(mode: AccessMode) -> &'static str {
    match mode {
        AccessMode::ReadOnly => "ro",
        AccessMode::ReadWrite => "rw",
    }
}

fn target_display_is_empty(target: &Path) -> bool {
    target.display().to_string().trim().is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(entries: Vec<EntryRecord>, immutable_segments: Vec<&str>) -> WorkspaceSnapshot {
        WorkspaceSnapshot {
            workspace: PathBuf::from("/workspace"),
            mounted: false,
            daemon: crate::state::DaemonStatus::Running,
            socket: PathBuf::from("/run/socket.sock"),
            entries,
            immutable_segments: immutable_segments.into_iter().map(str::to_owned).collect(),
            generation: 0,
        }
    }

    fn entry(name: &str, target: &str, mode: AccessMode) -> EntryRecord {
        EntryRecord::new(name, PathBuf::from(target), mode)
    }

    #[test]
    fn render_empty_snapshot_includes_example() {
        let rendered = EditableConfig::from_snapshot(&snapshot(vec![], vec![])).render();
        assert!(rendered.starts_with("version = 1\nimmutable_segments = []\n\n"));
        assert!(rendered.contains("# [entries.docs]\n"));
        assert!(rendered.contains("# target = \"/path/to/docs\"\n"));
        assert!(rendered.contains("# mode = \"rw\"\n"));
    }

    #[test]
    fn render_non_empty_snapshot_includes_explicit_defaults() {
        let rendered = EditableConfig::from_snapshot(&snapshot(
            vec![
                entry("cache", "/tmp/cache", AccessMode::ReadOnly),
                entry("docs", "/workspace/docs", AccessMode::ReadWrite),
            ],
            vec!["node_modules", "vendor"],
        ))
        .render();

        assert!(rendered.contains("version = 1\n"));
        assert!(rendered.contains("immutable_segments = [\"node_modules\", \"vendor\"]\n"));
        assert!(rendered.contains("[entries.\"cache\"]") || rendered.contains("[entries.cache]"));
        assert!(rendered.contains("target = \"/tmp/cache\"\n"));
        assert!(rendered.contains("mode = \"ro\"\n"));
        assert!(rendered.contains("[entries.\"docs\"]") || rendered.contains("[entries.docs]"));
        assert!(!rendered.contains("# [entries.docs]\n"));
    }

    #[test]
    fn parse_map_shaped_toml_and_missing_defaults() {
        let config = EditableConfig::parse(
            r#"
version = 1
immutable_segments = ["vendor"]

[entries.docs]
target = "/workspace/docs"
mode = "readwrite"

[entries.cache]
target = "/tmp/cache"
"#,
        )
        .unwrap();

        assert_eq!(config.version, 1);
        assert_eq!(
            config.immutable_segments,
            ["vendor".to_owned()].into_iter().collect()
        );
        assert_eq!(
            config.entries["docs"].target,
            PathBuf::from("/workspace/docs")
        );
        assert_eq!(config.entries["docs"].mode, AccessMode::ReadWrite);
        assert_eq!(config.entries["cache"].mode, AccessMode::ReadWrite);
    }

    #[test]
    fn parse_defaults_missing_entries_and_immutable_segments() {
        let config = EditableConfig::parse("version = 1\n").unwrap();
        assert!(config.entries.is_empty());
        assert!(config.immutable_segments.is_empty());
    }

    #[test]
    fn validation_rejects_invalid_names_empty_targets_and_unknown_fields() {
        let invalid_entry_name = EditableConfig::parse(
            r#"
version = 1

[entries."../docs"]
target = "/workspace/docs"
"#,
        )
        .unwrap_err();
        assert!(matches!(
            invalid_entry_name,
            EditConfigError::InvalidEntryName(value) if value == "../docs"
        ));

        let invalid_segment = EditableConfig::parse(
            r#"
version = 1
immutable_segments = ["../vendor"]
"#,
        )
        .unwrap_err();
        assert!(matches!(
            invalid_segment,
            EditConfigError::InvalidImmutableSegmentName(value) if value == "../vendor"
        ));

        let empty_target = EditableConfig::parse(
            r#"
version = 1

[entries.docs]
target = "   "
"#,
        )
        .unwrap_err();
        assert!(matches!(empty_target, EditConfigError::EmptyTarget(value) if value == "docs"));

        let unsupported_version = EditableConfig::parse(
            r#"
version = 2
"#,
        )
        .unwrap_err();
        assert!(matches!(
            unsupported_version,
            EditConfigError::UnsupportedVersion(2)
        ));

        let unknown_field = EditableConfig::parse(
            r#"
version = 1
immutable_segment = ["vendor"]
"#,
        )
        .unwrap_err();
        assert!(matches!(unknown_field, EditConfigError::Parse(_)));
    }

    #[test]
    fn planning_emits_only_changed_requests() {
        let before = EditableConfig {
            version: 1,
            immutable_segments: ["vendor".to_owned(), "cache".to_owned()]
                .into_iter()
                .collect(),
            entries: [
                (
                    "docs".to_owned(),
                    EditableEntry::new(PathBuf::from("/workspace/docs"), AccessMode::ReadOnly),
                ),
                (
                    "keep".to_owned(),
                    EditableEntry::new(PathBuf::from("/workspace/keep"), AccessMode::ReadWrite),
                ),
            ]
            .into_iter()
            .collect(),
        };
        let after = EditableConfig {
            version: 1,
            immutable_segments: ["cache".to_owned(), "node_modules".to_owned()]
                .into_iter()
                .collect(),
            entries: [
                (
                    "docs".to_owned(),
                    EditableEntry::new(PathBuf::from("/workspace/docs"), AccessMode::ReadWrite),
                ),
                (
                    "new".to_owned(),
                    EditableEntry::new(PathBuf::from("/tmp/new"), AccessMode::ReadOnly),
                ),
            ]
            .into_iter()
            .collect(),
        };

        let requests = plan_edit(&before, &after);
        assert_eq!(
            requests,
            vec![
                ControlRequest::Remove {
                    name: "keep".to_owned(),
                },
                ControlRequest::Add {
                    name: "docs".to_owned(),
                    target: PathBuf::from("/workspace/docs"),
                    mode: AccessMode::ReadWrite,
                    replace: true,
                },
                ControlRequest::Add {
                    name: "new".to_owned(),
                    target: PathBuf::from("/tmp/new"),
                    mode: AccessMode::ReadOnly,
                    replace: true,
                },
                ControlRequest::Thaw {
                    segment: "vendor".to_owned(),
                },
                ControlRequest::Freeze {
                    segment: "node_modules".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn error_comment_blocks_are_replaced_at_the_top() {
        let initial = wrap_error_comment_block(
            "version = 1\nimmutable_segments = []\n",
            "invalid immutable segment name: \"../vendor\"",
        );
        let updated = wrap_error_comment_block(&initial, "invalid entry name: \"../docs\"");

        assert!(updated.starts_with(ERROR_MARKER));
        assert_eq!(updated.matches(ERROR_MARKER).count(), 1);
        assert!(updated.contains("# invalid entry name: \"../docs\"\n"));
        assert!(!updated.contains("../vendor"));
        assert!(updated.ends_with("version = 1\nimmutable_segments = []\n"));
    }
}

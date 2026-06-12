use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{
    error::{Error, Result},
    paths,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Default)]
pub enum AccessMode {
    ReadOnly,
    #[default]
    ReadWrite,
}

impl Serialize for AccessMode {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(match self {
            Self::ReadOnly => "ro",
            Self::ReadWrite => "rw",
        })
    }
}

impl<'de> Deserialize<'de> for AccessMode {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        match value.as_str() {
            "ro" | "readonly" => Ok(Self::ReadOnly),
            "rw" | "readwrite" => Ok(Self::ReadWrite),
            other => Err(serde::de::Error::custom(format!(
                "invalid access mode: {other}"
            ))),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum DaemonStatus {
    Running,
    Stopped,
    #[default]
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EntryRecord {
    pub name: String,
    pub target: PathBuf,
    pub mode: AccessMode,
    #[serde(default)]
    pub generation: u64,
}

impl EntryRecord {
    pub fn new(name: impl Into<String>, target: PathBuf, mode: AccessMode) -> Self {
        Self {
            name: name.into(),
            target,
            mode,
            generation: 0,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceSnapshot {
    pub workspace: PathBuf,
    pub mounted: bool,
    pub daemon: DaemonStatus,
    pub socket: PathBuf,
    pub entries: Vec<EntryRecord>,
    #[serde(default)]
    pub immutable_segments: Vec<String>,
    pub generation: u64,
}

fn default_state_version() -> u32 {
    1
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PortalState {
    #[serde(default = "default_state_version")]
    pub version: u32,
    pub workspace: PathBuf,
    pub workspace_id: String,
    pub socket: PathBuf,
    #[serde(default)]
    pub state_file: PathBuf,
    pub mounted: bool,
    pub daemon: DaemonStatus,
    pub read_only_default: bool,
    pub generation: u64,
    #[serde(default)]
    pub entries: BTreeMap<String, EntryRecord>,
    #[serde(default)]
    pub immutable_segments: BTreeSet<String>,
}

impl PortalState {
    pub fn new(workspace: PathBuf, workspace_id: String, socket: PathBuf) -> Self {
        Self {
            version: default_state_version(),
            workspace,
            workspace_id,
            socket,
            state_file: PathBuf::new(),
            mounted: false,
            daemon: DaemonStatus::Unknown,
            read_only_default: false,
            generation: 0,
            entries: BTreeMap::new(),
            immutable_segments: BTreeSet::new(),
        }
    }

    pub fn with_defaults(mut self, read_only_default: bool) -> Self {
        self.read_only_default = read_only_default;
        self
    }

    pub fn with_storage_paths(mut self, state_file: PathBuf) -> Self {
        self.state_file = state_file;
        self
    }

    pub fn add_entry(&mut self, entry: EntryRecord, replace: bool) -> Result<()> {
        if self.entries.contains_key(&entry.name) && !replace {
            return Err(Error::EntryExists(entry.name));
        }

        self.generation = self.generation.saturating_add(1);
        let mut entry = entry;
        // The entry's `generation` is surfaced as the FUSE inode generation, which
        // must stay stable while the inode keeps mapping to the same object. A
        // top-level entry's identity is its `target`, so an in-place replace that
        // leaves the target unchanged (e.g. an `edit` mode flip) preserves the
        // existing generation; a new entry or a changed target gets a fresh one.
        entry.generation = match self.entries.get(&entry.name) {
            Some(existing) if existing.target == entry.target => existing.generation,
            _ => self.generation,
        };
        self.entries.insert(entry.name.clone(), entry);
        Ok(())
    }

    pub fn remove_entry(&mut self, name: &str) -> Result<EntryRecord> {
        self.generation = self.generation.saturating_add(1);
        self.entries
            .remove(name)
            .ok_or_else(|| Error::EntryNotFound(name.to_owned()))
    }

    pub fn entry(&self, name: &str) -> Option<&EntryRecord> {
        self.entries.get(name)
    }

    pub fn freeze_segment(&mut self, segment: String) -> bool {
        let inserted = self.immutable_segments.insert(segment);
        if inserted {
            self.generation = self.generation.saturating_add(1);
        }
        inserted
    }

    pub fn thaw_segment(&mut self, segment: &str) -> bool {
        let removed = self.immutable_segments.remove(segment);
        if removed {
            self.generation = self.generation.saturating_add(1);
        }
        removed
    }

    pub fn snapshot(&self) -> WorkspaceSnapshot {
        WorkspaceSnapshot {
            workspace: self.workspace.clone(),
            mounted: self.mounted,
            daemon: self.daemon,
            socket: self.socket.clone(),
            entries: self.entries.values().cloned().collect(),
            immutable_segments: self.immutable_segments.iter().cloned().collect(),
            generation: self.generation,
        }
    }

    pub fn load_from_path(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let text = fs::read_to_string(path)?;
        let mut state: Self = serde_json::from_str(&text)?;
        if state.state_file.as_os_str().is_empty() {
            state.state_file = path.to_path_buf();
        }
        Ok(state)
    }

    pub fn write_atomic(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let tmp_path = path.with_extension("json.tmp");
        let mut file = fs::File::create(&tmp_path)?;
        serde_json::to_writer_pretty(&mut file, self)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(tmp_path, path)?;
        Ok(())
    }
}

pub fn initialize_state(
    workspace: impl AsRef<Path>,
    read_only_default: bool,
    state_file: PathBuf,
    socket: PathBuf,
) -> Result<PortalState> {
    let workspace = paths::canonical_workspace_path(workspace)?;
    let workspace_id = paths::workspace_id(&workspace);

    Ok(PortalState::new(workspace, workspace_id, socket)
        .with_defaults(read_only_default)
        .with_storage_paths(state_file))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        sync::atomic::{AtomicUsize, Ordering},
    };

    static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

    fn unique_path(prefix: &str) -> PathBuf {
        let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "workspace-portal-{prefix}-{}-{id}",
            std::process::id()
        ))
    }

    #[test]
    fn add_and_remove_entries_update_generation() {
        let workspace = unique_path("state-workspace");
        let mut state = PortalState::new(
            workspace.clone(),
            "abc123".to_owned(),
            workspace.join("socket.sock"),
        );

        state
            .add_entry(
                EntryRecord::new("docs", workspace.join("docs"), AccessMode::ReadWrite),
                false,
            )
            .unwrap();
        assert_eq!(state.generation, 1);
        assert_eq!(state.entry("docs").unwrap().generation, 1);

        let removed = state.remove_entry("docs").unwrap();
        assert_eq!(removed.name, "docs");
        assert_eq!(state.generation, 2);
        assert!(state.entry("docs").is_none());
    }

    #[test]
    fn replace_with_same_target_preserves_entry_generation() {
        let workspace = unique_path("state-workspace-gen");
        let mut state = PortalState::new(
            workspace.clone(),
            "abc123".to_owned(),
            workspace.join("socket.sock"),
        );

        // First add: new entry gets a fresh generation.
        state
            .add_entry(
                EntryRecord::new("docs", PathBuf::from("/tmp/docs"), AccessMode::ReadWrite),
                false,
            )
            .unwrap();
        let g0 = state.entry("docs").unwrap().generation;
        assert_eq!(g0, 1, "first add should yield generation == 1");

        // Same-target replace (mode flip): entry generation must be preserved.
        state
            .add_entry(
                EntryRecord::new("docs", PathBuf::from("/tmp/docs"), AccessMode::ReadOnly),
                true,
            )
            .unwrap();
        assert_eq!(
            state.entry("docs").unwrap().generation,
            g0,
            "same-target replace must preserve entry generation"
        );
        assert_eq!(
            state.entry("docs").unwrap().mode,
            AccessMode::ReadOnly,
            "mode must be updated after same-target replace"
        );

        // Changed-target replace: entry should get a new (different) generation.
        state
            .add_entry(
                EntryRecord::new("docs", PathBuf::from("/tmp/docs2"), AccessMode::ReadOnly),
                true,
            )
            .unwrap();
        assert_ne!(
            state.entry("docs").unwrap().generation,
            g0,
            "changed-target replace must assign a fresh generation"
        );
    }

    #[test]
    fn write_atomic_persists_complete_state() {
        let workspace = unique_path("state-workspace-atomic");
        let state_dir = unique_path("state-state");
        let state_file = state_dir.join("portal.json");
        let mut state = PortalState::new(
            workspace.clone(),
            "abc123".to_owned(),
            workspace.join("socket.sock"),
        )
        .with_storage_paths(state_file.clone());
        state.mounted = true;
        state.daemon = DaemonStatus::Running;
        state.freeze_segment("vendor".to_owned());
        state
            .add_entry(
                EntryRecord::new("docs", workspace.join("docs"), AccessMode::ReadOnly),
                false,
            )
            .unwrap();

        state.write_atomic(&state_file).unwrap();
        assert!(state_file.exists());
        assert!(!state_file.with_extension("json.tmp").exists());

        let loaded = PortalState::load_from_path(&state_file).unwrap();
        assert_eq!(loaded.workspace, state.workspace);
        assert_eq!(loaded.entries.len(), 1);
        assert!(loaded.immutable_segments.contains("vendor"));
        assert_eq!(loaded.entry("docs").unwrap().mode, AccessMode::ReadOnly);
        assert_eq!(loaded.state_file, state_file);

        let _ = fs::remove_file(&state_file);
        let _ = fs::remove_dir_all(&state_dir);
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn freeze_and_thaw_segments_are_sorted_and_update_generation_on_change() {
        let workspace = unique_path("state-workspace-freeze");
        let mut state = PortalState::new(
            workspace.clone(),
            "abc123".to_owned(),
            workspace.join("socket.sock"),
        );

        assert!(state.freeze_segment("vendor".to_owned()));
        assert!(state.freeze_segment("cache".to_owned()));
        assert!(!state.freeze_segment("vendor".to_owned()));
        assert_eq!(state.generation, 2);

        let snapshot = state.snapshot();
        assert_eq!(
            snapshot.immutable_segments,
            vec!["cache".to_owned(), "vendor".to_owned()]
        );

        assert!(state.thaw_segment("vendor"));
        assert!(!state.thaw_segment("vendor"));
        assert_eq!(state.generation, 3);
    }
}

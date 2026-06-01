use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};

use fuser::{Config as FuserConfig, INodeNo, MountOption, SessionACL};

use crate::{
    error::Result,
    paths,
    state::{EntryRecord, PortalState},
};

mod attr;
mod callbacks;
mod path;
mod resolve;
mod runtime;
mod safe_open;

pub use path::{PortalPath, RenamePlan, ResolvedPortalPath, parse_portal_path};
pub use resolve::{resolve_read_path, resolve_write_path, validate_rename};

pub(crate) const ROOT_INO: INodeNo = INodeNo::ROOT;
// Use zero TTLs until we implement explicit invalidation on namespace changes.
// This avoids stale positive and negative dentries surviving successful renames.
pub(crate) const TTL: Duration = Duration::from_secs(0);

#[derive(Debug, Clone)]
pub struct FsConfig {
    pub workspace: PathBuf,
    pub read_only_default: bool,
}

#[derive(Debug)]
pub struct PortalFs {
    pub config: FsConfig,
    pub state: Arc<RwLock<PortalState>>,
    runtime: Mutex<runtime::FuseRuntime>,
}

impl PortalFs {
    pub fn new(state: Arc<RwLock<PortalState>>) -> Self {
        let config = {
            let state = state.read().unwrap();
            FsConfig {
                workspace: state.workspace.clone(),
                read_only_default: state.read_only_default,
            }
        };

        Self {
            config,
            state,
            runtime: Mutex::new(runtime::FuseRuntime::new()),
        }
    }

    pub async fn snapshot(&self) -> crate::state::WorkspaceSnapshot {
        self.state.read().unwrap().snapshot()
    }

    pub async fn entry(&self, name: &str) -> Option<EntryRecord> {
        self.state.read().unwrap().entry(name).cloned()
    }

    pub async fn list_entries(&self) -> std::collections::BTreeMap<String, EntryRecord> {
        self.state.read().unwrap().entries.clone()
    }

    pub fn validate_mount_point(name: &str) -> Result<()> {
        paths::validate_entry_name(name)
    }

    pub fn root_workspace(&self) -> &PathBuf {
        &self.config.workspace
    }

    pub fn root_entries(state: &PortalState) -> std::collections::BTreeMap<String, EntryRecord> {
        state.entries.clone()
    }

    pub fn parse_portal_path(path: impl AsRef<Path>) -> Result<PortalPath> {
        path::parse_portal_path(path)
    }

    pub fn resolve_portal_path(
        state: &PortalState,
        path: impl AsRef<Path>,
    ) -> Result<ResolvedPortalPath> {
        resolve::resolve_portal_path(state, path)
    }

    pub fn resolve_read_path(
        state: &PortalState,
        path: impl AsRef<Path>,
    ) -> Result<ResolvedPortalPath> {
        resolve::resolve_read_path(state, path)
    }

    pub fn resolve_write_path(
        state: &PortalState,
        path: impl AsRef<Path>,
    ) -> Result<ResolvedPortalPath> {
        resolve::resolve_write_path(state, path)
    }

    pub fn validate_rename(
        state: &PortalState,
        source: impl AsRef<Path>,
        destination: impl AsRef<Path>,
    ) -> Result<RenamePlan> {
        resolve::validate_rename(state, source, destination)
    }

    pub fn ensure_readable_entry(entry: &EntryRecord) -> Result<()> {
        resolve::ensure_readable_entry(entry)
    }

    pub fn ensure_writable_entry(entry: &EntryRecord) -> Result<()> {
        resolve::ensure_writable_entry(entry)
    }

    pub fn mount(self, mountpoint: &Path, allow_other: bool) -> Result<fuser::BackgroundSession> {
        let mut config = FuserConfig::default();
        config
            .mount_options
            .push(MountOption::FSName("workspace-portal".to_owned()));
        if allow_other {
            config.acl = SessionACL::All;
            config
                .mount_options
                .push(MountOption::DefaultPermissions);
        }
        Ok(fuser::spawn_mount2(self, mountpoint, &config)?)
    }
}

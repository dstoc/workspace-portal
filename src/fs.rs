use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
    thread,
    time::Duration,
};

use fuser::{Config as FuserConfig, INodeNo, MountOption, Notifier, Session, SessionACL};
use tracing::warn;

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
// fuser entry replies use one TTL for both entry and returned-attribute validity.
pub(crate) const ENTRY_TTL: Duration = Duration::from_secs(1);
pub(crate) const ATTR_TTL: Duration = Duration::from_secs(0);

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
    notifier: Arc<Mutex<Option<Notifier>>>,
}

pub(crate) fn build_mount_config(allow_other: bool, nosymfollow: bool) -> FuserConfig {
    let mut config = FuserConfig::default();
    config
        .mount_options
        .push(MountOption::FSName("workspace-portal".to_owned()));
    if allow_other {
        config.acl = SessionACL::All;
        config.mount_options.push(MountOption::DefaultPermissions);
    }
    if nosymfollow {
        config
            .mount_options
            .push(MountOption::CUSTOM("nosymfollow".to_owned()));
    }
    config
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
            notifier: Arc::new(Mutex::new(None)),
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

    pub(crate) fn invalidate_entry(&self, parent: INodeNo, name: &OsStr) {
        let notifier = match self.notifier.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => {
                warn!("fuse notifier mutex poisoned; attempting entry invalidation anyway");
                poisoned.into_inner().clone()
            }
        };

        let Some(notifier) = notifier else {
            warn!(
                parent = parent.0,
                name = ?name,
                "missing fuse notifier; skipped entry cache invalidation"
            );
            return;
        };

        let name = name.to_os_string();
        let log_name = name.clone();
        // Kernel invalidation can block if sent from the FUSE request thread
        // before that request's reply is processed, so send it out of band.
        if let Err(error) = thread::Builder::new()
            .name("workspace-portal-inval-entry".to_owned())
            .spawn(move || {
                if let Err(error) = notifier.inval_entry(parent, &name) {
                    warn!(
                        parent = parent.0,
                        name = ?name,
                        error = %error,
                        "failed to invalidate fuse entry cache"
                    );
                }
            })
        {
            warn!(
                parent = parent.0,
                name = ?log_name,
                error = %error,
                "failed to spawn fuse entry cache invalidation"
            );
        }
    }

    pub fn mount(
        self,
        mountpoint: &Path,
        allow_other: bool,
        nosymfollow: bool,
    ) -> Result<fuser::BackgroundSession> {
        let config = build_mount_config(allow_other, nosymfollow);
        let notifier = Arc::clone(&self.notifier);
        let session = Session::new(self, mountpoint, &config)?;
        *notifier.lock().unwrap() = Some(session.notifier());

        match session.spawn() {
            Ok(background) => Ok(background),
            Err(error) => {
                *notifier.lock().unwrap() = None;
                Err(error.into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_mount_config_omits_nosymfollow_by_default() {
        let config = build_mount_config(false, false);

        assert!(
            !config
                .mount_options
                .contains(&MountOption::CUSTOM("nosymfollow".to_owned()))
        );
    }

    #[test]
    fn build_mount_config_includes_nosymfollow_when_enabled() {
        let config = build_mount_config(false, true);

        assert!(
            config
                .mount_options
                .contains(&MountOption::CUSTOM("nosymfollow".to_owned()))
        );
    }

    #[test]
    fn ttl_constants_match_final_cache_policy() {
        assert_eq!(ENTRY_TTL, Duration::from_secs(1));
        assert_eq!(ATTR_TTL, Duration::from_secs(0));
    }
}

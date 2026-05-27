use std::{
    env, fs,
    path::{Path, PathBuf},
};

use crate::{
    error::{Error, Result},
    paths,
    state::PortalState,
};

#[derive(Debug, Clone)]
pub(crate) struct WorkspaceContext {
    pub(crate) workspace: PathBuf,
    pub(crate) workspace_id: String,
    pub(crate) socket: PathBuf,
    pub(crate) registry_path: PathBuf,
    pub(crate) state_path: PathBuf,
}

impl WorkspaceContext {
    pub(crate) fn from_workspace(
        workspace: PathBuf,
        state_dir: Option<&Path>,
        socket: Option<PathBuf>,
    ) -> Self {
        let workspace_id = paths::workspace_id(&workspace);
        let registry_path = paths::state_file_path(&workspace_id);
        let state_root = state_dir
            .map(PathBuf::from)
            .unwrap_or_else(paths::state_root);
        let state_path = paths::state_file_path_in(state_root, &workspace_id);
        let socket = socket.unwrap_or_else(|| paths::socket_path(&workspace_id));

        Self {
            workspace,
            workspace_id,
            socket,
            registry_path,
            state_path,
        }
    }
}

pub(crate) fn prepare_workspace_dir(workspace: &Path, adopt: bool, force: bool) -> Result<()> {
    if workspace.exists() {
        let metadata = fs::metadata(workspace)?;
        if !metadata.is_dir() {
            return Err(Error::InvalidWorkspace(workspace.to_path_buf()));
        }

        if !adopt && !force && !is_dir_empty(workspace)? {
            return Err(Error::Cli(
                "workspace directory is not empty; use --adopt or --force".to_owned(),
            ));
        }
    } else {
        fs::create_dir_all(workspace)?;
    }

    Ok(())
}

fn is_dir_empty(path: &Path) -> Result<bool> {
    let mut entries = fs::read_dir(path)?;
    Ok(entries.next().is_none())
}

pub(crate) fn load_workspace_state(workspace: &Path) -> Result<Option<PortalState>> {
    let workspace = paths::canonical_workspace_path(workspace)?;
    let workspace_id = paths::workspace_id(&workspace);
    let registry_path = paths::state_file_path(&workspace_id);

    if registry_path.exists() {
        let state = PortalState::load_from_path(&registry_path)?;
        if state.workspace != workspace {
            return Err(Error::StateCorrupt(format!(
                "registry workspace mismatch: {} != {}",
                state.workspace.display(),
                workspace.display()
            )));
        }
        return Ok(Some(state));
    }

    Ok(None)
}

pub(crate) fn load_workspace_context(
    workspace: Option<PathBuf>,
) -> Result<(WorkspaceContext, PortalState)> {
    let workspace = match workspace {
        Some(workspace) => paths::canonical_workspace_path(workspace)?,
        None => paths::discover_workspace(env::current_dir()?)?,
    };

    let workspace_id = paths::workspace_id(&workspace);
    let registry_path = paths::state_file_path(&workspace_id);

    let mut state = if registry_path.exists() {
        PortalState::load_from_path(&registry_path)?
    } else {
        return Err(Error::WorkspaceNotFound(workspace));
    };

    if state.workspace != workspace {
        return Err(Error::StateCorrupt(format!(
            "state workspace mismatch: {} != {}",
            state.workspace.display(),
            workspace.display()
        )));
    }

    let state_path = if state.state_file.as_os_str().is_empty() {
        registry_path.clone()
    } else {
        state.state_file.clone()
    };

    let socket = if state.socket.as_os_str().is_empty() {
        paths::socket_path(&workspace_id)
    } else {
        state.socket.clone()
    };

    state.state_file = state_path.clone();
    state.socket = socket.clone();

    Ok((
        WorkspaceContext {
            workspace,
            workspace_id,
            socket,
            registry_path,
            state_path,
        },
        state,
    ))
}

pub(crate) fn canonicalize_target(target: PathBuf) -> Result<PathBuf> {
    let target = target
        .canonicalize()
        .map_err(|_| Error::TargetNotFound(target.clone()))?;
    if !target.is_dir() {
        return Err(Error::TargetNotDirectory(target));
    }

    Ok(target)
}

pub(crate) fn persist_workspace_state(
    state: &PortalState,
    state_path: &Path,
    registry_path: &Path,
    socket: &Path,
) -> Result<()> {
    let mut to_write = state.clone();
    to_write.state_file = state_path.to_path_buf();
    to_write.socket = socket.to_path_buf();
    to_write.write_atomic(state_path)?;
    if registry_path != state_path {
        to_write.write_atomic(registry_path)?;
    }
    Ok(())
}

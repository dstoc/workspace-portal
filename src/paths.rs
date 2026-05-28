use std::{
    env,
    os::unix::ffi::OsStrExt,
    path::{Component, Path, PathBuf},
};

use nix::unistd::Uid;

use crate::error::{Error, Result};
use crate::state::PortalState;

pub fn validate_entry_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(Error::InvalidEntryName(name.to_owned()));
    }

    if name == "." || name == ".." || name.as_bytes().contains(&0) {
        return Err(Error::InvalidEntryName(name.to_owned()));
    }

    if name.contains('/') || name.contains(std::path::MAIN_SEPARATOR) {
        return Err(Error::InvalidEntryName(name.to_owned()));
    }

    if Path::new(name)
        .components()
        .any(|component| matches!(component, Component::ParentDir | Component::RootDir))
    {
        return Err(Error::InvalidEntryName(name.to_owned()));
    }

    Ok(())
}

pub fn state_file_path_in(root: impl AsRef<Path>, workspace_id: &str) -> PathBuf {
    root.as_ref()
        .join("workspaces")
        .join(format!("{workspace_id}.json"))
}

pub fn discover_workspace(start: impl AsRef<Path>) -> Result<PathBuf> {
    discover_workspace_in(start, state_root())
}

fn discover_workspace_in(start: impl AsRef<Path>, state_root: impl AsRef<Path>) -> Result<PathBuf> {
    let start = start.as_ref().to_path_buf();
    let mut current = canonical_workspace_path(&start)?;
    let state_root = state_root.as_ref().to_path_buf();

    loop {
        let workspace_id = workspace_id(&current);
        let registry_path = state_file_path_in(&state_root, &workspace_id);
        if registry_path.exists() {
            if let Ok(state) = PortalState::load_from_path(&registry_path) {
                if state.workspace == current {
                    return Ok(current);
                }
            }
        }

        if !current.pop() {
            return Err(Error::WorkspaceNotFound(start));
        }
    }
}

pub fn canonical_workspace_path(path: impl AsRef<Path>) -> Result<PathBuf> {
    let path = path.as_ref();
    if path.exists() {
        return Ok(path.canonicalize()?);
    }

    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }

    Ok(env::current_dir()?.join(path))
}

pub fn workspace_id(workspace: impl AsRef<Path>) -> String {
    let uid = Uid::effective().as_raw();
    let mut hasher = blake3::Hasher::new();
    hasher.update(workspace.as_ref().as_os_str().as_bytes());
    hasher.update(uid.to_string().as_bytes());

    let hash = hasher.finalize().to_hex().to_string();
    hash[..16].to_owned()
}

fn xdg_state_home() -> PathBuf {
    env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .unwrap_or_else(env::temp_dir)
}

fn xdg_runtime_dir() -> PathBuf {
    env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| env::temp_dir().join(format!("workspace-portal-{}", Uid::effective())))
}

pub fn state_root() -> PathBuf {
    xdg_state_home().join("workspace-portal")
}

pub fn runtime_root() -> PathBuf {
    xdg_runtime_dir().join("workspace-portal")
}

pub fn state_file_path(workspace_id: &str) -> PathBuf {
    state_file_path_in(state_root(), workspace_id)
}

pub fn log_file_path(workspace_id: &str) -> PathBuf {
    state_root()
        .join("logs")
        .join(format!("{workspace_id}.log"))
}

pub fn socket_path(workspace_id: &str) -> PathBuf {
    runtime_root().join(format!("{workspace_id}.sock"))
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
        env::temp_dir().join(format!(
            "workspace-portal-{prefix}-{}-{id}",
            std::process::id()
        ))
    }

    #[test]
    fn validate_entry_name_rejects_invalid_names() {
        for name in ["", ".", "..", "a/b", "a/../b"] {
            assert!(
                matches!(validate_entry_name(name), Err(Error::InvalidEntryName(value)) if value == name)
            );
        }
        assert!(validate_entry_name("valid-name_1").is_ok());
    }

    #[test]
    fn discover_workspace_can_use_registry_state() {
        let workspace = unique_path("paths-registry-workspace");
        let child = workspace.join("nested");
        fs::create_dir_all(&child).unwrap();

        let state_root = unique_path("paths-state-root");
        let workspace_id = workspace_id(&workspace);
        let registry_path = state_file_path_in(&state_root, &workspace_id);
        let state = PortalState::new(
            workspace.clone(),
            workspace_id.clone(),
            workspace.join("socket.sock"),
        );
        state.write_atomic(&registry_path).unwrap();

        let discovered = discover_workspace_in(&child, &state_root).unwrap();
        assert_eq!(discovered, workspace.canonicalize().unwrap());

        let _ = fs::remove_dir_all(&workspace);
        let _ = fs::remove_dir_all(&state_root);
    }
}

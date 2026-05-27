use std::{collections::BTreeMap, collections::HashMap, fs::File, path::PathBuf};

use fuser::{FileHandle, INodeNo};

use crate::{
    error::{Error, Result},
    state::PortalState,
};

use super::{
    ROOT_INO,
    path::{PortalPath, child_portal_path},
    resolve::state_for_path,
};

#[derive(Debug)]
pub(crate) enum OpenHandleKind {
    File,
}

#[derive(Debug)]
pub(crate) struct OpenHandle {
    pub(crate) file: File,
    pub(crate) kind: OpenHandleKind,
    pub(crate) writable: bool,
}

#[derive(Debug)]
pub(crate) struct FuseRuntime {
    pub(crate) inode_paths: BTreeMap<u64, PortalPath>,
    pub(crate) path_inodes: HashMap<PortalPath, u64>,
    pub(crate) handles: BTreeMap<u64, OpenHandle>,
    pub(crate) next_inode: u64,
    pub(crate) next_handle: u64,
}

impl FuseRuntime {
    pub(crate) fn new() -> Self {
        let mut inode_paths = BTreeMap::new();
        let mut path_inodes = HashMap::new();
        inode_paths.insert(ROOT_INO.0, PortalPath::Root);
        path_inodes.insert(PortalPath::Root, ROOT_INO.0);

        Self {
            inode_paths,
            path_inodes,
            handles: BTreeMap::new(),
            next_inode: ROOT_INO.0 + 1,
            next_handle: 1,
        }
    }

    pub(crate) fn inode_for_path(&mut self, path: PortalPath) -> INodeNo {
        if let Some(ino) = self.path_inodes.get(&path).copied() {
            return INodeNo(ino);
        }

        let mut ino = self.next_inode;
        while ino == ROOT_INO.0 || self.inode_paths.contains_key(&ino) {
            ino = ino.saturating_add(1);
        }
        self.next_inode = ino.saturating_add(1);
        self.inode_paths.insert(ino, path.clone());
        self.path_inodes.insert(path, ino);
        INodeNo(ino)
    }

    pub(crate) fn path_for_inode(
        &mut self,
        state: &PortalState,
        ino: INodeNo,
    ) -> Option<PortalPath> {
        if ino == ROOT_INO {
            return Some(PortalPath::Root);
        }

        if let Some(path) = self.inode_paths.get(&ino.0).cloned() {
            return Some(path);
        }

        state.entries.values().find_map(|entry| {
            if entry_inode(&entry.name) == ino {
                let path = PortalPath::Entry {
                    name: entry.name.clone(),
                    relative: PathBuf::new(),
                };
                self.inode_paths.insert(ino.0, path.clone());
                self.path_inodes.insert(path.clone(), ino.0);
                Some(path)
            } else {
                None
            }
        })
    }

    pub(crate) fn cache_portal_path(&mut self, path: PortalPath) -> INodeNo {
        self.inode_for_path(path)
    }

    pub(crate) fn handle_file(
        &mut self,
        file: File,
        kind: OpenHandleKind,
        writable: bool,
    ) -> FileHandle {
        let fh = (1u64 << 63) | self.next_handle;
        self.next_handle = self.next_handle.saturating_add(1);
        self.handles.insert(
            fh,
            OpenHandle {
                file,
                kind,
                writable,
            },
        );
        FileHandle(fh)
    }

    pub(crate) fn resolve_parent_child(
        &mut self,
        state: &PortalState,
        parent: INodeNo,
        name: &std::ffi::OsString,
    ) -> Result<(PortalPath, super::path::ResolvedPortalPath)> {
        let parent_path = self
            .path_for_inode(state, parent)
            .ok_or_else(|| Error::InvalidPortalPath(format!("unknown inode: {}", parent.0)))?;
        let child_path = child_portal_path(&parent_path, name)?;
        let resolved = state_for_path(state, &child_path)?;
        Ok((child_path, resolved))
    }

    pub(crate) fn resolve_parent_child_writable(
        &mut self,
        state: &PortalState,
        parent: INodeNo,
        name: &std::ffi::OsString,
    ) -> Result<(PortalPath, super::path::ResolvedPortalPath)> {
        let (path, resolved) = self.resolve_parent_child(state, parent, name)?;
        super::resolve::ensure_writable_entry(&resolved.entry)?;
        if state.read_only_default {
            return Err(Error::PermissionDenied(
                "workspace mount is read-only".to_owned(),
            ));
        }
        Ok((path, resolved))
    }
}

pub(crate) fn entry_inode(name: &str) -> INodeNo {
    let hash = blake3::hash(name.as_bytes());
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&hash.as_bytes()[..8]);
    let ino = u64::from_le_bytes(bytes);
    if ino == ROOT_INO.0 {
        INodeNo(ROOT_INO.0 + 1)
    } else {
        INodeNo(ino)
    }
}

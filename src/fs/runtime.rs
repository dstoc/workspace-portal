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
    pub(crate) ino: INodeNo,
    pub(crate) file: File,
    pub(crate) kind: OpenHandleKind,
    pub(crate) writable: bool,
}

#[derive(Debug)]
pub(crate) struct FuseRuntime {
    pub(crate) inode_paths: BTreeMap<u64, PortalPath>,
    pub(crate) path_inodes: HashMap<PortalPath, u64>,
    /// Outstanding kernel lookup count per inode. The FUSE protocol increments
    /// this by one for every reply that returns an inode to the kernel (LOOKUP,
    /// CREATE, MKDIR, SYMLINK, LINK) and FORGET decrements it by `nlookup`. An
    /// inode mapping must only be dropped once its count reaches zero.
    pub(crate) lookups: HashMap<u64, u64>,
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
            lookups: HashMap::new(),
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

    /// Ensures an inode exists for `path` without touching its lookup count.
    ///
    /// Used for inodes that are cached but *not* handed to the kernel as a new
    /// lookup reference (plain `readdir` entries, `getattr` re-resolution). Use
    /// [`remember_lookup`](Self::remember_lookup) for replies that the kernel
    /// counts as a lookup.
    pub(crate) fn cache_portal_path(&mut self, path: PortalPath) -> INodeNo {
        self.inode_for_path(path)
    }

    /// Resolves the inode for `path` and records one outstanding kernel lookup
    /// reference for it. Call this for every reply that returns an inode to the
    /// kernel (LOOKUP, CREATE, MKDIR, SYMLINK, LINK); the matching FORGET will
    /// release the reference.
    pub(crate) fn remember_lookup(&mut self, path: PortalPath) -> INodeNo {
        let ino = self.inode_for_path(path);
        if ino != ROOT_INO {
            *self.lookups.entry(ino.0).or_insert(0) += 1;
        }
        ino
    }

    /// Releases `nlookup` kernel lookup references for `ino`, dropping the
    /// cached mapping only once the count reaches zero. Honouring `nlookup` is
    /// required by the FUSE protocol: a single FORGET may release several
    /// references at once, and the kernel may keep using an inode whose count
    /// is still positive. Dropping it early makes later operations against it
    /// (e.g. `create` of a lock file under a forgotten directory) fail.
    pub(crate) fn forget_inode(&mut self, ino: INodeNo, nlookup: u64) {
        if ino == ROOT_INO {
            return;
        }

        // No tracked lookups: the kernel holds no outstanding reference, so
        // releasing any cached mapping is safe.
        if let Some(count) = self.lookups.get_mut(&ino.0) {
            *count = count.saturating_sub(nlookup);
            if *count > 0 {
                return;
            }
            self.lookups.remove(&ino.0);
        }

        if let Some(path) = self.inode_paths.remove(&ino.0) {
            self.path_inodes.remove(&path);
        }
    }

    pub(crate) fn rename_cached_subtree(&mut self, source: &PortalPath, destination: &PortalPath) {
        let mut stale_destination_paths = Vec::new();
        let mut renamed = Vec::new();

        for (ino, path) in &self.inode_paths {
            if *ino == ROOT_INO.0 {
                continue;
            }

            if path_has_prefix(path, destination) {
                stale_destination_paths.push((*ino, path.clone()));
            }
            if path_has_prefix(path, source) {
                renamed.push((*ino, replace_prefix(path, source, destination)));
            }
        }

        for (ino, old_path) in stale_destination_paths {
            self.inode_paths.remove(&ino);
            self.path_inodes.remove(&old_path);
            self.lookups.remove(&ino);
        }

        for (ino, new_path) in renamed {
            if let Some(old_path) = self.inode_paths.insert(ino, new_path.clone()) {
                self.path_inodes.remove(&old_path);
            }
            self.path_inodes.insert(new_path, ino);
        }
    }

    pub(crate) fn handle_file(
        &mut self,
        ino: INodeNo,
        file: File,
        kind: OpenHandleKind,
        writable: bool,
    ) -> FileHandle {
        let fh = (1u64 << 63) | self.next_handle;
        self.next_handle = self.next_handle.saturating_add(1);
        self.handles.insert(
            fh,
            OpenHandle {
                ino,
                file,
                kind,
                writable,
            },
        );
        FileHandle(fh)
    }

    /// Returns metadata from any open handle for `ino`. Used by `getattr` to serve
    /// attributes for soft-revoked entries that still have open file descriptors.
    pub(crate) fn open_handle_metadata(&self, ino: INodeNo) -> Option<std::fs::Metadata> {
        self.handles
            .values()
            .find(|h| h.ino == ino)
            .and_then(|h| h.file.metadata().ok())
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

fn path_has_prefix(path: &PortalPath, prefix: &PortalPath) -> bool {
    match (path, prefix) {
        (_, PortalPath::Root) => true,
        (
            PortalPath::Entry {
                name: path_name,
                relative: path_relative,
            },
            PortalPath::Entry {
                name: prefix_name,
                relative: prefix_relative,
            },
        ) => path_name == prefix_name && path_relative.starts_with(prefix_relative),
        _ => false,
    }
}

fn replace_prefix(path: &PortalPath, source: &PortalPath, destination: &PortalPath) -> PortalPath {
    match (path, source, destination) {
        (
            PortalPath::Entry {
                name: path_name,
                relative: path_relative,
            },
            PortalPath::Entry {
                name: source_name,
                relative: source_relative,
            },
            PortalPath::Entry {
                name: destination_name,
                relative: destination_relative,
            },
        ) if path_name == source_name && path_relative.starts_with(source_relative) => {
            let suffix = path_relative
                .strip_prefix(source_relative)
                .expect("prefix already checked");
            let mut relative = destination_relative.clone();
            relative.push(suffix);
            PortalPath::Entry {
                name: destination_name.clone(),
                relative,
            }
        }
        _ => path.clone(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn add_entry_replace_does_not_touch_open_handles() {
        use std::os::unix::io::AsRawFd;

        // Build a unique temp dir and file for this test.
        let pid = std::process::id();
        let tmp_dir =
            std::env::temp_dir().join(format!("workspace-portal-handle-preservation-{pid}"));
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let tmp_file = tmp_dir.join("held.txt");

        // 1. Build a PortalState with one "docs" entry (ReadWrite).
        let workspace = tmp_dir.join("workspace");
        let mut state = PortalState::new(
            workspace.clone(),
            "test-workspace-id".to_owned(),
            workspace.join("socket.sock"),
        );
        state
            .add_entry(
                crate::state::EntryRecord::new(
                    "docs",
                    tmp_dir.clone(),
                    crate::state::AccessMode::ReadWrite,
                ),
                false,
            )
            .unwrap();

        // 2. Construct a FuseRuntime and insert an OpenHandle with writable=true.
        let mut runtime = FuseRuntime::new();
        let file = std::fs::File::create(&tmp_file).unwrap();
        let fd = file.as_raw_fd();
        runtime.handles.insert(
            1u64,
            OpenHandle {
                ino: INodeNo(2),
                file,
                kind: OpenHandleKind::File,
                writable: true,
            },
        );

        // 3. Flip "docs" from ReadWrite → ReadOnly via add_entry(replace=true).
        state
            .add_entry(
                crate::state::EntryRecord::new(
                    "docs",
                    tmp_dir.clone(),
                    crate::state::AccessMode::ReadOnly,
                ),
                true,
            )
            .unwrap();

        // 4. Assert the entry's mode is now ReadOnly …
        assert_eq!(
            state.entry("docs").unwrap().mode,
            crate::state::AccessMode::ReadOnly,
            "entry mode should have been flipped to ReadOnly"
        );
        // … and the stored handle is completely untouched.
        let handle = runtime.handles.get(&1u64).expect("handle must still exist");
        assert!(
            handle.writable,
            "handle.writable must remain true after mode flip"
        );
        assert_eq!(
            handle.file.as_raw_fd(),
            fd,
            "handle fd must be unchanged after mode flip"
        );

        // 5. Clean up.
        drop(runtime); // drops the File, closing the fd
        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn rename_cached_subtree_preserves_inodes_and_moves_descendants() {
        let mut runtime = FuseRuntime::new();
        let source = PortalPath::Entry {
            name: "docs".to_owned(),
            relative: PathBuf::from("old"),
        };
        let child = PortalPath::Entry {
            name: "docs".to_owned(),
            relative: PathBuf::from("old/nested/file.txt"),
        };
        let destination = PortalPath::Entry {
            name: "docs".to_owned(),
            relative: PathBuf::from("new"),
        };

        let source_ino = runtime.cache_portal_path(source.clone());
        let child_ino = runtime.cache_portal_path(child.clone());

        runtime.rename_cached_subtree(&source, &destination);

        assert_eq!(runtime.path_inodes.get(&source), None);
        assert_eq!(runtime.path_inodes.get(&child), None);
        assert_eq!(runtime.inode_paths.get(&source_ino.0), Some(&destination));
        assert_eq!(
            runtime.inode_paths.get(&child_ino.0),
            Some(&PortalPath::Entry {
                name: "docs".to_owned(),
                relative: PathBuf::from("new/nested/file.txt"),
            })
        );
    }

    // Deterministic reproduction of the `git push` lock-create regression.
    //
    // The FUSE protocol reference-counts inodes: every LOOKUP reply (and
    // CREATE/MKDIR/etc. that returns an inode) increments the kernel's lookup
    // count by one, and FORGET carries an `nlookup` that decrements it. The
    // daemon must only drop an inode once its cumulative lookup count reaches
    // zero. `forget` (src/fs/callbacks.rs) instead drops the mapping on the
    // first FORGET regardless of `nlookup`, so an inode the kernel still
    // references gets evicted early. Because `path_for_inode` cannot re-derive
    // a *nested* path (only top-level entry inodes), a later operation against
    // that inode — e.g. `create` of `refs/remotes/origin/main.lock` — fails
    // with EACCES even though the entry is read-write.
    //
    // This exercises the inode table directly (no real mount), so it is not
    // subject to the kernel's nondeterministic FORGET scheduling that makes an
    // end-to-end `git push` reproduction flaky.
    #[test]
    fn forget_honours_lookup_count_for_nested_inode() {
        let mut runtime = FuseRuntime::new();

        // `path_for_inode` consults `state` only when re-deriving a top-level
        // entry inode; a cached nested inode never reaches that branch, so an
        // empty state is sufficient here.
        let workspace = PathBuf::from("/tmp/workspace-portal-forget-refcount");
        let state = PortalState::new(
            workspace.clone(),
            "test-workspace-id".to_owned(),
            workspace.join("socket.sock"),
        );

        // A nested directory inode, e.g. `.git/refs/remotes/origin`.
        let nested = PortalPath::Entry {
            name: "docs".to_owned(),
            relative: PathBuf::from(".git/refs/remotes/origin"),
        };

        // The kernel looks the directory up twice — say, once while reading
        // existing refs and again while preparing to create `main.lock` under
        // it. Each LOOKUP reply increments the lookup count, so it is now 2.
        let ino = runtime.remember_lookup(nested.clone());
        let ino_again = runtime.remember_lookup(nested.clone());
        assert_eq!(ino, ino_again, "the same path must map to the same inode");

        // The kernel forgets ONE of those two references (nlookup = 1). The
        // count drops 2 -> 1; the inode is still referenced and the kernel may
        // still issue a CREATE against it, so the daemon must keep resolving it.
        runtime.forget_inode(ino, 1);
        assert!(
            runtime.path_for_inode(&state, ino).is_some(),
            "a nested inode forgotten fewer times than it was looked up must \
             still resolve; dropping it early makes the next operation against \
             it (create of refs/remotes/origin/main.lock) fail with EACCES"
        );

        // After the matching second forget the count reaches 0 and the inode
        // may be evicted.
        runtime.forget_inode(ino, 1);
        assert!(
            runtime.path_for_inode(&state, ino).is_none(),
            "once the lookup count reaches zero the nested inode should be dropped"
        );
    }
}

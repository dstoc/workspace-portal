use std::{
    cmp,
    fs::{self, OpenOptions},
    os::unix::fs::{FileExt, OpenOptionsExt, PermissionsExt},
    time::SystemTime,
};

use fuser::{
    BsdFileFlags, CopyFileRangeFlags, Errno, FileHandle, FileType, Filesystem, FopenFlags,
    Generation, INodeNo, IoctlFlags, LockOwner, OpenFlags, PollEvents, PollFlags, PollNotifier,
    RenameFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry,
    ReplyIoctl, ReplyOpen, ReplyPoll, ReplyStatfs, ReplyWrite, ReplyXattr, Request, TimeOrNow,
    WriteFlags,
};

use crate::{
    error::{Error, Result},
    state::PortalState,
};
use tracing::debug;

use super::{
    PortalFs, ROOT_INO, TTL,
    attr::{attr_from_metadata, current_attr, directory_attr, file_attr, root_attr},
    path::{PortalPath, child_portal_path, portal_path_to_pathbuf},
    resolve::{entry_is_read_only, errno_from_error, state_for_path, validate_rename},
    runtime::OpenHandleKind,
};

fn dir_entries(
    runtime: &mut super::runtime::FuseRuntime,
    state: &PortalState,
    path: &PortalPath,
) -> Result<Vec<(INodeNo, FileType, String, PortalPath)>> {
    match path {
        PortalPath::Root => {
            let mut entries = Vec::with_capacity(state.entries.len() + 2);
            entries.push((
                ROOT_INO,
                FileType::Directory,
                ".".to_owned(),
                PortalPath::Root,
            ));
            entries.push((
                ROOT_INO,
                FileType::Directory,
                "..".to_owned(),
                PortalPath::Root,
            ));
            for entry in state.entries.values() {
                let entry_path = PortalPath::Entry {
                    name: entry.name.clone(),
                    relative: std::path::PathBuf::new(),
                };
                let ino = runtime.cache_portal_path(entry_path.clone());
                entries.push((ino, FileType::Directory, entry.name.clone(), entry_path));
            }
            Ok(entries)
        }
        PortalPath::Entry { .. } => {
            let resolved = state_for_path(state, path)?;
            let metadata = fs::symlink_metadata(&resolved.target)?;
            if !metadata.file_type().is_dir() {
                return Err(Error::TargetNotDirectory(resolved.target));
            }

            let mut entries = Vec::new();
            entries.push((
                runtime.cache_portal_path(path.clone()),
                FileType::Directory,
                ".".to_owned(),
                path.clone(),
            ));
            let parent = super::path::parent_portal_path(path).unwrap_or(PortalPath::Root);
            let parent_ino = runtime.cache_portal_path(parent.clone());
            entries.push((parent_ino, FileType::Directory, "..".to_owned(), parent));

            for child in fs::read_dir(&resolved.target)? {
                let child = child?;
                let name = child.file_name();
                let child_path = child_portal_path(path, &name)?;
                let child_metadata = child.metadata()?;
                let child_ino = runtime.cache_portal_path(child_path.clone());
                entries.push((
                    child_ino,
                    super::attr::file_type_from_metadata(&child_metadata),
                    name.to_string_lossy().into_owned(),
                    child_path,
                ));
            }

            Ok(entries)
        }
    }
}

fn open_path(
    runtime: &mut super::runtime::FuseRuntime,
    state: &PortalState,
    path: &PortalPath,
    ino: fuser::INodeNo,
    flags: i32,
    mode: u32,
) -> Result<FileHandle> {
    let resolved = state_for_path(state, path)?;
    let metadata = fs::symlink_metadata(&resolved.target)?;
    if metadata.file_type().is_dir() {
        return Err(Error::TargetNotDirectory(resolved.target));
    }

    let writable = (flags & libc::O_ACCMODE) != libc::O_RDONLY;
    if writable {
        super::resolve::ensure_writable_entry(&resolved.entry)?;
        if state.read_only_default {
            return Err(Error::PermissionDenied(
                "workspace mount is read-only".to_owned(),
            ));
        }
    }

    let mut options = OpenOptions::new();
    options.read(true);
    if writable {
        options.write(true);
    }
    if flags & libc::O_APPEND != 0 {
        options.append(true);
    }
    if flags & libc::O_TRUNC != 0 {
        options.truncate(true);
    }
    options.mode(mode & 0o7777);

    let file = options.open(&resolved.target)?;
    Ok(runtime.handle_file(ino, file, OpenHandleKind::File, writable))
}

fn copy_file_range_fallback(
    source: &fs::File,
    offset_in: u64,
    destination: &fs::File,
    offset_out: u64,
    len: u64,
) -> Result<u64> {
    let mut copied = 0u64;
    let mut buffer = vec![0u8; cmp::min(len, 1024 * 1024) as usize];

    while copied < len {
        let chunk_len = cmp::min((len - copied) as usize, buffer.len());
        let read = source.read_at(&mut buffer[..chunk_len], offset_in + copied)?;
        if read == 0 {
            break;
        }

        let mut written = 0usize;
        while written < read {
            let count = destination
                .write_at(&buffer[written..read], offset_out + copied + written as u64)?;
            if count == 0 {
                return Err(Error::Io(std::io::Error::from_raw_os_error(libc::EIO)));
            }
            written += count;
        }

        copied += read as u64;
    }

    Ok(copied)
}

impl Filesystem for PortalFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &std::ffi::OsStr, reply: ReplyEntry) {
        let state = self.state.read().unwrap().clone();
        let mut runtime = self.runtime.lock().unwrap();

        if parent == ROOT_INO {
            let Some(name) = name.to_str() else {
                debug!(
                    "lookup parent={} name={:?} -> ENOENT (non-utf8 root entry)",
                    parent.0, name
                );
                reply.error(Errno::ENOENT);
                return;
            };
            let Some(entry) = state.entries.get(name).cloned() else {
                debug!(
                    "lookup parent={} name={} -> ENOENT (root entry missing)",
                    parent.0, name
                );
                reply.error(Errno::ENOENT);
                return;
            };
            let path = PortalPath::Entry {
                name: entry.name.clone(),
                relative: std::path::PathBuf::new(),
            };
            let ino = runtime.cache_portal_path(path);
            let metadata = match fs::symlink_metadata(&entry.target) {
                Ok(metadata) => metadata,
                Err(_) => {
                    debug!(
                        "lookup parent={} name={} -> ENOENT (target missing: {})",
                        parent.0,
                        name,
                        entry.target.display()
                    );
                    reply.error(Errno::ENOENT);
                    return;
                }
            };
            let attr = attr_from_metadata(
                ino,
                &metadata,
                entry_is_read_only(&entry, state.read_only_default),
                0,
            );
            debug!(
                "lookup parent={} name={} -> ino={} ok",
                parent.0, name, ino.0
            );
            reply.entry(&TTL, &attr, Generation(entry.generation));
            return;
        }

        let name = name.to_os_string();
        let Some(parent_path) = runtime.path_for_inode(&state, parent) else {
            debug!(
                "lookup parent={} name={:?} -> ENOENT (unknown parent inode)",
                parent.0, name
            );
            reply.error(Errno::ENOENT);
            return;
        };
        let Ok(child_path) = child_portal_path(&parent_path, &name) else {
            debug!(
                "lookup parent={} name={:?} parent_path={:?} -> ENOENT (invalid child path)",
                parent.0, name, parent_path
            );
            reply.error(Errno::ENOENT);
            return;
        };
        let resolved = match state_for_path(&state, &child_path) {
            Ok(resolved) => resolved,
            Err(err) => {
                debug!(
                    "lookup parent={} name={:?} child_path={:?} -> errno={:?} err={}",
                    parent.0,
                    name,
                    child_path,
                    errno_from_error(&err),
                    err
                );
                reply.error(errno_from_error(&err));
                return;
            }
        };
        let metadata = match fs::symlink_metadata(&resolved.target) {
            Ok(metadata) => metadata,
            Err(_) => {
                debug!(
                    "lookup parent={} name={:?} child_path={:?} target={} -> ENOENT",
                    parent.0,
                    name,
                    child_path,
                    resolved.target.display()
                );
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let ino = runtime.cache_portal_path(child_path.clone());
        let attr = attr_from_metadata(
            ino,
            &metadata,
            entry_is_read_only(&resolved.entry, state.read_only_default),
            0,
        );
        debug!(
            "lookup parent={} name={:?} child_path={:?} -> ino={} ok",
            parent.0, name, child_path, ino.0
        );
        reply.entry(&TTL, &attr, Generation(resolved.entry.generation));
    }

    fn forget(&self, _req: &Request, ino: INodeNo, nlookup: u64) {
        debug!("forget ino={} nlookup={}", ino.0, nlookup);
        let mut runtime = self.runtime.lock().unwrap();
        runtime.forget_inode(ino);
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let state = self.state.read().unwrap().clone();
        if ino == ROOT_INO {
            match root_attr(&state) {
                Ok(attr) => {
                    debug!("getattr ino={} -> ok (root)", ino.0);
                    reply.attr(&TTL, &attr)
                }
                Err(err) => {
                    debug!("getattr ino={} -> EIO err={}", ino.0, err);
                    reply.error(Errno::EIO)
                }
            }
            return;
        }

        let mut runtime = self.runtime.lock().unwrap();
        let Some(path) = runtime.path_for_inode(&state, ino) else {
            debug!("getattr ino={} -> ENOENT (unknown inode)", ino.0);
            reply.error(Errno::ENOENT);
            return;
        };
        match file_attr(&mut runtime, &state, &path)
            .or_else(|_| directory_attr(&mut runtime, &state, &path))
        {
            Ok(attr) => {
                debug!("getattr ino={} path={:?} -> ok", ino.0, path);
                reply.attr(&TTL, &attr)
            }
            Err(err) => {
                // For soft revocation: the entry was removed but an open fd remains.
                // Serve attributes via fstat on the open handle so the kernel does not
                // abort in-flight reads with ENOENT.
                if matches!(&err, Error::EntryNotFound(_) | Error::TargetNotFound(_)) {
                    if let Some(metadata) = runtime.open_handle_metadata(ino) {
                        let attr = attr_from_metadata(ino, &metadata, false, 0);
                        debug!("getattr ino={} path={:?} -> ok (revoked, fstat fallback)", ino.0, path);
                        reply.attr(&TTL, &attr);
                        return;
                    }
                }
                debug!(
                    "getattr ino={} path={:?} -> errno={:?} err={}",
                    ino.0,
                    path,
                    errno_from_error(&err),
                    err
                );
                reply.error(errno_from_error(&err))
            }
        }
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let state = self.state.read().unwrap().clone();
        let mut runtime = self.runtime.lock().unwrap();
        let Some(path) = runtime.path_for_inode(&state, ino) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let resolved = match state_for_path(&state, &path) {
            Ok(resolved) => resolved,
            Err(_) => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        if entry_is_read_only(&resolved.entry, state.read_only_default) {
            reply.error(Errno::EROFS);
            return;
        }
        if uid.is_some() || gid.is_some() {
            reply.error(Errno::EPERM);
            return;
        }

        if let Some(mode) = mode {
            if let Err(err) =
                fs::set_permissions(&resolved.target, fs::Permissions::from_mode(mode & 0o7777))
            {
                reply.error(Errno::from_i32(err.raw_os_error().unwrap_or(libc::EIO)));
                return;
            }
        }

        if let Some(size) = size {
            let result = if let Some(fh) = fh {
                match runtime.handles.get(&fh.0) {
                    Some(handle) if handle.writable => handle.file.set_len(size),
                    Some(_) => {
                        reply.error(Errno::EPERM);
                        return;
                    }
                    None => {
                        reply.error(Errno::EBADF);
                        return;
                    }
                }
            } else {
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&resolved.target)
                    .and_then(|file| file.set_len(size))
            };
            if let Err(err) = result {
                reply.error(Errno::from_i32(err.raw_os_error().unwrap_or(libc::EIO)));
                return;
            }
        }

        match current_attr(&mut runtime, &state, &path) {
            Ok(attr) => reply.attr(&TTL, &attr),
            Err(_) => reply.error(Errno::EIO),
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let state = self.state.read().unwrap().clone();
        let mut runtime = self.runtime.lock().unwrap();
        let path = if ino == ROOT_INO {
            PortalPath::Root
        } else if let Some(path) = runtime.path_for_inode(&state, ino) {
            path
        } else {
            debug!(
                "readdir ino={} offset={} -> ENOENT (unknown inode)",
                ino.0, offset
            );
            reply.error(Errno::ENOENT);
            return;
        };

        let Ok(entries) = dir_entries(&mut runtime, &state, &path) else {
            debug!(
                "readdir ino={} path={:?} offset={} -> ENOTDIR",
                ino.0, path, offset
            );
            reply.error(Errno::ENOTDIR);
            return;
        };

        debug!(
            "readdir ino={} path={:?} offset={} entries={}",
            ino.0,
            path,
            offset,
            entries.len()
        );

        for (idx, (entry_ino, kind, name, _)) in
            entries.into_iter().enumerate().skip(offset as usize)
        {
            if reply.add(entry_ino, (idx + 1) as u64, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let state = self.state.read().unwrap().clone();
        if ino == ROOT_INO {
            reply.opened(FileHandle(0), FopenFlags::empty());
            return;
        }

        let mut runtime = self.runtime.lock().unwrap();
        let Some(path) = runtime.path_for_inode(&state, ino) else {
            reply.error(Errno::ENOENT);
            return;
        };

        let resolved = match state_for_path(&state, &path) {
            Ok(resolved) => resolved,
            Err(_) => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        match fs::symlink_metadata(&resolved.target) {
            Ok(metadata) if metadata.file_type().is_dir() => {
                reply.opened(FileHandle(0), FopenFlags::empty())
            }
            Ok(_) => reply.error(Errno::ENOTDIR),
            Err(_) => reply.error(Errno::ENOENT),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let state = self.state.read().unwrap().clone();
        let mut runtime = self.runtime.lock().unwrap();
        let Some(path) = runtime.path_for_inode(&state, ino) else {
            debug!(
                "open ino={} flags={:#x} -> ENOENT (unknown inode)",
                ino.0, flags.0
            );
            reply.error(Errno::ENOENT);
            return;
        };
        match open_path(&mut runtime, &state, &path, ino, flags.0, 0) {
            Ok(fh) => {
                debug!(
                    "open ino={} path={:?} flags={:#x} -> fh={} ok",
                    ino.0, path, flags.0, fh.0
                );
                reply.opened(fh, FopenFlags::empty())
            }
            Err(Error::TargetNotDirectory(target)) => {
                debug!(
                    "open ino={} path={:?} flags={:#x} target={} -> EISDIR",
                    ino.0,
                    path,
                    flags.0,
                    target.display()
                );
                reply.error(Errno::EISDIR)
            }
            Err(Error::PermissionDenied(err)) => {
                debug!(
                    "open ino={} path={:?} flags={:#x} -> EROFS err={}",
                    ino.0, path, flags.0, err
                );
                reply.error(Errno::EROFS)
            }
            Err(err) => {
                debug!(
                    "open ino={} path={:?} flags={:#x} -> errno={:?} err={}",
                    ino.0,
                    path,
                    flags.0,
                    errno_from_error(&err),
                    err
                );
                reply.error(errno_from_error(&err))
            }
        }
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &std::ffi::OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let state = self.state.read().unwrap().clone();
        if parent == ROOT_INO {
            reply.error(Errno::EPERM);
            return;
        }

        let mut runtime = self.runtime.lock().unwrap();
        let name = name.to_os_string();
        let Ok((path, resolved)) = runtime.resolve_parent_child_writable(&state, parent, &name)
        else {
            reply.error(Errno::EACCES);
            return;
        };

        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        options.mode((mode & !umask) & 0o7777);
        if flags & libc::O_TRUNC != 0 {
            options.truncate(true);
        }

        match options.open(&resolved.target) {
            Ok(file) => {
                let ino = runtime.cache_portal_path(path.clone());
                let fh = runtime.handle_file(ino, file, OpenHandleKind::File, true);
                let metadata = match fs::symlink_metadata(&resolved.target) {
                    Ok(metadata) => metadata,
                    Err(err) => {
                        reply.error(Errno::from_i32(err.raw_os_error().unwrap_or(libc::EIO)));
                        return;
                    }
                };
                let attr = attr_from_metadata(
                    ino,
                    &metadata,
                    entry_is_read_only(&resolved.entry, state.read_only_default),
                    0,
                );
                reply.created(
                    &TTL,
                    &attr,
                    Generation(resolved.entry.generation),
                    fh,
                    FopenFlags::empty(),
                );
            }
            Err(err) => reply.error(Errno::from_i32(err.raw_os_error().unwrap_or(libc::EIO))),
        }
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &std::ffi::OsStr,
        mode: u32,
        umask: u32,
        reply: ReplyEntry,
    ) {
        let state = self.state.read().unwrap().clone();
        if parent == ROOT_INO {
            reply.error(Errno::EPERM);
            return;
        }

        let mut runtime = self.runtime.lock().unwrap();
        let name = name.to_os_string();
        let Ok((path, resolved)) = runtime.resolve_parent_child_writable(&state, parent, &name)
        else {
            reply.error(Errno::EACCES);
            return;
        };

        match fs::create_dir(&resolved.target) {
            Ok(()) => {
                if let Err(err) = fs::set_permissions(
                    &resolved.target,
                    fs::Permissions::from_mode((mode & !umask) & 0o7777),
                ) {
                    reply.error(Errno::from_i32(err.raw_os_error().unwrap_or(libc::EIO)));
                    return;
                }
                let metadata = match fs::symlink_metadata(&resolved.target) {
                    Ok(metadata) => metadata,
                    Err(err) => {
                        reply.error(Errno::from_i32(err.raw_os_error().unwrap_or(libc::EIO)));
                        return;
                    }
                };
                let ino = runtime.cache_portal_path(path);
                let attr = attr_from_metadata(
                    ino,
                    &metadata,
                    entry_is_read_only(&resolved.entry, state.read_only_default),
                    0,
                );
                reply.entry(&TTL, &attr, Generation(resolved.entry.generation));
            }
            Err(err) => reply.error(Errno::from_i32(err.raw_os_error().unwrap_or(libc::EIO))),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &std::ffi::OsStr, reply: ReplyEmpty) {
        let state = self.state.read().unwrap().clone();
        if parent == ROOT_INO {
            reply.error(Errno::EPERM);
            return;
        }

        let mut runtime = self.runtime.lock().unwrap();
        let name = name.to_os_string();
        let Ok((_, resolved)) = runtime.resolve_parent_child_writable(&state, parent, &name) else {
            reply.error(Errno::EACCES);
            return;
        };

        match fs::symlink_metadata(&resolved.target) {
            Ok(metadata) if metadata.file_type().is_dir() => reply.error(Errno::EISDIR),
            Ok(_) => match fs::remove_file(&resolved.target) {
                Ok(()) => reply.ok(),
                Err(err) => reply.error(Errno::from_i32(err.raw_os_error().unwrap_or(libc::EIO))),
            },
            Err(_) => reply.error(Errno::ENOENT),
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &std::ffi::OsStr, reply: ReplyEmpty) {
        let state = self.state.read().unwrap().clone();
        if parent == ROOT_INO {
            reply.error(Errno::EPERM);
            return;
        }

        let mut runtime = self.runtime.lock().unwrap();
        let name = name.to_os_string();
        let Ok((_, resolved)) = runtime.resolve_parent_child_writable(&state, parent, &name) else {
            reply.error(Errno::EACCES);
            return;
        };

        match fs::remove_dir(&resolved.target) {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(Errno::from_i32(err.raw_os_error().unwrap_or(libc::EIO))),
        }
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &std::ffi::OsStr,
        newparent: INodeNo,
        newname: &std::ffi::OsStr,
        _flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        let state = self.state.read().unwrap().clone();
        let mut runtime = self.runtime.lock().unwrap();
        let source_name = name.to_os_string();
        let target_name = newname.to_os_string();

        let Ok((source_path, source_resolved)) =
            runtime.resolve_parent_child_writable(&state, parent, &source_name)
        else {
            debug!(
                "rename parent={} name={:?} newparent={} newname={:?} -> EACCES (source)",
                parent.0, source_name, newparent.0, target_name
            );
            reply.error(Errno::EACCES);
            return;
        };
        let Ok((target_path, target_resolved)) =
            runtime.resolve_parent_child_writable(&state, newparent, &target_name)
        else {
            debug!(
                "rename parent={} name={:?} newparent={} newname={:?} -> EACCES (target)",
                parent.0, source_name, newparent.0, target_name
            );
            reply.error(Errno::EACCES);
            return;
        };

        match validate_rename(
            &state,
            portal_path_to_pathbuf(&source_path),
            portal_path_to_pathbuf(&target_path),
        ) {
            Ok(_) => match fs::rename(&source_resolved.target, &target_resolved.target) {
                Ok(()) => {
                    runtime.rename_cached_subtree(&source_path, &target_path);
                    debug!(
                        "rename parent={} name={:?} newparent={} newname={:?} source={} target={} -> ok",
                        parent.0,
                        source_name,
                        newparent.0,
                        target_name,
                        source_resolved.target.display(),
                        target_resolved.target.display()
                    );
                    reply.ok()
                }
                Err(err) => {
                    debug!(
                        "rename parent={} name={:?} newparent={} newname={:?} source={} target={} -> errno={}",
                        parent.0,
                        source_name,
                        newparent.0,
                        target_name,
                        source_resolved.target.display(),
                        target_resolved.target.display(),
                        err.raw_os_error().unwrap_or(libc::EIO)
                    );
                    reply.error(Errno::from_i32(err.raw_os_error().unwrap_or(libc::EIO)))
                }
            },
            Err(Error::PermissionDenied(err)) => {
                debug!(
                    "rename parent={} name={:?} newparent={} newname={:?} -> EPERM err={}",
                    parent.0, source_name, newparent.0, target_name, err
                );
                reply.error(Errno::EPERM)
            }
            Err(Error::InvalidPortalPath(err)) => {
                debug!(
                    "rename parent={} name={:?} newparent={} newname={:?} -> EINVAL err={}",
                    parent.0, source_name, newparent.0, target_name, err
                );
                reply.error(Errno::EINVAL)
            }
            Err(Error::EntryNotFound(err)) => {
                debug!(
                    "rename parent={} name={:?} newparent={} newname={:?} -> ENOENT entry={}",
                    parent.0, source_name, newparent.0, target_name, err
                );
                reply.error(Errno::ENOENT)
            }
            Err(Error::TargetNotFound(err)) => {
                debug!(
                    "rename parent={} name={:?} newparent={} newname={:?} -> ENOENT target={}",
                    parent.0,
                    source_name,
                    newparent.0,
                    target_name,
                    err.display()
                );
                reply.error(Errno::ENOENT)
            }
            Err(err) => {
                debug!(
                    "rename parent={} name={:?} newparent={} newname={:?} -> EIO err={}",
                    parent.0, source_name, newparent.0, target_name, err
                );
                reply.error(Errno::EIO)
            }
        }
    }

    fn link(
        &self,
        _req: &Request,
        ino: INodeNo,
        newparent: INodeNo,
        newname: &std::ffi::OsStr,
        reply: ReplyEntry,
    ) {
        let state = self.state.read().unwrap().clone();
        if newparent == ROOT_INO {
            reply.error(Errno::EPERM);
            return;
        }

        let mut runtime = self.runtime.lock().unwrap();
        let Some(source_path) = runtime.path_for_inode(&state, ino) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let source_resolved = match state_for_path(&state, &source_path) {
            Ok(resolved) => resolved,
            Err(err) => {
                reply.error(errno_from_error(&err));
                return;
            }
        };

        let target_name = newname.to_os_string();
        let Ok((target_path, target_resolved)) =
            runtime.resolve_parent_child_writable(&state, newparent, &target_name)
        else {
            reply.error(Errno::EACCES);
            return;
        };

        if source_resolved.entry.name != target_resolved.entry.name {
            reply.error(Errno::EXDEV);
            return;
        }

        match fs::symlink_metadata(&source_resolved.target) {
            Ok(metadata) if metadata.file_type().is_dir() => {
                reply.error(Errno::EPERM);
                return;
            }
            Ok(_) => {}
            Err(err) => {
                reply.error(Errno::from_i32(err.raw_os_error().unwrap_or(libc::EIO)));
                return;
            }
        }

        match fs::hard_link(&source_resolved.target, &target_resolved.target) {
            Ok(()) => {
                let metadata = match fs::symlink_metadata(&target_resolved.target) {
                    Ok(metadata) => metadata,
                    Err(err) => {
                        reply.error(Errno::from_i32(err.raw_os_error().unwrap_or(libc::EIO)));
                        return;
                    }
                };
                let ino = runtime.cache_portal_path(target_path);
                let attr = attr_from_metadata(
                    ino,
                    &metadata,
                    entry_is_read_only(&target_resolved.entry, state.read_only_default),
                    0,
                );
                reply.entry(&TTL, &attr, Generation(target_resolved.entry.generation));
            }
            Err(err) => reply.error(Errno::from_i32(err.raw_os_error().unwrap_or(libc::EIO))),
        }
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        if ino == ROOT_INO {
            reply.error(Errno::EISDIR);
            return;
        }

        // Read directly from the open file handle. We deliberately skip re-resolving
        // the path through the current state so that soft revocation (removing an
        // entry while a file descriptor is still open) does not break in-flight reads:
        // the kernel may have forgotten the inode after the lookup returned ENOENT, but
        // the file handle remains valid until RELEASE.
        let runtime = self.runtime.lock().unwrap();
        let Some(handle) = runtime.handles.get(&fh.0) else {
            reply.error(Errno::EBADF);
            return;
        };
        if !matches!(handle.kind, OpenHandleKind::File) {
            reply.error(Errno::EISDIR);
            return;
        }

        let mut buffer = vec![0u8; size as usize];
        match handle.file.read_at(&mut buffer, offset) {
            Ok(read) => reply.data(&buffer[..read]),
            Err(err) => reply.error(Errno::from_i32(err.raw_os_error().unwrap_or(libc::EIO))),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        let state = self.state.read().unwrap().clone();
        if ino == ROOT_INO {
            reply.error(Errno::EINVAL);
            return;
        }

        let mut runtime = self.runtime.lock().unwrap();
        let Some(path) = runtime.path_for_inode(&state, ino) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let resolved = match state_for_path(&state, &path) {
            Ok(resolved) => resolved,
            Err(_) => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        match fs::read_link(&resolved.target) {
            Ok(target) => {
                #[cfg(unix)]
                {
                    use std::os::unix::ffi::OsStrExt;
                    reply.data(target.as_os_str().as_bytes());
                }
                #[cfg(not(unix))]
                {
                    reply.data(target.to_string_lossy().as_bytes());
                }
            }
            Err(err) => reply.error(Errno::from_i32(err.raw_os_error().unwrap_or(libc::EIO))),
        }
    }

    fn getxattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _name: &std::ffi::OsStr,
        _size: u32,
        reply: ReplyXattr,
    ) {
        reply.error(Errno::from_i32(libc::ENODATA));
    }

    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let state = self.state.read().unwrap().clone();
        if ino == ROOT_INO {
            reply.error(Errno::EISDIR);
            return;
        }

        let mut runtime = self.runtime.lock().unwrap();
        let Some(path) = runtime.path_for_inode(&state, ino) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let resolved = match state_for_path(&state, &path) {
            Ok(resolved) => resolved,
            Err(_) => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        if entry_is_read_only(&resolved.entry, state.read_only_default) {
            reply.error(Errno::EROFS);
            return;
        }

        let Some(handle) = runtime.handles.get(&fh.0) else {
            reply.error(Errno::EBADF);
            return;
        };
        if !handle.writable {
            reply.error(Errno::EPERM);
            return;
        }

        match handle.file.write_at(data, offset) {
            Ok(written) => reply.written(written as u32),
            Err(err) => reply.error(Errno::from_i32(err.raw_os_error().unwrap_or(libc::EIO))),
        }
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        let runtime = self.runtime.lock().unwrap();
        match runtime.handles.get(&fh.0) {
            Some(handle) if handle.writable => match handle.file.sync_all() {
                Ok(()) => reply.ok(),
                Err(err) => reply.error(Errno::from_i32(err.raw_os_error().unwrap_or(libc::EIO))),
            },
            Some(_) => reply.ok(),
            None => reply.error(Errno::EBADF),
        }
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let mut runtime = self.runtime.lock().unwrap();
        runtime.handles.remove(&fh.0);
        reply.ok();
    }

    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        datasync: bool,
        reply: ReplyEmpty,
    ) {
        let runtime = self.runtime.lock().unwrap();
        match runtime.handles.get(&fh.0) {
            Some(handle) if handle.writable => {
                let result = if datasync {
                    handle.file.sync_data()
                } else {
                    handle.file.sync_all()
                };
                match result {
                    Ok(()) => reply.ok(),
                    Err(err) => {
                        reply.error(Errno::from_i32(err.raw_os_error().unwrap_or(libc::EIO)))
                    }
                }
            }
            Some(_) => reply.ok(),
            None => reply.error(Errno::EBADF),
        }
    }

    fn copy_file_range(
        &self,
        _req: &Request,
        ino_in: INodeNo,
        fh_in: FileHandle,
        offset_in: u64,
        ino_out: INodeNo,
        fh_out: FileHandle,
        offset_out: u64,
        len: u64,
        _flags: CopyFileRangeFlags,
        reply: ReplyWrite,
    ) {
        if ino_in == ROOT_INO || ino_out == ROOT_INO {
            reply.error(Errno::EISDIR);
            return;
        }

        let state = self.state.read().unwrap().clone();
        let mut runtime = self.runtime.lock().unwrap();

        let Some(_) = runtime.path_for_inode(&state, ino_in) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let Some(output_path) = runtime.path_for_inode(&state, ino_out) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let output_resolved = match state_for_path(&state, &output_path) {
            Ok(resolved) => resolved,
            Err(err) => {
                reply.error(errno_from_error(&err));
                return;
            }
        };
        if entry_is_read_only(&output_resolved.entry, state.read_only_default) {
            reply.error(Errno::EROFS);
            return;
        }

        let Some(source_handle) = runtime.handles.get(&fh_in.0) else {
            reply.error(Errno::EBADF);
            return;
        };
        let source = match source_handle.file.try_clone() {
            Ok(file) => file,
            Err(err) => {
                reply.error(Errno::from_i32(err.raw_os_error().unwrap_or(libc::EIO)));
                return;
            }
        };

        let Some(destination_handle) = runtime.handles.get(&fh_out.0) else {
            reply.error(Errno::EBADF);
            return;
        };
        if !destination_handle.writable {
            reply.error(Errno::EPERM);
            return;
        }
        let destination = match destination_handle.file.try_clone() {
            Ok(file) => file,
            Err(err) => {
                reply.error(Errno::from_i32(err.raw_os_error().unwrap_or(libc::EIO)));
                return;
            }
        };

        match copy_file_range_fallback(&source, offset_in, &destination, offset_out, len) {
            Ok(written) => reply.written(written as u32),
            Err(err) => reply.error(errno_from_error(&err)),
        }
    }

    fn releasedir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn fsyncdir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn ioctl(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _flags: IoctlFlags,
        _cmd: u32,
        _in_data: &[u8],
        _out_size: u32,
        reply: ReplyIoctl,
    ) {
        reply.error(Errno::ENOTTY);
    }

    fn poll(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _ph: PollNotifier,
        events: PollEvents,
        _flags: PollFlags,
        reply: ReplyPoll,
    ) {
        let runtime = self.runtime.lock().unwrap();
        let Some(handle) = runtime.handles.get(&fh.0) else {
            reply.error(Errno::EBADF);
            return;
        };

        let mut ready = PollEvents::empty();
        if matches!(handle.kind, OpenHandleKind::File) {
            ready |= events & (PollEvents::POLLIN | PollEvents::POLLRDNORM);
            if handle.writable {
                ready |= events & (PollEvents::POLLOUT | PollEvents::POLLWRNORM);
            }
        }
        reply.poll(ready);
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        let state = self.state.read().unwrap();
        reply.statfs(state.entries.len() as u64 + 1, 0, 0, 0, 0, 4096, 255, 0);
    }
}

use std::{
    fs::{self, OpenOptions},
    os::unix::fs::{FileExt, OpenOptionsExt, PermissionsExt},
    time::SystemTime,
};

use fuser::{
    BsdFileFlags, Errno, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo,
    IoctlFlags, LockOwner, OpenFlags, PollEvents, PollFlags, PollNotifier, RenameFlags, ReplyAttr,
    ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyIoctl, ReplyOpen,
    ReplyPoll, ReplyStatfs, ReplyWrite, Request, TimeOrNow, WriteFlags,
};

use crate::{
    error::{Error, Result},
    state::PortalState,
};

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
    Ok(runtime.handle_file(file, OpenHandleKind::File, writable))
}

impl Filesystem for PortalFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &std::ffi::OsStr, reply: ReplyEntry) {
        let state = self.state.read().unwrap().clone();
        let mut runtime = self.runtime.lock().unwrap();

        if parent == ROOT_INO {
            let Some(name) = name.to_str() else {
                reply.error(Errno::ENOENT);
                return;
            };
            let Some(entry) = state.entries.get(name).cloned() else {
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
            reply.entry(&TTL, &attr, Generation(entry.generation));
            return;
        }

        let name = name.to_os_string();
        let Some(parent_path) = runtime.path_for_inode(&state, parent) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let Ok(child_path) = child_portal_path(&parent_path, &name) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let resolved = match state_for_path(&state, &child_path) {
            Ok(resolved) => resolved,
            Err(err) => {
                reply.error(errno_from_error(&err));
                return;
            }
        };
        let metadata = match fs::symlink_metadata(&resolved.target) {
            Ok(metadata) => metadata,
            Err(_) => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let ino = runtime.cache_portal_path(child_path);
        let attr = attr_from_metadata(
            ino,
            &metadata,
            entry_is_read_only(&resolved.entry, state.read_only_default),
            0,
        );
        reply.entry(&TTL, &attr, Generation(resolved.entry.generation));
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let state = self.state.read().unwrap().clone();
        if ino == ROOT_INO {
            match root_attr(&state) {
                Ok(attr) => reply.attr(&TTL, &attr),
                Err(_) => reply.error(Errno::EIO),
            }
            return;
        }

        let mut runtime = self.runtime.lock().unwrap();
        let Some(path) = runtime.path_for_inode(&state, ino) else {
            reply.error(Errno::ENOENT);
            return;
        };
        match file_attr(&mut runtime, &state, &path)
            .or_else(|_| directory_attr(&mut runtime, &state, &path))
        {
            Ok(attr) => reply.attr(&TTL, &attr),
            Err(err) => reply.error(errno_from_error(&err)),
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
            reply.error(Errno::ENOENT);
            return;
        };

        let Ok(entries) = dir_entries(&mut runtime, &state, &path) else {
            reply.error(Errno::ENOTDIR);
            return;
        };

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
            reply.error(Errno::ENOENT);
            return;
        };
        match open_path(&mut runtime, &state, &path, flags.0, 0) {
            Ok(fh) => reply.opened(fh, FopenFlags::empty()),
            Err(Error::TargetNotDirectory(_)) => reply.error(Errno::EISDIR),
            Err(Error::PermissionDenied(_)) => reply.error(Errno::EROFS),
            Err(err) => reply.error(errno_from_error(&err)),
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
                let fh = runtime.handle_file(file, OpenHandleKind::File, true);
                let ino = runtime.cache_portal_path(path);
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
            reply.error(Errno::EACCES);
            return;
        };
        let Ok((target_path, target_resolved)) =
            runtime.resolve_parent_child_writable(&state, newparent, &target_name)
        else {
            reply.error(Errno::EACCES);
            return;
        };

        match validate_rename(
            &state,
            portal_path_to_pathbuf(&source_path),
            portal_path_to_pathbuf(&target_path),
        ) {
            Ok(_) => match fs::rename(&source_resolved.target, &target_resolved.target) {
                Ok(()) => reply.ok(),
                Err(err) => reply.error(Errno::from_i32(err.raw_os_error().unwrap_or(libc::EIO))),
            },
            Err(Error::PermissionDenied(_)) => reply.error(Errno::EPERM),
            Err(Error::InvalidPortalPath(_)) => reply.error(Errno::EINVAL),
            Err(Error::EntryNotFound(_) | Error::TargetNotFound(_)) => reply.error(Errno::ENOENT),
            Err(_) => reply.error(Errno::EIO),
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
        let state = self.state.read().unwrap().clone();
        if ino == ROOT_INO {
            reply.error(Errno::EISDIR);
            return;
        }

        let mut runtime = self.runtime.lock().unwrap();
        let Some(_) = runtime.path_for_inode(&state, ino) else {
            reply.error(Errno::ENOENT);
            return;
        };

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

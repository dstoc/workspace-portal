use std::{
    cmp,
    ffi::CString,
    fs,
    os::unix::{ffi::OsStrExt, fs::FileExt, fs::MetadataExt, io::AsRawFd},
    path::Path,
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
    resolve::{
        ensure_mutable_relative_path, entry_is_read_only, errno_from_error,
        is_immutable_path_error, state_for_path, validate_rename,
    },
    runtime::OpenHandleKind,
    safe_open,
};

fn mutation_permission_errno(error: &Error, fallback: Errno) -> Errno {
    if is_immutable_path_error(error) {
        Errno::EPERM
    } else {
        fallback
    }
}

fn raw_os_errno(error: &std::io::Error) -> i32 {
    error.raw_os_error().unwrap_or(libc::EIO)
}

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
            // Confined directory read beneath the entry target; fails (ENOTDIR)
            // if the resolved path is not a directory.
            let children = safe_open::list_dir(&resolved.entry.target, &resolved.relative)?;

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

            for (name, file_type) in children {
                let child_path = child_portal_path(path, &name)?;
                let child_ino = runtime.cache_portal_path(child_path.clone());
                entries.push((
                    child_ino,
                    file_type,
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
    let metadata = safe_open::lstat(&resolved.entry.target, &resolved.relative)?;
    if metadata.file_type().is_dir() {
        return Err(Error::TargetNotDirectory(resolved.target));
    }

    let writable = (flags & libc::O_ACCMODE) != libc::O_RDONLY;
    if writable {
        super::resolve::ensure_writable_entry(&resolved.entry)?;
        ensure_mutable_relative_path(state, &resolved.relative)?;
        if state.read_only_default {
            return Err(Error::PermissionDenied(
                "workspace mount is read-only".to_owned(),
            ));
        }
    }

    let mut oflags = if writable {
        libc::O_RDWR
    } else {
        libc::O_RDONLY
    };
    if flags & libc::O_APPEND != 0 {
        oflags |= libc::O_APPEND;
    }
    if flags & libc::O_TRUNC != 0 {
        oflags |= libc::O_TRUNC;
    }

    let file = safe_open::open_file(
        &resolved.entry.target,
        &resolved.relative,
        oflags,
        mode & 0o7777,
    )?;
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

fn timeornow_to_timespec(value: Option<TimeOrNow>) -> libc::timespec {
    match value {
        None => libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_OMIT,
        },
        Some(TimeOrNow::Now) => libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_NOW,
        },
        Some(TimeOrNow::SpecificTime(t)) => {
            match t.duration_since(SystemTime::UNIX_EPOCH) {
                Ok(d) => libc::timespec {
                    tv_sec: d.as_secs() as libc::time_t,
                    tv_nsec: d.subsec_nanos() as _,
                },
                Err(e) => {
                    // Time is before the Unix epoch.
                    let d = e.duration();
                    let nanos = d.subsec_nanos();
                    let mut tv_sec = -(d.as_secs() as libc::time_t);
                    let tv_nsec: libc::c_long = if nanos == 0 {
                        0
                    } else {
                        // Borrow 1 second so tv_nsec stays positive.
                        tv_sec -= 1;
                        (1_000_000_000 - nanos) as libc::c_long
                    };
                    libc::timespec { tv_sec, tv_nsec }
                }
            }
        }
    }
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
            let ino = runtime.remember_lookup(path);
            let metadata = match safe_open::lstat(&entry.target, Path::new("")) {
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
        let metadata = match safe_open::lstat(&resolved.entry.target, &resolved.relative) {
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
        let ino = runtime.remember_lookup(child_path.clone());
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
        runtime.forget_inode(ino, nlookup);
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
                if matches!(&err, Error::EntryNotFound(_) | Error::TargetNotFound(_))
                    && let Some(metadata) = runtime.open_handle_metadata(ino)
                {
                    let attr = attr_from_metadata(ino, &metadata, false, 0);
                    debug!(
                        "getattr ino={} path={:?} -> ok (revoked, fstat fallback)",
                        ino.0, path
                    );
                    reply.attr(&TTL, &attr);
                    return;
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
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
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
            debug!(
                operation = "setattr",
                ino = ino.0,
                file_handle = ?fh.map(|fh| fh.0),
                errno = ?Errno::ENOENT,
                "setattr failed for unknown inode"
            );
            reply.error(Errno::ENOENT);
            return;
        };
        let resolved = match state_for_path(&state, &path) {
            Ok(resolved) => resolved,
            Err(err) => {
                debug!(
                    operation = "setattr",
                    ino = ino.0,
                    file_handle = ?fh.map(|fh| fh.0),
                    portal_path = %portal_path_to_pathbuf(&path).display(),
                    errno = ?Errno::ENOENT,
                    error = %err,
                    "setattr failed to resolve portal path"
                );
                reply.error(Errno::ENOENT);
                return;
            }
        };

        if entry_is_read_only(&resolved.entry, state.read_only_default) {
            debug!(
                operation = "setattr",
                ino = ino.0,
                file_handle = ?fh.map(|fh| fh.0),
                portal_path = %portal_path_to_pathbuf(&path).display(),
                entry = %resolved.entry.name,
                entry_target = %resolved.entry.target.display(),
                relative_path = %resolved.relative.display(),
                target = %resolved.target.display(),
                errno = ?Errno::EROFS,
                "setattr denied on read-only entry"
            );
            reply.error(Errno::EROFS);
            return;
        }
        if (mode.is_some() || uid.is_some() || gid.is_some())
            && let Err(err) = ensure_mutable_relative_path(&state, &resolved.relative)
        {
            let errno = mutation_permission_errno(&err, Errno::EROFS);
            debug!(
                operation = "setattr",
                ino = ino.0,
                file_handle = ?fh.map(|fh| fh.0),
                portal_path = %portal_path_to_pathbuf(&path).display(),
                entry = %resolved.entry.name,
                entry_target = %resolved.entry.target.display(),
                relative_path = %resolved.relative.display(),
                target = %resolved.target.display(),
                errno = ?errno,
                error = %err,
                mode_requested = mode.is_some(),
                requested_uid = ?uid,
                requested_gid = ?gid,
                "setattr metadata change denied by immutable path"
            );
            reply.error(errno);
            return;
        }
        if uid.is_some() || gid.is_some() {
            let current_metadata = safe_open::lstat(&resolved.entry.target, &resolved.relative);
            let (current_uid, current_gid) = match current_metadata.as_ref() {
                Ok(metadata) => (Some(metadata.uid()), Some(metadata.gid())),
                Err(_) => (None, None),
            };
            let uid_matches = match (uid, current_uid) {
                (Some(requested), Some(current)) => requested == current,
                (None, _) => true,
                _ => false,
            };
            let gid_matches = match (gid, current_gid) {
                (Some(requested), Some(current)) => requested == current,
                (None, _) => true,
                _ => false,
            };
            if uid_matches && gid_matches {
                debug!(
                    operation = "setattr",
                    ino = ino.0,
                    file_handle = ?fh.map(|fh| fh.0),
                    portal_path = %portal_path_to_pathbuf(&path).display(),
                    entry = %resolved.entry.name,
                    entry_target = %resolved.entry.target.display(),
                    relative_path = %resolved.relative.display(),
                    target = %resolved.target.display(),
                    requested_uid = ?uid,
                    requested_gid = ?gid,
                    current_uid = ?current_uid,
                    current_gid = ?current_gid,
                    "setattr allowed no-op uid/gid change"
                );
            } else {
                debug!(
                    operation = "setattr",
                    ino = ino.0,
                    file_handle = ?fh.map(|fh| fh.0),
                    portal_path = %portal_path_to_pathbuf(&path).display(),
                    entry = %resolved.entry.name,
                    entry_target = %resolved.entry.target.display(),
                    relative_path = %resolved.relative.display(),
                    target = %resolved.target.display(),
                    requested_uid = ?uid,
                    requested_gid = ?gid,
                    current_uid = ?current_uid,
                    current_gid = ?current_gid,
                    current_metadata_error = ?current_metadata.as_ref().err(),
                    errno = ?Errno::EPERM,
                    "setattr denied uid/gid change"
                );
                reply.error(Errno::EPERM);
                return;
            }
        }

        if let Some(mode) = mode
            && let Err(err) = safe_open::chmod(
                &resolved.entry.target,
                &resolved.relative,
                (mode & 0o7777) as libc::mode_t,
            )
        {
            let raw_errno = raw_os_errno(&err);
            debug!(
                operation = "setattr",
                ino = ino.0,
                file_handle = ?fh.map(|fh| fh.0),
                portal_path = %portal_path_to_pathbuf(&path).display(),
                entry = %resolved.entry.name,
                entry_target = %resolved.entry.target.display(),
                relative_path = %resolved.relative.display(),
                target = %resolved.target.display(),
                mode = mode,
                raw_os_errno = raw_errno,
                error = %err,
                "setattr chmod failed"
            );
            reply.error(Errno::from_i32(raw_errno));
            return;
        }

        if let Some(size) = size {
            let result = if let Some(fh) = fh {
                match runtime.handles.get(&fh.0) {
                    Some(handle) if handle.writable => handle.file.set_len(size),
                    Some(_) => {
                        debug!(
                            operation = "setattr",
                            ino = ino.0,
                            file_handle = fh.0,
                            portal_path = %portal_path_to_pathbuf(&path).display(),
                            entry = %resolved.entry.name,
                            entry_target = %resolved.entry.target.display(),
                            relative_path = %resolved.relative.display(),
                            target = %resolved.target.display(),
                            size = size,
                            errno = ?Errno::EPERM,
                            "setattr truncate denied on read-only handle"
                        );
                        reply.error(Errno::EPERM);
                        return;
                    }
                    None => {
                        debug!(
                            operation = "setattr",
                            ino = ino.0,
                            file_handle = fh.0,
                            portal_path = %portal_path_to_pathbuf(&path).display(),
                            entry = %resolved.entry.name,
                            entry_target = %resolved.entry.target.display(),
                            relative_path = %resolved.relative.display(),
                            target = %resolved.target.display(),
                            size = size,
                            errno = ?Errno::EBADF,
                            "setattr truncate failed for unknown handle"
                        );
                        reply.error(Errno::EBADF);
                        return;
                    }
                }
            } else {
                if let Err(err) = ensure_mutable_relative_path(&state, &resolved.relative) {
                    let errno = mutation_permission_errno(&err, Errno::EROFS);
                    debug!(
                        operation = "setattr",
                        ino = ino.0,
                        portal_path = %portal_path_to_pathbuf(&path).display(),
                        entry = %resolved.entry.name,
                        entry_target = %resolved.entry.target.display(),
                        relative_path = %resolved.relative.display(),
                        target = %resolved.target.display(),
                        size = size,
                        errno = ?errno,
                        error = %err,
                        "setattr path truncate denied by immutable path"
                    );
                    reply.error(errno);
                    return;
                }
                safe_open::truncate(&resolved.entry.target, &resolved.relative, size)
            };
            if let Err(err) = result {
                let raw_errno = raw_os_errno(&err);
                debug!(
                    operation = "setattr",
                    ino = ino.0,
                    file_handle = ?fh.map(|fh| fh.0),
                    portal_path = %portal_path_to_pathbuf(&path).display(),
                    entry = %resolved.entry.name,
                    entry_target = %resolved.entry.target.display(),
                    relative_path = %resolved.relative.display(),
                    target = %resolved.target.display(),
                    size = size,
                    raw_os_errno = raw_errno,
                    error = %err,
                    "setattr truncate failed"
                );
                reply.error(Errno::from_i32(raw_errno));
                return;
            }
        }

        if atime.is_some() || mtime.is_some() {
            let times: [libc::timespec; 2] =
                [timeornow_to_timespec(atime), timeornow_to_timespec(mtime)];

            // Prefer futimens on an open writable handle; fall back to utimensat on path.
            let use_fd: Option<i32> = fh.and_then(|fh| {
                runtime.handles.get(&fh.0).and_then(|handle| {
                    if handle.writable {
                        Some(handle.file.as_raw_fd())
                    } else {
                        None
                    }
                })
            });

            // Prefer futimens on an open writable handle; otherwise apply the
            // timestamps through a confined resolution beneath the entry root.
            if use_fd.is_none()
                && let Err(err) = ensure_mutable_relative_path(&state, &resolved.relative)
            {
                let errno = mutation_permission_errno(&err, Errno::EROFS);
                debug!(
                    operation = "setattr",
                    ino = ino.0,
                    file_handle = ?fh.map(|fh| fh.0),
                    portal_path = %portal_path_to_pathbuf(&path).display(),
                    entry = %resolved.entry.name,
                    entry_target = %resolved.entry.target.display(),
                    relative_path = %resolved.relative.display(),
                    target = %resolved.target.display(),
                    atime_requested = atime.is_some(),
                    mtime_requested = mtime.is_some(),
                    errno = ?errno,
                    error = %err,
                    "setattr timestamp change denied by immutable path"
                );
                reply.error(errno);
                return;
            }

            let result: std::io::Result<()> = if let Some(fd) = use_fd {
                let rc = unsafe { libc::futimens(fd, times.as_ptr()) };
                if rc == -1 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            } else {
                safe_open::set_times(&resolved.entry.target, &resolved.relative, &times)
            };

            if let Err(err) = result {
                let raw_errno = raw_os_errno(&err);
                debug!(
                    operation = "setattr",
                    ino = ino.0,
                    file_handle = ?fh.map(|fh| fh.0),
                    portal_path = %portal_path_to_pathbuf(&path).display(),
                    entry = %resolved.entry.name,
                    entry_target = %resolved.entry.target.display(),
                    relative_path = %resolved.relative.display(),
                    target = %resolved.target.display(),
                    atime_requested = atime.is_some(),
                    mtime_requested = mtime.is_some(),
                    timestamp_method = if use_fd.is_some() { "futimens" } else { "set_times" },
                    raw_os_errno = raw_errno,
                    error = %err,
                    "setattr timestamp update failed"
                );
                reply.error(Errno::from_i32(raw_errno));
                return;
            }
        }

        match current_attr(&mut runtime, &state, &path) {
            Ok(attr) => reply.attr(&TTL, &attr),
            Err(err) => {
                debug!(
                    operation = "setattr",
                    ino = ino.0,
                    file_handle = ?fh.map(|fh| fh.0),
                    portal_path = %portal_path_to_pathbuf(&path).display(),
                    entry = %resolved.entry.name,
                    entry_target = %resolved.entry.target.display(),
                    relative_path = %resolved.relative.display(),
                    target = %resolved.target.display(),
                    errno = ?Errno::EIO,
                    error = %err,
                    "setattr current_attr failed"
                );
                reply.error(Errno::EIO)
            }
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
        match safe_open::lstat(&resolved.entry.target, &resolved.relative) {
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
                let errno =
                    mutation_permission_errno(&Error::PermissionDenied(err.clone()), Errno::EROFS);
                debug!(
                    "open ino={} path={:?} flags={:#x} -> {:?} err={}",
                    ino.0, path, flags.0, errno, err
                );
                reply.error(errno)
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
            debug!(
                operation = "create",
                parent = parent.0,
                name = ?name,
                errno = ?Errno::EPERM,
                "create denied at root parent"
            );
            reply.error(Errno::EPERM);
            return;
        }

        let mut runtime = self.runtime.lock().unwrap();
        let name = name.to_os_string();
        let (path, resolved) = match runtime.resolve_parent_child_writable(&state, parent, &name) {
            Ok(value) => value,
            Err(err) => {
                let errno = mutation_permission_errno(&err, Errno::EACCES);
                debug!(
                    operation = "create",
                    parent = parent.0,
                    name = ?name,
                    errno = ?errno,
                    error = %err,
                    "create resolve_parent_child_writable failed"
                );
                reply.error(errno);
                return;
            }
        };

        let mut oflags = libc::O_CREAT | libc::O_EXCL | libc::O_RDWR;
        if flags & libc::O_TRUNC != 0 {
            oflags |= libc::O_TRUNC;
        }
        let create_mode = ((mode & !umask) & 0o7777) as libc::mode_t;

        debug!(
            operation = "create",
            parent = parent.0,
            name = ?name,
            portal_path = %portal_path_to_pathbuf(&path).display(),
            entry = %resolved.entry.name,
            entry_target = %resolved.entry.target.display(),
            relative_path = %resolved.relative.display(),
            target = %resolved.target.display(),
            mode = mode,
            umask = umask,
            create_mode = create_mode,
            flags = flags,
            open_flags = oflags,
            "create resolved before create_file"
        );

        match safe_open::create_file(
            &resolved.entry.target,
            &resolved.relative,
            oflags,
            create_mode,
        ) {
            Ok(file) => {
                let ino = runtime.remember_lookup(path.clone());
                let fh = runtime.handle_file(ino, file, OpenHandleKind::File, true);
                let metadata = match safe_open::lstat(&resolved.entry.target, &resolved.relative) {
                    Ok(metadata) => metadata,
                    Err(err) => {
                        let raw_errno = raw_os_errno(&err);
                        debug!(
                            operation = "create",
                            parent = parent.0,
                            name = ?name,
                            portal_path = %portal_path_to_pathbuf(&path).display(),
                            entry = %resolved.entry.name,
                            entry_target = %resolved.entry.target.display(),
                            relative_path = %resolved.relative.display(),
                            target = %resolved.target.display(),
                            raw_os_errno = raw_errno,
                            error = %err,
                            "create lstat failed after create_file"
                        );
                        reply.error(Errno::from_i32(raw_errno));
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
                debug!(
                    operation = "create",
                    parent = parent.0,
                    name = ?name,
                    portal_path = %portal_path_to_pathbuf(&path).display(),
                    entry = %resolved.entry.name,
                    entry_target = %resolved.entry.target.display(),
                    relative_path = %resolved.relative.display(),
                    target = %resolved.target.display(),
                    ino = ino.0,
                    file_handle = fh.0,
                    "create succeeded"
                );
            }
            Err(err) => {
                let raw_errno = raw_os_errno(&err);
                debug!(
                    operation = "create",
                    parent = parent.0,
                    name = ?name,
                    portal_path = %portal_path_to_pathbuf(&path).display(),
                    entry = %resolved.entry.name,
                    entry_target = %resolved.entry.target.display(),
                    relative_path = %resolved.relative.display(),
                    target = %resolved.target.display(),
                    raw_os_errno = raw_errno,
                    error = %err,
                    "create_file failed"
                );
                reply.error(Errno::from_i32(raw_errno))
            }
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
            debug!(
                operation = "mkdir",
                parent = parent.0,
                name = ?name,
                errno = ?Errno::EPERM,
                "mkdir denied at root parent"
            );
            reply.error(Errno::EPERM);
            return;
        }

        let mut runtime = self.runtime.lock().unwrap();
        let name = name.to_os_string();
        let (path, resolved) = match runtime.resolve_parent_child_writable(&state, parent, &name) {
            Ok(value) => value,
            Err(err) => {
                let errno = mutation_permission_errno(&err, Errno::EACCES);
                debug!(
                    operation = "mkdir",
                    parent = parent.0,
                    name = ?name,
                    errno = ?errno,
                    error = %err,
                    "mkdir resolve_parent_child_writable failed"
                );
                reply.error(errno);
                return;
            }
        };

        let dir_mode = ((mode & !umask) & 0o7777) as libc::mode_t;
        debug!(
            operation = "mkdir",
            parent = parent.0,
            name = ?name,
            portal_path = %portal_path_to_pathbuf(&path).display(),
            entry = %resolved.entry.name,
            entry_target = %resolved.entry.target.display(),
            relative_path = %resolved.relative.display(),
            target = %resolved.target.display(),
            mode = mode,
            umask = umask,
            dir_mode = dir_mode,
            "mkdir resolved before mkdir"
        );

        match safe_open::mkdir(&resolved.entry.target, &resolved.relative, dir_mode) {
            Ok(()) => {
                // Force the requested permissions (mkdirat applies the umask).
                if let Err(err) =
                    safe_open::chmod(&resolved.entry.target, &resolved.relative, dir_mode)
                {
                    let raw_errno = raw_os_errno(&err);
                    debug!(
                        operation = "mkdir",
                        parent = parent.0,
                        name = ?name,
                        portal_path = %portal_path_to_pathbuf(&path).display(),
                        entry = %resolved.entry.name,
                        entry_target = %resolved.entry.target.display(),
                        relative_path = %resolved.relative.display(),
                        target = %resolved.target.display(),
                        raw_os_errno = raw_errno,
                        error = %err,
                        "mkdir chmod failed"
                    );
                    reply.error(Errno::from_i32(raw_errno));
                    return;
                }
                let metadata = match safe_open::lstat(&resolved.entry.target, &resolved.relative) {
                    Ok(metadata) => metadata,
                    Err(err) => {
                        let raw_errno = raw_os_errno(&err);
                        debug!(
                            operation = "mkdir",
                            parent = parent.0,
                            name = ?name,
                            portal_path = %portal_path_to_pathbuf(&path).display(),
                            entry = %resolved.entry.name,
                            entry_target = %resolved.entry.target.display(),
                            relative_path = %resolved.relative.display(),
                            target = %resolved.target.display(),
                            raw_os_errno = raw_errno,
                            error = %err,
                            "mkdir lstat failed after mkdir"
                        );
                        reply.error(Errno::from_i32(raw_errno));
                        return;
                    }
                };
                let ino = runtime.remember_lookup(path.clone());
                let attr = attr_from_metadata(
                    ino,
                    &metadata,
                    entry_is_read_only(&resolved.entry, state.read_only_default),
                    0,
                );
                reply.entry(&TTL, &attr, Generation(resolved.entry.generation));
                debug!(
                    operation = "mkdir",
                    parent = parent.0,
                    name = ?name,
                    portal_path = %portal_path_to_pathbuf(&path).display(),
                    entry = %resolved.entry.name,
                    entry_target = %resolved.entry.target.display(),
                    relative_path = %resolved.relative.display(),
                    target = %resolved.target.display(),
                    ino = ino.0,
                    "mkdir succeeded"
                );
            }
            Err(err) => {
                let raw_errno = raw_os_errno(&err);
                debug!(
                    operation = "mkdir",
                    parent = parent.0,
                    name = ?name,
                    portal_path = %portal_path_to_pathbuf(&path).display(),
                    entry = %resolved.entry.name,
                    entry_target = %resolved.entry.target.display(),
                    relative_path = %resolved.relative.display(),
                    target = %resolved.target.display(),
                    raw_os_errno = raw_errno,
                    error = %err,
                    "safe_open::mkdir failed"
                );
                reply.error(Errno::from_i32(raw_errno))
            }
        }
    }

    fn symlink(
        &self,
        _req: &Request,
        parent: INodeNo,
        link_name: &std::ffi::OsStr,
        target: &std::path::Path,
        reply: ReplyEntry,
    ) {
        let state = self.state.read().unwrap().clone();
        if parent == ROOT_INO {
            reply.error(Errno::EPERM);
            return;
        }

        let mut runtime = self.runtime.lock().unwrap();
        let name = link_name.to_os_string();
        let (path, resolved) = match runtime.resolve_parent_child_writable(&state, parent, &name) {
            Ok(value) => value,
            Err(err) => {
                reply.error(mutation_permission_errno(&err, Errno::EACCES));
                return;
            }
        };

        match safe_open::symlink(&resolved.entry.target, &resolved.relative, target) {
            Ok(()) => {
                let metadata = match safe_open::lstat(&resolved.entry.target, &resolved.relative) {
                    Ok(metadata) => metadata,
                    Err(err) => {
                        reply.error(Errno::from_i32(err.raw_os_error().unwrap_or(libc::EIO)));
                        return;
                    }
                };
                let ino = runtime.remember_lookup(path);
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
        let (_, resolved) = match runtime.resolve_parent_child_writable(&state, parent, &name) {
            Ok(value) => value,
            Err(err) => {
                reply.error(mutation_permission_errno(&err, Errno::EACCES));
                return;
            }
        };

        match safe_open::lstat(&resolved.entry.target, &resolved.relative) {
            Ok(metadata) if metadata.file_type().is_dir() => reply.error(Errno::EISDIR),
            Ok(_) => match safe_open::unlink(&resolved.entry.target, &resolved.relative) {
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
        let (_, resolved) = match runtime.resolve_parent_child_writable(&state, parent, &name) {
            Ok(value) => value,
            Err(err) => {
                reply.error(mutation_permission_errno(&err, Errno::EACCES));
                return;
            }
        };

        match safe_open::rmdir(&resolved.entry.target, &resolved.relative) {
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

        let (source_path, source_resolved) = match runtime.resolve_parent_child_writable(
            &state,
            parent,
            &source_name,
        ) {
            Ok(value) => value,
            Err(err) => {
                let errno = mutation_permission_errno(&err, Errno::EACCES);
                debug!(
                    "rename parent={} name={:?} newparent={} newname={:?} -> {:?} (source) err={}",
                    parent.0, source_name, newparent.0, target_name, errno, err
                );
                reply.error(errno);
                return;
            }
        };
        let (target_path, target_resolved) = match runtime.resolve_parent_child_writable(
            &state,
            newparent,
            &target_name,
        ) {
            Ok(value) => value,
            Err(err) => {
                let errno = mutation_permission_errno(&err, Errno::EACCES);
                debug!(
                    "rename parent={} name={:?} newparent={} newname={:?} -> {:?} (target) err={}",
                    parent.0, source_name, newparent.0, target_name, errno, err
                );
                reply.error(errno);
                return;
            }
        };

        match validate_rename(
            &state,
            portal_path_to_pathbuf(&source_path),
            portal_path_to_pathbuf(&target_path),
        ) {
            Ok(_) => match safe_open::rename(
                &source_resolved.entry.target,
                &source_resolved.relative,
                &target_resolved.relative,
            ) {
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
        if newparent == ROOT_INO {
            debug!(
                "link ino={} newparent={} newname={:?} -> EPERM (root destination)",
                ino.0, newparent.0, newname
            );
            reply.error(Errno::EPERM);
            return;
        }

        let state = self.state.read().unwrap().clone();
        let mut runtime = self.runtime.lock().unwrap();

        let Some(source_path) = runtime.path_for_inode(&state, ino) else {
            debug!(
                "link ino={} newparent={} newname={:?} -> ENOENT (unknown source inode)",
                ino.0, newparent.0, newname
            );
            reply.error(Errno::ENOENT);
            return;
        };
        let source_resolved = match state_for_path(&state, &source_path) {
            Ok(resolved) => resolved,
            Err(err) => {
                let errno = errno_from_error(&err);
                debug!(
                    "link ino={} newparent={} newname={:?} source_path={:?} -> {:?} err={}",
                    ino.0, newparent.0, newname, source_path, errno, err
                );
                reply.error(errno);
                return;
            }
        };
        if let Err(err) = ensure_mutable_relative_path(&state, &source_resolved.relative) {
            debug!(
                "link ino={} newparent={} newname={:?} source={} -> EPERM err={}",
                ino.0,
                newparent.0,
                newname,
                source_resolved.target.display(),
                err
            );
            reply.error(mutation_permission_errno(&err, Errno::EPERM));
            return;
        }

        let name = newname.to_os_string();
        let (destination_path, destination_resolved) =
            match runtime.resolve_parent_child_writable(&state, newparent, &name) {
                Ok(value) => value,
                Err(err) => {
                    let errno = mutation_permission_errno(&err, Errno::EACCES);
                    debug!(
                        "link ino={} newparent={} newname={:?} -> {:?} (destination) err={}",
                        ino.0, newparent.0, newname, errno, err
                    );
                    reply.error(errno);
                    return;
                }
            };

        if source_resolved.entry.name != destination_resolved.entry.name {
            debug!(
                "link ino={} newparent={} newname={:?} source_entry={} dest_entry={} -> EXDEV",
                ino.0,
                newparent.0,
                newname,
                source_resolved.entry.name,
                destination_resolved.entry.name
            );
            reply.error(Errno::EXDEV);
            return;
        }

        let metadata =
            match safe_open::lstat(&source_resolved.entry.target, &source_resolved.relative) {
                Ok(metadata) => metadata,
                Err(err) => {
                    let errno = Errno::from_i32(raw_os_errno(&err));
                    debug!(
                        "link ino={} newparent={} newname={:?} source={} -> {:?} lstat error={}",
                        ino.0,
                        newparent.0,
                        newname,
                        source_resolved.target.display(),
                        errno,
                        err
                    );
                    reply.error(errno);
                    return;
                }
            };
        if metadata.file_type().is_dir() {
            debug!(
                "link ino={} newparent={} newname={:?} source={} -> EPERM (source is directory)",
                ino.0,
                newparent.0,
                newname,
                source_resolved.target.display()
            );
            reply.error(Errno::EPERM);
            return;
        }

        match safe_open::hard_link(
            &source_resolved.entry.target,
            &source_resolved.relative,
            &destination_resolved.relative,
        ) {
            Ok(()) => match safe_open::lstat(
                &destination_resolved.entry.target,
                &destination_resolved.relative,
            ) {
                Ok(metadata) => {
                    let destination_ino = runtime.remember_lookup(destination_path.clone());
                    let attr = attr_from_metadata(
                        destination_ino,
                        &metadata,
                        entry_is_read_only(&destination_resolved.entry, state.read_only_default),
                        0,
                    );
                    debug!(
                        "link ino={} newparent={} newname={:?} source={} destination={} -> ino={} ok",
                        ino.0,
                        newparent.0,
                        newname,
                        source_resolved.target.display(),
                        destination_resolved.target.display(),
                        destination_ino.0
                    );
                    reply.entry(
                        &TTL,
                        &attr,
                        Generation(destination_resolved.entry.generation),
                    );
                }
                Err(err) => {
                    let errno = Errno::from_i32(raw_os_errno(&err));
                    debug!(
                        "link ino={} newparent={} newname={:?} destination={} -> {:?} lstat error={}",
                        ino.0,
                        newparent.0,
                        newname,
                        destination_resolved.target.display(),
                        errno,
                        err
                    );
                    reply.error(errno);
                }
            },
            Err(err) => {
                let errno = Errno::from_i32(raw_os_errno(&err));
                debug!(
                    "link ino={} newparent={} newname={:?} source={} destination={} -> errno={:?}",
                    ino.0,
                    newparent.0,
                    newname,
                    source_resolved.target.display(),
                    destination_resolved.target.display(),
                    errno
                );
                reply.error(errno);
            }
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
        if !state.readlink {
            reply.error(Errno::ELOOP);
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

        match safe_open::readlink(&resolved.entry.target, &resolved.relative) {
            Ok(target) => {
                reply.data(target.as_os_str().as_bytes());
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
        match state_for_path(&state, &path) {
            Ok(_) => {}
            Err(_) => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

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
        if let Err(err) = ensure_mutable_relative_path(&state, &output_resolved.relative) {
            reply.error(mutation_permission_errno(&err, Errno::EROFS));
            return;
        }
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

    fn statfs(&self, _req: &Request, ino: INodeNo, reply: ReplyStatfs) {
        let state = self.state.read().unwrap().clone();

        // Measure the filesystem backing the queried inode. The root is a virtual
        // union with no single backing store, and `state.workspace` is *this*
        // mountpoint, so statvfs-ing it would re-enter the (single-threaded) FUSE
        // session and deadlock. Measure the directory hosting the mount instead;
        // for other inodes, the entry target resolved confined beneath the entry.
        let measured = if ino == ROOT_INO {
            state.workspace.parent().and_then(statvfs_for)
        } else {
            let mut runtime = self.runtime.lock().unwrap();
            let resolved = runtime
                .path_for_inode(&state, ino)
                .and_then(|p| state_for_path(&state, &p).ok());
            match resolved {
                Some(resolved) => {
                    safe_open::statvfs(&resolved.entry.target, &resolved.relative).ok()
                }
                None => None,
            }
        };

        // Fall back to the directory hosting the mount, then to hardcoded values.
        // Never statvfs the mountpoint itself (see above): it would deadlock.
        let buf = measured.or_else(|| state.workspace.parent().and_then(statvfs_for));

        match buf {
            Some(buf) => {
                let namelen = if buf.f_namemax == 0 {
                    255
                } else {
                    buf.f_namemax as u32
                };
                reply.statfs(
                    buf.f_blocks,
                    buf.f_bfree,
                    buf.f_bavail,
                    buf.f_files,
                    buf.f_ffree,
                    buf.f_bsize as u32,
                    namelen,
                    buf.f_frsize as u32,
                );
            }
            None => {
                reply.statfs(state.entries.len() as u64 + 1, 0, 0, 0, 0, 4096, 255, 0);
            }
        }
    }
}

fn statvfs_for(path: &Path) -> Option<libc::statvfs> {
    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: zeroed libc::statvfs is a valid initial value for the out-param.
    let mut buf: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut buf) };
    if rc == 0 { Some(buf) } else { None }
}

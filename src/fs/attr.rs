use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use fuser::{FileAttr, FileType, INodeNo};

use crate::{error::Result, state::PortalState};

use super::{
    ROOT_INO,
    path::PortalPath,
    resolve::{entry_is_read_only, state_for_path},
    runtime::FuseRuntime,
};

pub(crate) fn file_type_from_metadata(metadata: &fs::Metadata) -> FileType {
    let file_type = metadata.file_type();
    if file_type.is_dir() {
        FileType::Directory
    } else if file_type.is_symlink() {
        FileType::Symlink
    } else {
        FileType::RegularFile
    }
}

pub(crate) fn system_time_from_unix(secs: i64, nanos: i64) -> SystemTime {
    let secs = secs.max(0) as u64;
    let nanos = nanos.max(0) as u32;
    UNIX_EPOCH + Duration::new(secs, nanos)
}

pub(crate) fn attr_from_metadata(
    ino: INodeNo,
    metadata: &fs::Metadata,
    read_only: bool,
    entries: u32,
) -> FileAttr {
    let kind = file_type_from_metadata(metadata);
    let perm: u16 = if kind == FileType::Directory {
        if read_only { 0o555 } else { 0o755 }
    } else {
        (metadata.permissions().mode() & 0o777) as u16
    };

    let atime = system_time_from_unix(metadata.atime(), metadata.atime_nsec());
    let mtime = system_time_from_unix(metadata.mtime(), metadata.mtime_nsec());
    let ctime = system_time_from_unix(metadata.ctime(), metadata.ctime_nsec());

    FileAttr {
        ino,
        size: metadata.len(),
        blocks: metadata.blocks(),
        atime,
        mtime,
        ctime,
        crtime: ctime,
        kind,
        perm,
        nlink: metadata.nlink() as u32
            + if kind == FileType::Directory {
                entries
            } else {
                0
            },
        uid: metadata.uid(),
        gid: metadata.gid(),
        rdev: metadata.rdev() as u32,
        flags: 0,
        blksize: metadata.blksize() as u32,
    }
}

pub(crate) fn root_attr(state: &PortalState) -> Result<FileAttr> {
    Ok(FileAttr {
        ino: ROOT_INO,
        size: 0,
        blocks: 0,
        atime: UNIX_EPOCH,
        mtime: UNIX_EPOCH,
        ctime: UNIX_EPOCH,
        crtime: UNIX_EPOCH,
        kind: FileType::Directory,
        perm: if state.read_only_default {
            0o555
        } else {
            0o755
        },
        nlink: 2 + state.entries.len() as u32,
        uid: 0,
        gid: 0,
        rdev: 0,
        flags: 0,
        blksize: 4096,
    })
}

pub(crate) fn directory_attr(
    runtime: &mut FuseRuntime,
    state: &PortalState,
    path: &PortalPath,
) -> Result<FileAttr> {
    if matches!(path, PortalPath::Root) {
        return root_attr(state);
    }

    let resolved = state_for_path(state, path)?;
    let metadata = fs::symlink_metadata(&resolved.target)?;
    let read_only = entry_is_read_only(&resolved.entry, state.read_only_default);
    let entries = if metadata.file_type().is_dir() {
        fs::read_dir(&resolved.target)?.count() as u32
    } else {
        0
    };
    let ino = runtime.cache_portal_path(path.clone());
    Ok(attr_from_metadata(ino, &metadata, read_only, entries))
}

pub(crate) fn file_attr(
    runtime: &mut FuseRuntime,
    state: &PortalState,
    path: &PortalPath,
) -> Result<FileAttr> {
    let resolved = state_for_path(state, path)?;
    let metadata = fs::symlink_metadata(&resolved.target)?;
    let ino = runtime.cache_portal_path(path.clone());
    Ok(attr_from_metadata(
        ino,
        &metadata,
        entry_is_read_only(&resolved.entry, state.read_only_default),
        0,
    ))
}

pub(crate) fn current_attr(
    runtime: &mut FuseRuntime,
    state: &PortalState,
    path: &PortalPath,
) -> Result<FileAttr> {
    file_attr(runtime, state, path)
}

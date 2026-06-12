use std::path::{Component, Path};

use fuser::Errno;

use crate::{
    error::{Error, Result},
    state::{AccessMode, EntryRecord, PortalState},
};

use super::path::{PortalPath, RenamePlan, ResolvedPortalPath, parse_portal_path};

pub(crate) fn resolve_portal_path(
    state: &PortalState,
    path: impl AsRef<Path>,
) -> Result<ResolvedPortalPath> {
    let portal_path = parse_portal_path(path)?;
    match portal_path {
        PortalPath::Root => Err(Error::InvalidPortalPath(
            "workspace root does not map to a host target".to_owned(),
        )),
        PortalPath::Entry { name, relative } => {
            let entry = state
                .entry(&name)
                .cloned()
                .ok_or_else(|| Error::EntryNotFound(name.clone()))?;

            let target = if relative.as_os_str().is_empty() {
                entry.target.clone()
            } else {
                entry.target.join(&relative)
            };

            Ok(ResolvedPortalPath {
                entry,
                relative,
                target,
            })
        }
    }
}

pub fn resolve_read_path(
    state: &PortalState,
    path: impl AsRef<Path>,
) -> Result<ResolvedPortalPath> {
    resolve_portal_path(state, path)
}

pub fn resolve_write_path(
    state: &PortalState,
    path: impl AsRef<Path>,
) -> Result<ResolvedPortalPath> {
    let resolved = resolve_portal_path(state, path)?;
    ensure_writable_entry(&resolved.entry)?;
    ensure_mutable_relative_path(state, &resolved.relative)?;
    Ok(resolved)
}

pub fn validate_rename(
    state: &PortalState,
    source: impl AsRef<Path>,
    destination: impl AsRef<Path>,
) -> Result<RenamePlan> {
    let source_path = parse_portal_path(source)?;
    let destination_path = parse_portal_path(destination)?;

    let PortalPath::Entry {
        name: source_name,
        relative: source_relative,
    } = source_path
    else {
        return Err(Error::InvalidPortalPath(
            "workspace root cannot be renamed".to_owned(),
        ));
    };

    let PortalPath::Entry {
        name: destination_name,
        relative: target_relative,
    } = destination_path
    else {
        return Err(Error::InvalidPortalPath(
            "workspace root cannot be the destination of a rename".to_owned(),
        ));
    };

    if source_name != destination_name {
        return Err(Error::PermissionDenied(format!(
            "cross-entry rename is not allowed: {source_name} -> {destination_name}"
        )));
    }

    let entry = state
        .entry(&source_name)
        .cloned()
        .ok_or_else(|| Error::EntryNotFound(source_name.clone()))?;
    ensure_writable_entry(&entry)?;
    ensure_mutable_relative_path(state, &source_relative)?;
    ensure_mutable_relative_path(state, &target_relative)?;

    let source_target = if source_relative.as_os_str().is_empty() {
        entry.target.clone()
    } else {
        entry.target.join(&source_relative)
    };
    let target_target = if target_relative.as_os_str().is_empty() {
        entry.target.clone()
    } else {
        entry.target.join(&target_relative)
    };

    Ok(RenamePlan {
        entry,
        source_relative,
        target_relative,
        source_target,
        target_target,
    })
}

pub(crate) fn state_for_path(state: &PortalState, path: &PortalPath) -> Result<ResolvedPortalPath> {
    resolve_portal_path(state, super::path::portal_path_to_pathbuf(path))
}

pub(crate) fn ensure_readable_entry(entry: &EntryRecord) -> Result<()> {
    match entry.mode {
        AccessMode::ReadOnly | AccessMode::ReadWrite => Ok(()),
    }
}

pub(crate) fn ensure_writable_entry(entry: &EntryRecord) -> Result<()> {
    match entry.mode {
        AccessMode::ReadWrite => Ok(()),
        AccessMode::ReadOnly => Err(Error::PermissionDenied(format!(
            "entry '{}' is read-only",
            entry.name
        ))),
    }
}

pub(crate) fn immutable_segment_match(state: &PortalState, relative: &Path) -> Option<String> {
    relative.components().find_map(|component| match component {
        Component::Normal(segment) => {
            let segment = segment.to_str()?;
            state
                .immutable_segments
                .contains(segment)
                .then(|| segment.to_owned())
        }
        _ => None,
    })
}

pub(crate) fn ensure_mutable_relative_path(state: &PortalState, relative: &Path) -> Result<()> {
    if let Some(segment) = immutable_segment_match(state, relative) {
        return Err(Error::ImmutablePath { segment });
    }
    Ok(())
}

pub(crate) fn is_immutable_path_error(error: &Error) -> bool {
    matches!(error, Error::ImmutablePath { .. })
}

pub(crate) fn entry_is_read_only(entry: &EntryRecord, read_only_default: bool) -> bool {
    entry.mode == AccessMode::ReadOnly || read_only_default
}

pub(crate) fn errno_from_error(error: &Error) -> Errno {
    match error {
        Error::Io(err) => Errno::from_i32(err.raw_os_error().unwrap_or(libc::EIO)),
        Error::EntryNotFound(_) | Error::TargetNotFound(_) => Errno::ENOENT,
        Error::TargetNotDirectory(_) => Errno::ENOTDIR,
        Error::PermissionDenied(_) | Error::ImmutablePath { .. } => Errno::EPERM,
        Error::InvalidPortalPath(_) => Errno::EINVAL,
        _ => Errno::EIO,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use super::*;

    static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

    fn unique_path(prefix: &str) -> PathBuf {
        let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "workspace-portal-{prefix}-{}-{id}",
            std::process::id()
        ))
    }

    fn test_state() -> PortalState {
        let workspace = unique_path("fs-workspace");
        let mut state = PortalState::new(
            workspace.clone(),
            "abc123".to_owned(),
            workspace.join("socket.sock"),
        );
        state
            .add_entry(
                EntryRecord::new("docs", workspace.join("docs"), AccessMode::ReadWrite),
                false,
            )
            .unwrap();
        state
            .add_entry(
                EntryRecord::new("notes", workspace.join("notes"), AccessMode::ReadOnly),
                false,
            )
            .unwrap();
        state.freeze_segment("vendor".to_owned());
        state
    }

    #[test]
    fn resolves_reads_and_enforces_writes() {
        let state = test_state();
        let read = resolve_read_path(&state, "/notes/readme.md").unwrap();
        assert_eq!(read.entry.name, "notes");
        assert_eq!(
            read.target,
            state.entry("notes").unwrap().target.join("readme.md")
        );

        let err = resolve_write_path(&state, "/notes/readme.md").unwrap_err();
        assert!(matches!(err, Error::PermissionDenied(message) if message.contains("read-only")));
    }

    #[test]
    fn rejects_cross_entry_rename() {
        let state = test_state();

        let err = validate_rename(&state, "/docs/a.txt", "/notes/b.txt").unwrap_err();
        assert!(
            matches!(err, Error::PermissionDenied(message) if message.contains("cross-entry rename"))
        );

        let plan = validate_rename(&state, "/docs/a.txt", "/docs/b.txt").unwrap();
        assert_eq!(plan.entry.name, "docs");
        assert_eq!(
            plan.source_target,
            state.entry("docs").unwrap().target.join("a.txt")
        );
        assert_eq!(
            plan.target_target,
            state.entry("docs").unwrap().target.join("b.txt")
        );
    }

    #[test]
    fn rejects_mutations_under_immutable_segments() {
        let state = test_state();

        let err = resolve_write_path(&state, "/docs/vendor/lock.json").unwrap_err();
        assert!(is_immutable_path_error(&err));

        let err = resolve_write_path(&state, "/docs/src/vendor").unwrap_err();
        assert!(is_immutable_path_error(&err));

        let err =
            validate_rename(&state, "/docs/tmp/file.txt", "/docs/vendor/file.txt").unwrap_err();
        assert!(is_immutable_path_error(&err));
    }

    #[test]
    fn immutable_segment_matching_is_exact() {
        let state = test_state();

        assert!(ensure_mutable_relative_path(&state, Path::new("src/vendors/file.txt")).is_ok());
        let err =
            ensure_mutable_relative_path(&state, Path::new("src/vendor/file.txt")).unwrap_err();
        assert!(is_immutable_path_error(&err));
    }
}

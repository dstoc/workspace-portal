//! Confined host-path resolution for the FUSE layer.
//!
//! Every host filesystem operation the daemon performs must resolve strictly
//! beneath the entry's target directory, even if the backing store is mutated
//! between when an inode was cached and when the daemon acts on it (a TOCTOU
//! race; see `docs/proposals/symlink-confinement.md`). The plain
//! `entry.target.join(relative)` + std-fs approach follows symlinks in every
//! path component and can be raced into reading or writing outside the entry.
//!
//! This module resolves an entry-relative path against a pinned directory file
//! descriptor for the entry root using `openat2(2)` with
//! `RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS`. The kernel fails the resolution
//! (`EXDEV`) the instant any component — including a symlink swapped in by a
//! racing writer, or a `..` — would leave the entry root. Symlinks that stay
//! within the entry still resolve, so legitimate in-entry links are unaffected.
//!
//! Leaf operations that create or remove a name (mkdir, symlink, unlink, …) are
//! issued with the `*at` syscalls against a confined parent directory fd, so
//! the single final component cannot traverse anywhere.

use std::{
    ffi::{CString, OsStr, OsString},
    fs::{File, Metadata},
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};

use fuser::FileType;

// openat2(2) `resolve` flags. Defined locally to avoid depending on the libc
// crate surfacing them on every target.
const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
const RESOLVE_BENEATH: u64 = 0x08;

#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

fn cstr(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))
}

fn check(rc: libc::c_int) -> io::Result<()> {
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Open the entry root as a pinned `O_PATH` directory fd. `O_NOFOLLOW` fails
/// closed (`ELOOP`) if the root itself has been swapped for a symlink.
fn open_root(entry_root: &Path) -> io::Result<OwnedFd> {
    let c = cstr(entry_root)?;
    let fd = unsafe {
        libc::open(
            c.as_ptr(),
            libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Resolve `relative` beneath `root`, refusing any escape, and open it with the
/// given `open(2)` flags/mode. An empty `relative` refers to the root itself.
fn openat2_beneath(
    root: &OwnedFd,
    relative: &Path,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> io::Result<OwnedFd> {
    let rel = if relative.as_os_str().is_empty() {
        CString::new(".").unwrap()
    } else {
        cstr(relative)?
    };
    let how = OpenHow {
        flags: (flags | libc::O_CLOEXEC) as u64,
        mode: mode as u64,
        resolve: RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS,
    };
    let ret = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            root.as_raw_fd(),
            rel.as_ptr(),
            &how as *const OpenHow,
            std::mem::size_of::<OpenHow>(),
        )
    };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(ret as RawFd) })
}

/// Resolve the parent directory of `relative` beneath the entry root and return
/// a confined parent dir fd plus the final single-component leaf name. Leaf
/// operations then use the `*at` syscalls against this fd, so the leaf cannot
/// traverse out of the entry.
fn open_parent(entry_root: &Path, relative: &Path) -> io::Result<(OwnedFd, CString)> {
    let leaf = relative
        .file_name()
        .ok_or_else(|| io::Error::from_raw_os_error(libc::EINVAL))?;
    let parent = relative.parent().unwrap_or_else(|| Path::new(""));
    let root = open_root(entry_root)?;
    let parent_fd = openat2_beneath(&root, parent, libc::O_PATH | libc::O_DIRECTORY, 0)?;
    let leaf =
        CString::new(leaf.as_bytes()).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    Ok((parent_fd, leaf))
}

fn file_type_from_mode(mode: libc::mode_t) -> FileType {
    match mode & libc::S_IFMT {
        libc::S_IFDIR => FileType::Directory,
        libc::S_IFLNK => FileType::Symlink,
        libc::S_IFIFO => FileType::NamedPipe,
        libc::S_IFCHR => FileType::CharDevice,
        libc::S_IFBLK => FileType::BlockDevice,
        libc::S_IFSOCK => FileType::Socket,
        _ => FileType::RegularFile,
    }
}

/// Open a file beneath the entry root with the given `open(2)` flags. Symlinks
/// are followed only while they stay beneath the root.
pub(crate) fn open_file(
    entry_root: &Path,
    relative: &Path,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> io::Result<File> {
    let root = open_root(entry_root)?;
    let fd = openat2_beneath(&root, relative, flags, mode)?;
    Ok(File::from(fd))
}

/// `lstat` semantics confined beneath the entry root: intermediate components
/// are confined, the final component is not followed.
pub(crate) fn lstat(entry_root: &Path, relative: &Path) -> io::Result<Metadata> {
    let root = open_root(entry_root)?;
    let fd = openat2_beneath(&root, relative, libc::O_PATH | libc::O_NOFOLLOW, 0)?;
    File::from(fd).metadata()
}

/// Create a new file beneath the entry root via `openat`, confined to the entry.
pub(crate) fn create_file(
    entry_root: &Path,
    relative: &Path,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> io::Result<File> {
    let (parent, leaf) = open_parent(entry_root, relative)?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            leaf.as_ptr(),
            flags | libc::O_CLOEXEC,
            mode as libc::c_uint,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// List a directory beneath the entry root, returning `(name, type)` for each
/// child (excluding `.`/`..`). Child types come from `fstatat` with
/// `AT_SYMLINK_NOFOLLOW`, so symlinks are reported as symlinks.
pub(crate) fn list_dir(
    entry_root: &Path,
    relative: &Path,
) -> io::Result<Vec<(OsString, FileType)>> {
    let root = open_root(entry_root)?;
    let dir_fd = openat2_beneath(&root, relative, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
    let dir_raw = dir_fd.as_raw_fd();

    // fdopendir takes ownership of the fd it is given (closedir closes it), so
    // hand it a dup and keep `dir_fd` for the per-child fstatat calls.
    let dup_fd = unsafe { libc::dup(dir_raw) };
    if dup_fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let dirp = unsafe { libc::fdopendir(dup_fd) };
    if dirp.is_null() {
        unsafe { libc::close(dup_fd) };
        return Err(io::Error::last_os_error());
    }

    let mut entries = Vec::new();
    loop {
        let ent = unsafe { libc::readdir(dirp) };
        if ent.is_null() {
            break;
        }
        let name = unsafe { std::ffi::CStr::from_ptr((*ent).d_name.as_ptr()) };
        let name_bytes = name.to_bytes();
        if name_bytes == b"." || name_bytes == b".." {
            continue;
        }
        let cname = match CString::new(name_bytes) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let rc =
            unsafe { libc::fstatat(dir_raw, cname.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) };
        if rc != 0 {
            continue;
        }
        entries.push((
            OsStr::from_bytes(name_bytes).to_os_string(),
            file_type_from_mode(st.st_mode),
        ));
    }
    unsafe { libc::closedir(dirp) };
    Ok(entries)
}

pub(crate) fn mkdir(entry_root: &Path, relative: &Path, mode: libc::mode_t) -> io::Result<()> {
    let (parent, leaf) = open_parent(entry_root, relative)?;
    check(unsafe { libc::mkdirat(parent.as_raw_fd(), leaf.as_ptr(), mode) })
}

pub(crate) fn symlink(entry_root: &Path, relative: &Path, target: &Path) -> io::Result<()> {
    let (parent, leaf) = open_parent(entry_root, relative)?;
    let ctarget = cstr(target)?;
    check(unsafe { libc::symlinkat(ctarget.as_ptr(), parent.as_raw_fd(), leaf.as_ptr()) })
}

pub(crate) fn unlink(entry_root: &Path, relative: &Path) -> io::Result<()> {
    let (parent, leaf) = open_parent(entry_root, relative)?;
    check(unsafe { libc::unlinkat(parent.as_raw_fd(), leaf.as_ptr(), 0) })
}

pub(crate) fn hard_link(entry_root: &Path, source: &Path, destination: &Path) -> io::Result<()> {
    let (source_parent, source_leaf) = open_parent(entry_root, source)?;
    let (destination_parent, destination_leaf) = open_parent(entry_root, destination)?;
    check(unsafe {
        libc::linkat(
            source_parent.as_raw_fd(),
            source_leaf.as_ptr(),
            destination_parent.as_raw_fd(),
            destination_leaf.as_ptr(),
            0,
        )
    })
}

pub(crate) fn rmdir(entry_root: &Path, relative: &Path) -> io::Result<()> {
    let (parent, leaf) = open_parent(entry_root, relative)?;
    check(unsafe { libc::unlinkat(parent.as_raw_fd(), leaf.as_ptr(), libc::AT_REMOVEDIR) })
}

pub(crate) fn readlink(entry_root: &Path, relative: &Path) -> io::Result<PathBuf> {
    let (parent, leaf) = open_parent(entry_root, relative)?;
    let mut buf = vec![0u8; libc::PATH_MAX as usize];
    let len = unsafe {
        libc::readlinkat(
            parent.as_raw_fd(),
            leaf.as_ptr(),
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
        )
    };
    if len < 0 {
        return Err(io::Error::last_os_error());
    }
    buf.truncate(len as usize);
    Ok(PathBuf::from(OsString::from(OsStr::from_bytes(&buf))))
}

/// Rename within a single entry. Both endpoints resolve beneath the same entry
/// root (cross-entry rename is rejected before this is called).
pub(crate) fn rename(entry_root: &Path, source: &Path, destination: &Path) -> io::Result<()> {
    let (src_parent, src_leaf) = open_parent(entry_root, source)?;
    let (dst_parent, dst_leaf) = open_parent(entry_root, destination)?;
    check(unsafe {
        libc::renameat(
            src_parent.as_raw_fd(),
            src_leaf.as_ptr(),
            dst_parent.as_raw_fd(),
            dst_leaf.as_ptr(),
        )
    })
}

/// `chmod` the inode `relative` resolves to, confined beneath the entry root.
pub(crate) fn chmod(entry_root: &Path, relative: &Path, mode: libc::mode_t) -> io::Result<()> {
    // Resolve to a confined O_PATH fd (following in-entry symlinks but never
    // escaping), then chmod the pinned inode via /proc/self/fd.
    let root = open_root(entry_root)?;
    let fd = openat2_beneath(&root, relative, libc::O_PATH, 0)?;
    let proc_path = CString::new(format!("/proc/self/fd/{}", fd.as_raw_fd())).unwrap();
    check(unsafe { libc::chmod(proc_path.as_ptr(), mode) })
}

/// Apply `utimensat` timestamps to the inode `relative` resolves to, confined
/// beneath the entry root.
pub(crate) fn set_times(
    entry_root: &Path,
    relative: &Path,
    times: &[libc::timespec; 2],
) -> io::Result<()> {
    let root = open_root(entry_root)?;
    let fd = openat2_beneath(&root, relative, libc::O_PATH, 0)?;
    let proc_path = CString::new(format!("/proc/self/fd/{}", fd.as_raw_fd())).unwrap();
    check(unsafe { libc::utimensat(libc::AT_FDCWD, proc_path.as_ptr(), times.as_ptr(), 0) })
}

/// Truncate the file `relative` resolves to, confined beneath the entry root.
pub(crate) fn truncate(entry_root: &Path, relative: &Path, size: u64) -> io::Result<()> {
    let file = open_file(entry_root, relative, libc::O_WRONLY, 0)?;
    file.set_len(size)
}

/// `statvfs` the filesystem backing `relative`, confined beneath the entry root.
pub(crate) fn statvfs(entry_root: &Path, relative: &Path) -> io::Result<libc::statvfs> {
    let root = open_root(entry_root)?;
    let fd = openat2_beneath(&root, relative, libc::O_PATH, 0)?;
    let mut buf: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::fstatvfs(fd.as_raw_fd(), &mut buf) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink as unix_symlink;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

    fn temp_root(prefix: &str) -> PathBuf {
        let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!(
            "workspace-portal-safeopen-{prefix}-{}-{id}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn reads_a_file_inside_the_entry() {
        let root = temp_root("read");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/file"), b"inside").unwrap();

        let md = lstat(&root, Path::new("sub/file")).unwrap();
        assert!(md.is_file());
        assert_eq!(md.len(), 6);

        let mut f = open_file(&root, Path::new("sub/file"), libc::O_RDONLY, 0).unwrap();
        let mut s = String::new();
        std::io::Read::read_to_string(&mut f, &mut s).unwrap();
        assert_eq!(s, "inside");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn follows_in_entry_symlinks() {
        let root = temp_root("in-entry-link");
        std::fs::create_dir_all(root.join("real")).unwrap();
        std::fs::write(root.join("real/data"), b"ok").unwrap();
        // A symlink that stays beneath the root must resolve.
        unix_symlink("real", root.join("link")).unwrap();

        let md = lstat(&root, Path::new("link/data")).unwrap();
        assert!(md.is_file());
        assert_eq!(md.len(), 2);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rejects_symlink_escaping_the_entry() {
        let root = temp_root("escape");
        let outside = temp_root("escape-outside");
        std::fs::write(outside.join("secret"), b"must-not-leak").unwrap();
        // An absolute symlink that escapes the entry root.
        unix_symlink(&outside, root.join("esc")).unwrap();

        // Following the escaping link out of the entry must fail with EXDEV.
        let err = lstat(&root, Path::new("esc/secret")).unwrap_err();
        assert_eq!(
            err.raw_os_error(),
            Some(libc::EXDEV),
            "expected EXDEV, got {err:?}"
        );

        let open_err = open_file(&root, Path::new("esc/secret"), libc::O_RDONLY, 0).unwrap_err();
        assert_eq!(open_err.raw_os_error(), Some(libc::EXDEV));

        // The escaping symlink itself is still observable (we just don't follow it).
        let link_md = lstat(&root, Path::new("esc")).unwrap();
        assert!(link_md.file_type().is_symlink());

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn rejects_parent_traversal() {
        let root = temp_root("dotdot");
        let outside = temp_root("dotdot-outside");
        std::fs::write(outside.join("secret"), b"x").unwrap();

        // `..` that climbs out of the root is refused even though the path is
        // lexically valid on the host.
        let rel = PathBuf::from("../")
            .join(outside.file_name().unwrap())
            .join("secret");
        let err = lstat(&root, &rel).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EXDEV));

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn list_dir_reports_children_without_following() {
        let root = temp_root("list");
        std::fs::create_dir_all(root.join("d/inner")).unwrap();
        std::fs::write(root.join("d/f"), b"x").unwrap();
        unix_symlink("/etc", root.join("d/lnk")).unwrap();

        let mut names: Vec<_> = list_dir(&root, Path::new("d"))
            .unwrap()
            .into_iter()
            .map(|(n, t)| (n.to_string_lossy().into_owned(), t))
            .collect();
        names.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(names.len(), 3);
        assert!(
            names
                .iter()
                .any(|(n, t)| n == "f" && *t == FileType::RegularFile)
        );
        assert!(
            names
                .iter()
                .any(|(n, t)| n == "inner" && *t == FileType::Directory)
        );
        // The symlink is reported as a symlink, not followed to /etc.
        assert!(
            names
                .iter()
                .any(|(n, t)| n == "lnk" && *t == FileType::Symlink)
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn create_and_remove_within_entry() {
        let root = temp_root("write");
        std::fs::create_dir_all(root.join("d")).unwrap();

        mkdir(&root, Path::new("d/made"), 0o755).unwrap();
        assert!(root.join("d/made").is_dir());

        let _ = create_file(
            &root,
            Path::new("d/new"),
            libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
            0o644,
        )
        .unwrap();
        assert!(root.join("d/new").is_file());

        unlink(&root, Path::new("d/new")).unwrap();
        assert!(!root.join("d/new").exists());

        rmdir(&root, Path::new("d/made")).unwrap();
        assert!(!root.join("d/made").exists());

        let _ = std::fs::remove_dir_all(&root);
    }
}

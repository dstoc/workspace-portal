# Proposal: persist timestamp changes in `setattr`

## Motivation

Build systems decide what to rebuild from file modification times. `make`,
`ninja`, many code generators, and archive extractors (`tar`, `unzip`,
`cp -p`, `rsync -t`) all set or preserve mtimes explicitly via `utimensat(2)`.
Inside a workspace-portal mount those operations are issued as FUSE `setattr`
calls carrying `atime`/`mtime` values.

Today `setattr` ignores them. The host file's mtime still advances naturally on
`write`, but any *explicit* timestamp set is silently discarded — and the call
still reports success. A `touch -d`, a `cp -p`, or a tar extraction therefore
appears to work while leaving the on-disk mtime unchanged, which can desync
incremental builds (stale artifacts treated as fresh, or fresh sources treated
as stale and rebuilt forever).

## Problem statement

`fn setattr` in `src/fs/callbacks.rs:344` applies `mode` (via
`fs::set_permissions`, line 385) and `size` (via `set_len`, line 394), then
returns the current attributes. The timestamp parameters are bound as `_atime`,
`_mtime`, `_ctime`, `_crtime`, `_chgtime`, `_bkuptime` and never read. So:

```text
touch -d '2020-01-01' workspace/project-a/foo   # exits 0, mtime unchanged
cp -p src dst                                    # dst mtime != src mtime
tar -xf archive.tar                              # extracted mtimes are "now"
```

This is the one `setattr` path that fails silently rather than with an errno,
which makes it the most dangerous: callers have no signal that the operation
did not take effect.

## Proposal

Apply `atime` and `mtime` in `setattr` when present, using `utimensat(2)` on the
resolved host path (or `futimens(2)` on the open handle when `fh` is provided,
matching the existing `size`/`fh` branch at line 395).

### Timestamp value mapping

fuser delivers `_atime`/`_mtime` as `Option<TimeOrNow>` and `_ctime` as
`Option<SystemTime>`. Map each field to a `libc::timespec` using the standard
`utimensat` sentinels:

- `None` (field not being changed) → `UTIME_OMIT`
- `Some(TimeOrNow::Now)` → `UTIME_NOW`
- `Some(TimeOrNow::SpecificTime(t))` → seconds/nanos derived from `t`

Only call into `utimensat`/`futimens` when at least one of `atime`/`mtime` is
`Some`, so the common mode-only or size-only `setattr` keeps its current code
path untouched.

### `ctime`, `crtime`, and the other fields

`_ctime`, `_crtime`, `_chgtime`, `_bkuptime`, and `_flags` remain ignored.
`utimensat` cannot set an arbitrary ctime, and the BSD/crtime fields have no
Linux backing here. This matches existing behavior and is not a regression —
the proposal only adds the atime/mtime path that real tooling depends on.

### Symlink inode handling

The kernel resolves the inode before calling `setattr`, so the call may target
a symlink inode itself. Use `AT_SYMLINK_NOFOLLOW` with the path-based
`utimensat` so the timestamps land on the exact inode the kernel addressed
rather than the symlink's target. The `futimens` (open-handle) branch already
operates on the opened inode and needs no flag.

### Implementation shape

In `src/fs/callbacks.rs`, after the existing `size` block and before the final
`current_attr` reply:

1. If `_atime.is_none() && _mtime.is_none()`, skip straight to the reply (no
   change in behavior).
2. Build two `libc::timespec` values from `_atime`/`_mtime` using the mapping
   above.
3. If `fh` is `Some` and its handle is `writable`, call `libc::futimens(fd, ..)`
   on the handle's file descriptor (`handle.file.as_raw_fd()`).
4. Otherwise call `libc::utimensat(libc::AT_FDCWD, target, .., AT_SYMLINK_NOFOLLOW)`
   on `resolved.target`.
5. On error, reply with `Errno::from_i32(errno)`; on success fall through to the
   existing `current_attr` reply.

The read-only and `EROFS` checks already guarding `setattr`
(`src/fs/callbacks.rs:376`) cover the new path unchanged — a timestamp set on a
read-only entry is rejected before any host call, which is correct.

`libc` is already a direct dependency; no new crate or `nix` feature is
required.

## Non-goals

- Setting `ctime`/`crtime` to caller-supplied values. Not supported by the
  underlying syscall; out of scope.
- Honoring `uid`/`gid` changes. `setattr` already returns `EPERM` for those
  (`src/fs/callbacks.rs:380`) and that stays.
- Extended-attribute timestamps or any xattr work. Separate concern.

## Verification

1. Mount a read-write entry over a scratch directory.
2. `touch -d '2020-01-01T00:00:00' workspace/entry/file`; confirm `stat`
   through the mount and on the host path both report the 2020 mtime.
3. `touch -a -d '2021-02-02' workspace/entry/file`; confirm atime changed and
   mtime did not (`UTIME_OMIT` path).
4. `cp -p host_src workspace/entry/dst`; confirm `dst` mtime equals `src` mtime.
5. Extract a tarball containing known mtimes into the entry; confirm the
   restored files keep their archived mtimes.
6. Repeat step 2 against a read-only entry; confirm a non-zero errno
   (`EROFS`) and no change.
7. Run the full E2E suite: `./scripts/fuse-e2e-podman.sh`.

A new ignored E2E test `fuse_e2e_setattr_persists_timestamps` in
`tests/fuse_e2e.rs` should cover steps 2–6 with the existing `Fixture`/`run`
helpers, following the pattern of
`fuse_e2e_file_lifecycle_covers_create_append_overwrite_truncate_and_fsync`.

## Success criteria

- `utimensat`/`futimens` through the mount changes the on-host mtime/atime.
- `UTIME_OMIT` semantics hold: setting only atime leaves mtime intact and vice
  versa.
- `cp -p` and tar extraction preserve source mtimes through the mount.
- Timestamp sets on read-only entries still fail with `EROFS`.
- Mode-only and size-only `setattr` calls are byte-for-byte unchanged in
  behavior.
- All existing tests continue to pass.

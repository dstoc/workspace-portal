# Proposal: report real free space from `statfs`

## Motivation

A workspace-portal mount exists to expose real host directories for normal
development. Tools that write into it — package managers, installers,
compilers' temp-file logic, SQLite, `git gc` — frequently call `statvfs(3)` and
check `f_bavail`/`f_bfree` before committing to a write, and refuse or warn when
the filesystem looks full. `df` on the mount is also a routine operator check.

Today the mount reports itself as a completely full, zero-capacity filesystem.
Writes still succeed (they land on the backing file, which has real space), but
any caller that trusts `statvfs` sees "no space available" and may abort before
it ever tries.

## Problem statement

`fn statfs` in `src/fs/callbacks.rs:1274` replies with hardcoded values:

```rust
reply.statfs(state.entries.len() as u64 + 1, 0, 0, 0, 0, 4096, 255, 0);
//           blocks                          bfree bavail files ffree bsize namelen frsize
```

`bfree`, `bavail`, `files`, and `ffree` are all `0`, and `blocks` is just the
entry count. So `df` shows a ~tiny, 100%-full filesystem and
`statvfs().f_bavail == 0`. Observable failures:

```text
df -h workspace            # 0 used, 0 avail, 100% full
some-installer ./workspace # "No space left on device" pre-check, never writes
```

The backing filesystem's true capacity is readily available — the portal just
isn't reporting it.

## Proposal

Answer `statfs` from a real `statvfs(2)` of the host path that backs the
queried inode, and pass its capacity counters through.

### Which backing path to measure

`statfs` receives the inode it was called on (`_ino`). Resolve it the same way
the rest of the callbacks do:

1. If `_ino == ROOT_INO`, measure the workspace mount directory
   (`state.workspace`) — it always exists and is a reasonable stand-in for the
   namespace as a whole.
2. Otherwise resolve the inode to its `PortalPath` via
   `runtime.path_for_inode`, then `state_for_path` to get `resolved.target`, and
   measure that host path.
3. If resolution fails for any reason, fall back to measuring `state.workspace`.

This keeps the answer correct for the common case (a tool calls `statvfs` on a
path inside one entry and gets that entry's backing filesystem) without
inventing a synthetic aggregate across entries that may live on different
filesystems.

### Field mapping

Call `libc::statvfs` on the chosen path and forward its fields to
`reply.statfs(blocks, bfree, bavail, files, ffree, bsize, namelen, frsize)`:

- `blocks` ← `f_blocks`
- `bfree` ← `f_bfree`
- `bavail` ← `f_bavail`
- `files` ← `f_files`
- `ffree` ← `f_ffree`
- `bsize` ← `f_bsize`
- `frsize` ← `f_frsize`
- `namelen` ← `f_namemax` (retain the current `255` as the fallback if zero)

`libc` is already a direct dependency, so no new crate is needed.

### Cross-filesystem honesty

Because each entry can be backed by a different filesystem, a single mount-wide
number would be a lie for at least some paths. Measuring the backing path of the
queried inode means each `statvfs` answer is accurate for the path the caller
actually asked about. This is the narrowest correct behavior and avoids
inventing an aggregate.

## Non-goals

- Synthesizing a single aggregate capacity across all entries. Entries can span
  filesystems; a per-inode answer is both simpler and more accurate.
- Quota or per-entry accounting. The portal reports the backing filesystem's
  real numbers, nothing more.
- Reporting a fake-large capacity to satisfy pre-checks. The goal is truth, not
  a different constant.

## Verification

1. Mount a read-write entry over a directory on a filesystem with known free
   space.
2. `df -h workspace/entry`; confirm size/avail roughly match the host's
   `df -h <target>`.
3. `stat -f workspace/entry`; confirm nonzero `Blocks free`/`Blocks available`.
4. Confirm `df workspace` (root inode) reports the workspace filesystem's real
   numbers rather than zeros.
5. Run the full E2E suite: `./scripts/fuse-e2e-podman.sh`.

A new ignored E2E test `fuse_e2e_statfs_reports_backing_capacity` in
`tests/fuse_e2e.rs` should assert that a `statvfs` (via `nix` or a `df`/`stat -f`
subprocess) on an entry path returns nonzero available blocks, using the
existing `Fixture`/`run` helpers.

## Success criteria

- `statvfs` on a path inside an entry returns the backing filesystem's real
  `f_blocks`/`f_bfree`/`f_bavail`.
- `df` on the mount no longer shows a zero-capacity, 100%-full filesystem.
- `statfs` on the root inode returns the workspace filesystem's real numbers.
- Resolution failure degrades to measuring the workspace directory rather than
  erroring.
- All existing tests continue to pass.

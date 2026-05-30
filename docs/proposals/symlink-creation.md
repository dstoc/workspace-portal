# Proposal: symlink creation inside workspace entries

## Motivation

Tools that run inside a workspace-portal mount — build systems, package
managers, language servers — occasionally create symlinks as part of their
normal operation. For example, `npm install` creates symlinks in
`node_modules/.bin`; some build scripts create convenience aliases next to
output artifacts. Today those operations fail silently with `ENOSYS` because
the `symlink` FUSE callback is not implemented, even though the entry is
read-write and the host filesystem supports symlinks.

Reading symlinks already works: `readlink` is implemented
(`src/fs/callbacks.rs:923`) and the `lookup` + `getattr` paths return the
correct `FileType::Symlink` for symlinks that already exist on the host.
The missing piece is the write side.

## Problem statement

When a process inside the mount calls `symlink(target, linkpath)` and
`linkpath` falls inside a read-write entry, FUSE returns `ENOSYS` because
`PortalFs` does not override the `symlink` method from the `Filesystem` trait.
The fuser default is to return `ENOSYS`, which most callers interpret as a
hard, permanent failure rather than a transient one.

## Proposal

Implement `fn symlink` on `PortalFs` in `src/fs/callbacks.rs`.

The callback signature provided by fuser is:

```rust
fn symlink(
    &self,
    _req: &Request,
    parent: INodeNo,
    link_name: &OsStr,
    target: &Path,
    reply: ReplyEntry,
)
```

Where:
- `parent` is the inode of the directory that will contain the new symlink.
- `link_name` is the filename of the new symlink (single component).
- `target` is the symlink content — the path the symlink will point to. It is
  not constrained to be within the workspace; it may be absolute or relative,
  and its validity is not our concern.

### Implementation shape

The implementation follows the same pattern as `create` and `mkdir`:

1. Reject `parent == ROOT_INO` with `EPERM` — symlinks cannot be created
   directly in the portal root, consistent with how `create` and `mkdir` behave.

2. Call `runtime.resolve_parent_child_writable(&state, parent, &link_name)` to
   resolve the host path and enforce the read-only check. On failure, return
   `EACCES`.

3. Call `std::os::unix::fs::symlink(target, &resolved.target)` to create the
   symlink on the host.

4. Stat the newly created symlink with `fs::symlink_metadata(&resolved.target)`
   (important: `symlink_metadata` does not follow the link).

5. Assign an inode with `runtime.cache_portal_path(path)`, build the attr with
   `attr_from_metadata`, and reply with `reply.entry(&TTL, &attr, Generation(...))`.

The host `symlink(2)` call will fail with `EEXIST` if a file already exists at
the target path, propagated as-is. No additional existence check is needed.

### Read-only behavior

`resolve_parent_child_writable` already rejects read-only entries and a
read-only workspace mount with `PermissionDenied`, which maps to `EPERM` via
`errno_from_error`. No new policy is needed.

### Symlink content confinement

The symlink `target` (the path the symlink points to) is written verbatim to
the host. The portal makes no attempt to confine it to within the entry or the
workspace. This is consistent with how broken symlinks are already treated:
they are visible and readable as symlinks but traversal fails if the target
does not exist. A process with write access to an entry can already create
arbitrary files there; a dangling or escaping symlink is no worse.

This proposal does not add any confinement policy. That remains in the
existing list of known limits (`docs/workspace-portal.md` §Known current
limits).

## Non-goals

- Confinement of symlink targets to within the entry subtree. This would
  require `openat2`/`RESOLVE_NO_SYMLINKS`-style resolution and is a separate
  effort already noted in near-term future work.
- Exposing symlink creation at the portal root level. Top-level entries are
  controlled through the daemon protocol, not the filesystem interface.
- Any change to how existing symlinks are traversed or read. `readlink` and
  `lookup` already handle those paths.

## Verification

1. Mount a read-write workspace entry over a scratch directory.
2. Run `ln -s ./target ./workspace/entry/link` inside the mount; confirm it
   succeeds and `ls -la` shows a symlink.
3. Run `readlink ./workspace/entry/link`; confirm it returns `./target`.
4. Attempt the same in a read-only entry; confirm `EPERM` or `EROFS`.
5. Attempt to create a symlink directly at the workspace root
   (`ln -s x ./workspace/link`); confirm `EPERM`.
6. Run the full E2E suite: `./scripts/fuse-e2e-podman.sh`.

A new ignored E2E test `fuse_e2e_symlink_creation` in `tests/fuse_e2e.rs`
should cover steps 2–5 using the existing `Fixture` and `run` helpers,
following the pattern of `fuse_e2e_symlinks_cover_traversal_and_broken_targets`.

## Success criteria

- `std::os::unix::fs::symlink` called through the FUSE mount succeeds for a
  read-write entry.
- The created symlink is immediately visible via `readlink` and `fs::symlink_metadata`
  both through the mount and on the underlying host path.
- A symlink creation attempt on a read-only entry returns a non-zero errno
  (EPERM or EROFS).
- All existing tests continue to pass.

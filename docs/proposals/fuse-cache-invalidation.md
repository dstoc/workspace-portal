# Proposal: invalidate FUSE namespace caches on mutation

## Motivation

`workspace-portal` currently returns zero TTLs for every FUSE entry and
attribute response:

```rust
// Use zero TTLs until we implement explicit invalidation on namespace changes.
// This avoids stale positive and negative dentries surviving successful renames.
pub(crate) const TTL: Duration = Duration::from_secs(0);
```

That choice is intentionally correctness-biased. It avoids a real failure mode:
tools such as `rustc` write a temporary file, rename it into place, then
immediately open the destination. If the kernel has cached a negative dentry for
the destination name, the immediate open can see cached `ENOENT` even though the
daemon successfully completed the rename. The ignored E2E test
`fuse_e2e_rename_destination_is_immediately_openable` in `tests/fuse_e2e.rs`
captures this pattern.

Zero TTLs are a broad workaround. They force frequent round trips into the
daemon even when the namespace has not changed, and they prevent the filesystem
from using the kernel's normal lookup and attribute cache for stable paths. The
MVP used a one-second TTL, and fuser's own examples commonly choose one second,
but there is no fuser default for these replies: the filesystem supplies a TTL
to each positive entry and attribute response. The right fix is to tell the
kernel exactly which cached names became stale when the daemon mutates the
namespace.

## Problem statement

There are two cache layers involved:

- `FuseRuntime` tracks daemon-side `PortalPath <-> inode` mappings in
  `src/fs/runtime.rs`.
- The kernel tracks FUSE dentries and attributes according to the TTLs returned
  from `lookup`, `getattr`, `create`, `mkdir`, `symlink`, and `link` in
  `src/fs/callbacks.rs`.

The daemon-side cache already handles renames with
`FuseRuntime::rename_cached_subtree`. That keeps old inodes resolving to their
new `PortalPath` after a successful rename. It does not invalidate kernel
dentries. With a nonzero TTL, the kernel may still believe:

- a newly created or renamed destination does not exist because it cached a
  negative lookup before the mutation
- a removed or renamed source still exists because it cached a positive lookup
  before the mutation
- a replaced destination still points at the old inode until the TTL expires
- a top-level entry added, removed, or replaced by the control daemon is stale
  at `ROOT_INO`

The current mount path also makes invalidation unavailable to the filesystem
callbacks. `PortalFs::mount` calls `fuser::spawn_mount2` in `src/fs.rs`, returns
a `fuser::BackgroundSession`, and stores that session in `Daemon` in
`src/daemon/runtime.rs`. The callbacks that perform nested FUSE mutations live
inside `PortalFs`, but the `Notifier` is reachable from `BackgroundSession`.

`fuser 0.17` exposes the needed API:

- `BackgroundSession::notifier()`
- `Notifier::inval_entry(parent, name)`
- `Notifier::inval_inode(ino, offset, len)`
- `Notifier::delete(parent, child, name)`

The missing piece is a narrow bridge from successful namespace mutations to
those notifications. The first implementation should use `inval_entry` as the
primary operation. `delete` adds watcher semantics and stricter child-inode
requirements, so it is intentionally deferred until there is a separate need for
watcher behavior.

## Proposal

Add explicit FUSE dentry invalidation for every operation that changes the
visible namespace, then restore the previous one-second entry TTL once the
invalidation path is covered by tests. Keep standalone attribute replies at zero
in the first pass unless `Notifier::inval_inode` attribute invalidation
semantics are confirmed during implementation. Lookup and create replies use
fuser's high-level entry-response TTL, which also applies to the attributes
returned in those responses.

The first use case is the existing rustc-style rename pattern:

1. `lookup docs/target/fs-race/libtest-1.rmeta` returns `ENOENT` and the kernel
   caches that negative dentry.
2. `rename docs/target/fs-race/rmeta0001/full.rmeta ->
   docs/target/fs-race/libtest-1.rmeta` succeeds.
3. The daemon invalidates the destination name in the destination parent.
4. The immediate open of `libtest-1.rmeta` re-enters the daemon instead of using
   the stale negative dentry.

Generalize only to the namespace mutations already implemented today:
`create`, `mkdir`, `symlink`, `unlink`, `rmdir`, `rename`, `link`, and top-level
entry add/remove/replace from the control daemon.

### Notifier plumbing

Add a notifier slot to `PortalFs`, for example:

```rust
notifier: Arc<Mutex<Option<fuser::Notifier>>>
```

Change `PortalFs::mount` to create a `fuser::Session` explicitly rather than
using `spawn_mount2` directly:

1. Build the mount config with the existing `build_mount_config`.
2. Clone the notifier slot out of `PortalFs`.
3. Call `fuser::Session::new(self, mountpoint, &config)`.
4. Store `session.notifier()` in the cloned slot.
5. Call `session.spawn()` and return the existing `BackgroundSession` type.

This keeps `Daemon`'s ownership model unchanged: `Daemon` still stores
`Option<fuser::BackgroundSession>` and dropping it still unmounts. The important
property is that the notifier is installed before the filesystem thread starts
serving requests.

Top-level control-plane mutations do not need access to `PortalFs`. `Daemon`
already owns the `BackgroundSession`, so `Add { .. }` and `Remove { .. }` can
call `self.mount.as_ref().map(|mount| mount.notifier())` after a successful
state mutation.

### Invalidation helper

Add a small helper in `src/fs/callbacks.rs` or `src/fs/runtime.rs` that hides
the optional notifier and the "log but do not fail the completed mutation"
policy:

```rust
fn invalidate_entry(notifier: &Option<Notifier>, parent: INodeNo, name: &OsStr);
```

`Notifier::inval_entry` already treats `ENOENT` as harmless inside fuser. Any
other invalidation failure should be logged with the operation, parent inode,
and name, but the filesystem operation should still return the result of the
underlying mutation. Once a rename or unlink succeeds on disk, returning an
error because the cache notification failed would leave the caller with an
incorrect view of what happened. The fallback is bounded by the TTL selected
after this proposal is implemented.

Before raising `ENTRY_TTL`, missing notifier state is a warning and behavior
remains correct because the current single TTL is still zero. `ENTRY_TTL` should
not become nonzero until the mount path installs the notifier before request
handling and the E2E tests below pass.

### Mutation semantics

For nested FUSE operations in `src/fs/callbacks.rs`:

- `create`, `mkdir`, `symlink`, `link`: after the backing operation succeeds
  and before replying with the new entry, invalidate `(parent, name)` to clear
  any stale negative dentry. Then return the normal entry/create reply so the
  kernel can cache the new positive lookup.
- `unlink`, `rmdir`: after successful removal, invalidate `(parent, name)`.
  Do not use `Notifier::delete` in the first pass; watcher notification is not
  required for this cache-coherence fix.
- `rename`: after the backing rename succeeds and
  `runtime.rename_cached_subtree(&source_path, &target_path)` runs, invalidate
  both `(parent, old_name)` and `(newparent, new_name)`. This handles both stale
  positive source dentries and stale negative or replaced destination dentries.
- `link`: invalidate the new destination name. The source name is unchanged.
- Parent directory attributes are not invalidated in v1. Keep `ATTR_TTL = 0`
  for standalone `reply.attr(...)` calls until the exact `inval_inode`
  offset/length convention for attribute-only invalidation is confirmed against
  fuser's Linux behavior. This keeps dentry correctness independent of
  standalone attribute-cache assumptions. Attributes returned by
  `reply.entry(...)` and `reply.created(...)` inherit `ENTRY_TTL` because
  fuser 0.17 exposes only one TTL for those high-level entry replies.

For control-plane namespace changes in `src/daemon/runtime.rs`:

- `Add { name, replace: false, .. }`: after `state.add_entry` succeeds,
  invalidate `(ROOT_INO, name)`, clearing any stale negative root dentry.
- `Add { name, replace: true, .. }`: invalidate `(ROOT_INO, name)`, clearing a
  stale positive dentry for the replaced target as well as a stale negative one.
- `Remove { name }`: after `state.remove_entry` succeeds, invalidate
  `(ROOT_INO, name)`.

The proposal does not require invalidating every descendant when a top-level
entry is removed. Soft revocation remains the design: removed entries disappear
from new lookup by name, while already-open handles continue to work until
closed.

### TTL restoration

After invalidation is implemented and tested, replace the single `TTL` constant
with named TTLs:

```rust
pub(crate) const ENTRY_TTL: Duration = Duration::from_secs(1);
pub(crate) const ATTR_TTL: Duration = Duration::from_secs(0);
```

Use `ENTRY_TTL` for replies that create or refresh dentries:
`reply.entry(...)` and `reply.created(...)`. In fuser 0.17 these high-level
entry replies accept a single TTL and pass it to the low-level response as both
entry validity and attribute validity, so lookup-returned and create-returned
attributes also get `ENTRY_TTL`.

Use `ATTR_TTL` for standalone `reply.attr(...)`. Keep it at zero for v1 unless
attribute-cache invalidation is confirmed and covered by tests.

Use one second for `ENTRY_TTL` because it is the project's pre-zero-TTL
baseline, not because fuser supplies that as a default. It is enough to avoid
the worst "every lookup re-enters the daemon" behavior while keeping the
stale-window small if the kernel drops or rejects an invalidation for an
unexpected reason. Longer entry TTLs and nonzero attribute TTLs can be later
performance changes with dedicated tests.

## Non-goals

- Redesigning `FuseRuntime` inode assignment. The existing
  `rename_cached_subtree` behavior stays; this proposal only adds kernel-cache
  invalidation around it.
- Implementing hard revocation. Removing a top-level entry still does not tear
  down existing file handles.
- Adding a user-facing cache tuning flag. A fixed conservative TTL is enough for
  the first correct implementation.
- Watching backing-store changes made outside the daemon. This proposal covers
  namespace changes caused by `workspace-portal` operations, not arbitrary host
  edits under an entry target.
- Using notifications to update file data caches. The implemented read/write
  path does not currently opt into a data-cache policy that requires this.

## Verification

1. Unit-test the mount configuration path enough to prove `PortalFs::mount`
   installs a notifier before spawning the session. This can be a small test
   around the new construction helper if direct FUSE mounting is not practical
   in unit tests.
2. Add focused unit tests for any helper that maps `PortalPath` plus basename to
   invalidation targets, especially root entries and nested paths.
3. Run the FUSE E2E suite with `ENTRY_TTL = Duration::from_secs(1)` and
   `ATTR_TTL = Duration::from_secs(0)`, then enable
   `fuse_e2e_rename_destination_is_immediately_openable`. It must pass without
   relying on zero entry TTLs.
4. Add an E2E test for stale positive dentries: create a file through the mount,
   stat it to populate the dentry, unlink it through the mount, then immediately
   assert a fresh open/stat of the same path returns `ENOENT`.
5. Add an E2E test for stale root dentries: attempt to stat a missing top-level
   entry, add it through the control command while the daemon is mounted, then
   immediately stat/open it through the mount.
6. Keep the existing soft-revocation E2E coverage passing. Removing a top-level
   entry invalidates new root lookups but does not break already-open handles.
7. Run the normal Rust checks (`cargo test`, plus the ignored FUSE E2E tests in
   an environment with FUSE support).

## Success criteria

- `src/fs.rs` no longer uses zero entry TTLs as the steady-state dentry cache
  policy.
- Successful `rename` invalidates both old and new dentries before the caller's
  immediate follow-up open can observe stale kernel state.
- Successful create/link/symlink/mkdir clear stale negative dentries for the new
  name.
- Successful unlink/rmdir/remove clear stale positive dentries for the removed
  name.
- Top-level add/remove/replace through the daemon invalidates `ROOT_INO` names.
- Existing soft-revocation behavior and daemon-side inode rename coherence are
  unchanged.

# Proposal: immutable segment names inside workspace entries

## Motivation

Some subtrees inside an exposed workspace entry should stay visible but
untouchable to consumers of the mount. A common example is a generated or
curated subtree
that tools may read but must not rewrite, rename, delete, or repopulate through
the FUSE mount:

```text
workspace/
  app/
    src/
    vendor/
      locked/
```

If `vendor` is declared immutable, a consumer should still be able to `open`,
`read`, and `readdir` beneath `app/vendor` or `app/src/vendor`, but any attempt
to create, modify, rename, unlink, chmod, truncate, or otherwise mutate a
subtree rooted at any path component named `vendor` should fail.

The current codebase does not have a way to express that. The only write policy
today is entry-wide `ro` vs `rw` (`AccessMode` in `src/state.rs`, enforced from
`src/fs/resolve.rs` and `src/fs/callbacks.rs`). That is too coarse when a single
entry contains both mutable and frozen subtrees.

## Problem statement

The existing write checks are entry-scoped:

- `resolve_write_path` and `ensure_writable_entry` (`src/fs/resolve.rs`) only
  ask whether the top-level entry is `rw`.
- Mutating FUSE callbacks such as `create`, `mkdir`, `unlink`, `rmdir`,
  `rename`, `link`, `setattr`, `write`, and the write side of
  `copy_file_range` (`src/fs/callbacks.rs`) either call those helpers directly
  or implement equivalent entry-wide checks.
- Open file handles capture `writable: bool` at open time in `OpenHandle`
  (`src/fs/runtime.rs`) and later writes use that captured capability rather
  than re-evaluating live policy on every write.

That means the daemon can currently say only:

1. this entire top-level entry is writable, or
2. this entire top-level entry is read-only.

There is no representation for "deny mutation beneath any path component named
`vendor`, but not beneath `vendors`", and there is no shared subtree-policy
helper for the mutating callbacks to consult.

## Proposal

Add **immutable segment-name rules** beneath entries.

A rule names one segment string. If `foo` is frozen, then any subtree whose
path includes a component exactly equal to `foo` is immutable. For example:

- `a/foo` is immutable.
- `a/foo/x` is immutable.
- `a/bar/foo` is immutable.
- `a/bar/foo/x` is immutable.
- `a/foo2` is not made immutable by that rule.

This proposal still does not support compound immutable prefixes such as
`foo/bar`. The first implementation freezes by **segment name**, matched
anywhere in the path.

### Semantics

Immutable applies to **consumer-visible filesystem mutation through the mount**.

Reads remain allowed:

- `lookup`
- `getattr`
- `open` read-only
- `read`
- `readlink`
- `readdir`
- `statfs`

Mutations are denied when the target path contains an immutable segment name
anywhere in its relative path:

- opening a file for write or truncate
- `write`
- `create`
- `mkdir`
- `symlink`
- `link`
- `unlink`
- `rmdir`
- `rename` whenever either the source path or the destination path contains an
  immutable segment name
- all `setattr` mutations, including mode, size, and timestamp changes
- the write side of `copy_file_range`

This also applies when the matching segment would be newly created by the
operation. If `vendor` is frozen, then `mkdir app/vendor`,
`mkdir app/src/vendor`, and `rename app/tmp app/vendor` all fail even if the
target `vendor` path does not yet exist.

The error should be `EPERM`, not `EROFS`. This is a targeted policy denial on a
path subtree, not a filesystem-wide read-only mount. New immutable-segment
checks should therefore map policy denials to `EPERM` consistently, even in
callbacks that currently use `EACCES` or `EROFS` for broader read-only cases.

### V1 handle semantics

V1 deliberately preserves the current handle model: **already-open writable file
handles continue to write successfully after a path becomes immutable**.

This is consistent with the current system behavior when an entry flips from
`rw` to `ro`. `OpenHandle` stores `writable: bool` at open time, and later
`write`, `fsync`, and some `setattr` paths operate on that captured handle
rather than re-checking live policy (`src/fs/runtime.rs`,
`src/fs/callbacks.rs`). The existing `edit` proposal already depends on that
behavior.

This proposal keeps that model for the first implementation:

- new writable opens into an immutable subtree fail
- path-based mutations into an immutable subtree fail
- writes through a handle opened before the rule was added may still succeed

Stricter retroactive revocation is explicitly deferred.

### Control-plane scope

This proposal governs **mounted consumers**, not administrative entry
management.

Control-plane operations such as top-level `add`, `rm`, retarget, or mode flips
remain allowed even when an entry contains immutable subtrees. The daemon owner
must still be able to reconfigure the exported namespace.

### Storage location

Store immutable segment names at the workspace level, not as per-path prefixes.

Extend `PortalState` in `src/state.rs` with a persisted collection of immutable
segment names, for example:

```rust
pub immutable_segments: BTreeSet<String>
```

where each segment passes the same lexical validation as an entry child name:

- non-empty
- no slash
- no `.` or `..`
- no NUL

Workspace-level storage is the narrowest fit for the requested semantics:

- the rule is global by segment name: `freeze vendor` should affect every
  `vendor` subtree, not just one entry or one entry-relative prefix
- enforcement code already has the entry-relative path in hand and can scan its
  components
- the rule set does not need to move with any particular entry record because
  it is not entry-specific

### Administrative surface

Add a minimal control-plane surface for managing immutable rules directly,
instead of trying to fold them into the existing entry table editor.

Suggested CLI:

```bash
workspace-portal freeze <segment> [--workspace <path>]
workspace-portal thaw <segment> [--workspace <path>]
workspace-portal status [--json]
```

For example:

```bash
workspace-portal freeze vendor
workspace-portal thaw vendor
```

`freeze` and `thaw` should reuse workspace discovery the same way `add`, `rm`,
and `edit` do. The argument is a single segment name, not a path. It need not
already appear anywhere on the host. That allows preemptively freezing future
subtrees so later creates under any matching segment are denied.

Status output should surface immutable rules so the policy is inspectable. In
JSON that should be explicit machine-readable data, not only a human string.
Specifically:

- human `status` output should print one `IMMUTABLE SEGMENTS` line listing the
  frozen segment names in stable sorted order, or `<none>` when empty
- `status --json` should include a top-level `immutable_segments: ["foo", ...]`
  field alongside the existing workspace, daemon, socket, and entries fields

### Protocol shape

Add control requests for immutable-segment mutation and extend status payloads
to report them.

Suggested additions in `src/protocol.rs`:

- `Freeze { segment: String }`
- `Thaw { segment: String }`
- `StatusPayload` gains `immutable_segments: Vec<String>` or an equivalent
  serialized form

The daemon applies these by mutating the live `PortalState` behind the shared
`Arc<RwLock<_>>`, then persisting the state exactly as it already does for entry
add/remove.

No remount, no rebuild of `PortalFs`, and no special invalidation mechanism are
needed; the current live-state model is already sufficient.

### Enforcement shape

Introduce one shared helper in `src/fs/resolve.rs` that answers:

```rust
fn ensure_mutable_path(state: &PortalState, relative: &Path) -> Result<()>
```

It should reject when any component of `relative` matches one of the workspace's
immutable segment names.

Examples:

- immutable `foo` blocks `foo`, `foo/x`, `bar/foo`, `bar/foo/x`
- immutable `foo` does not block `foo2`
- immutable `foo` does not block `foodir`

Every mutating path should call the same helper:

- `open_path` when `writable == true`
- `resolve_parent_child_writable`
- `validate_rename` on both source and destination
- `setattr` before mode/size/time changes
- `write` only for handles opened after the rule exists; in V1 this means no
  new extra check beyond the open-time/path-time checks
- `copy_file_range` on the output path

The intent is to centralize the policy decision and keep callback-specific code
limited to mapping that decision to the right errno. For immutable-segment
denials, that errno should always be `EPERM`.

### Why not encode this as read-only entries or a generic policy engine

This proposal is narrower than either obvious alternative.

Making callers split one logical project across more top-level entries just to
freeze a repeated subtree name such as `vendor` is awkward and leaks the
daemon's storage model into the user's namespace shape.

Building a generic "path policy engine" would be over-design for the first
concrete need. The codebase needs one new policy dimension: immutable segment
names matched anywhere in entry-relative paths. That can be implemented with a
small persisted data shape and one shared enforcement helper.

## Non-goals

- Retroactively revoking writes on file handles that were already open when a
  path became immutable.
- Preventing daemon-owner control-plane mutations to an entry that contains
  immutable subtrees.
- Implementing host-side filesystem immutability (for example chattr-style
  inode flags). This is only a workspace-portal mount policy.
- Supporting path-scoped exceptions such as "freeze `vendor` everywhere except
  under `third_party`".
- Supporting nested immutable prefixes such as `foo/bar`.
- Redesigning the `edit` buffer to manage immutable rules in the first
  implementation.
- Policy frameworks beyond a flat set of immutable segment names.

## Verification

1. Unit test in `src/fs/resolve.rs` for immutable-segment matching:
   `foo` matches `foo`, `foo/x`, and `bar/foo`, but not `foo2` or `foodir`.
2. Unit test in `src/state.rs` for persisting and reloading immutable segment
   sets on `PortalState`.
3. Unit test in `src/protocol.rs` for `Freeze`/`Thaw` request JSON round-trips
   and status payload serialization of immutable segments.
4. Unit test in `src/daemon/runtime.rs` that freezing a segment updates live state
   without rebuilding the daemon or dropping open handles.
5. Ignored FUSE E2E test in `tests/fuse_e2e.rs`:
   start an `rw` entry, freeze `vendor`, and verify that `mkdir`, file create,
   unlink, rename, chmod, truncate, and fresh writable opens fail under both
   `docs/vendor` and `docs/src/vendor` while reads still succeed.
6. Ignored FUSE E2E test:
   freeze `foo`, confirm operations under `docs/foo/bar` and `docs/a/foo/bar`
   fail, and confirm `docs/foofoo` remains writable.
7. Ignored FUSE E2E test:
   open a file for write, then freeze its parent path, and confirm that the
   already-open fd can still write in V1 while a fresh write-open fails.
8. Ignored FUSE E2E test:
   freeze `vendor`, confirm `mkdir docs/vendor` and `mkdir docs/src/vendor`
   fail even when those paths do not yet exist.
9. CLI/status test:
   freeze two segment names, run `status --json`, and confirm the payload
   exposes a sorted `immutable_segments` list; run human `status` and confirm
   it prints the same names.

## Success criteria

- The daemon can persist and report a workspace-wide set of immutable segment
  names.
- Mutating filesystem operations through the mount fail with `EPERM` when the
  target path contains an immutable segment name anywhere in its relative path.
- Mutating operations that would create a new matching segment also fail with
  `EPERM`.
- Read-only operations continue to work in immutable subtrees.
- Matching is correct: freezing `foo` does not freeze `foo2`.
- Existing open writable handles continue to behave exactly as they do today in
  V1.

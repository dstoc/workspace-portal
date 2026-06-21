# Proposal: scoped hard links and hard-link audit

## Motivation

Some tools use hard links as a normal performance optimization. A concrete case
is Rust/Cargo incremental compilation, where build artifacts may be duplicated
with `link(2)` before falling back to copying. Returning `EOPNOTSUPP` for every
hard-link request keeps the portal simple, but it also makes the mount less
compatible with build tools that expect ordinary read-write filesystem
semantics under mutable paths such as `target`.

At the same time, workspace entries may mark segments such as `.jj` and `.git`
as immutable. Those segments are meant to stay readable but protected from
consumer mutation through the mount. Hard links can weaken a path-based
immutable policy if a file under an immutable segment is given another mutable
name and later written through that alias.

The portal should allow the build-tool use case without allowing new hard-link
aliases out of immutable segments.

## Problem statement

`PortalFs::link` in `src/fs/callbacks.rs` currently returns `EOPNOTSUPP`
unconditionally. That behavior was introduced by
`docs/proposals/disable-hard-links.md` after the previous implementation:

1. resolved the source inode to a portal path;
2. resolved the destination parent and name;
3. rejected cross-entry links with `EXDEV`;
4. rejected directory links with `EPERM`;
5. called a confined `safe_open::hard_link` helper in `src/fs/safe_open.rs`;
6. replied with attributes for the new name.

That broad disablement prevents a direct immutable-segment escape, but it also
prevents safe mutable-to-mutable hard links that do not involve protected paths.

The immutable-segment implementation is path-based. It rejects mutations when
the operation's relevant entry-relative path contains a frozen segment, using
`ensure_mutable_relative_path` in `src/fs/resolve.rs`. That is enough to reject
new links whose source or destination path is visibly under `.jj`, `.git`, or
another immutable segment. It does not prove that a backing inode has no other
aliases elsewhere on the host filesystem.

For this proposal, pre-existing hard links are explicitly handled as an audit
concern rather than as an enforcement concern.

## Proposal

Restore hard-link creation through the portal, but only for source and
destination paths that are both mutable according to the current path policy.

Add a separate audit command:

```bash
workspace-portal audit hardlinks <workspace>
```

The audit reports backing inodes that are visible through both immutable and
mutable portal paths. This gives users a way to find pre-existing aliases that
could undermine the path-based immutable policy, while keeping normal
mutable-to-mutable hard links available for build tools.

### Link semantics

`PortalFs::link` should allow `link(2)` only when all of the following are true:

- the source inode resolves to a normal entry path;
- the destination parent resolves to a normal entry path;
- the source and destination are in the same entry;
- the entry is writable and the mount is not globally read-only;
- the source relative path does not contain an immutable segment;
- the destination relative path does not contain an immutable segment;
- the source is not a directory;
- the destination does not already exist.

The error behavior should stay close to the previous implementation:

- portal-root links fail with `EPERM`;
- cross-entry links fail with `EXDEV`;
- directory links fail with `EPERM`;
- immutable source or destination paths fail with `EPERM`;
- backing `linkat` errors are returned using their host errno.

This keeps hard links available for normal mutable build output:

```text
docs/target/incremental/a.o -> docs/target/incremental/a.copy.o
```

and rejects direct immutable-segment escapes:

```text
docs/.git/config -> docs/target/config-alias
docs/target/config -> docs/.git/config-alias
```

### Implementation shape

Reintroduce a confined `safe_open::hard_link` helper in
`src/fs/safe_open.rs`. It should match the earlier shape: resolve both parent
directories beneath the same entry root with `open_parent`, then call
`linkat(src_parent, src_leaf, dst_parent, dst_leaf, 0)`.

Update `PortalFs::link` in `src/fs/callbacks.rs` to:

1. reject `newparent == ROOT_INO`;
2. resolve the source inode with `runtime.path_for_inode`;
3. convert that source portal path with `state_for_path`;
4. call `ensure_mutable_relative_path` for the source relative path;
5. resolve the destination with `runtime.resolve_parent_child_writable`;
6. reject if source and destination entries differ;
7. reject source directories with `EPERM`;
8. call `safe_open::hard_link`;
9. stat the destination and reply with a remembered lookup.

The implementation should keep the current immutable-segment helper as the
policy boundary. It should not add inode-wide mutation checks in the FUSE write
path as part of this change.

### Audit command

Add an `Audit` top-level command in `src/cli.rs` with a nested `Hardlinks`
subcommand:

```rust
Commands::Audit(AuditCommand)
AuditCommands::Hardlinks(HardlinkAuditCommand)
```

The user-facing syntax is:

```bash
workspace-portal audit hardlinks <workspace>
```

The command should load the workspace state in the same style as `status`: use
the live daemon status when the socket is live, otherwise fall back to persisted
state. It does not need to mutate daemon state and should not require the
workspace to be stopped.

The audit scans the backing targets for all configured entries. It should use
`symlink_metadata`/`lstat` semantics and must not follow symlinked directories.
For every non-directory path with `nlink > 1`, group visible paths by backing
`(dev, ino)`.

A group is reported when it contains:

- at least one path whose entry-relative path contains an immutable segment; and
- at least one path whose entry-relative path does not contain an immutable
  segment.

The report should include the entry name and entry-relative path for every
visible alias in the group, separated into immutable and mutable aliases. If a
backing inode has additional aliases outside configured entries, the command is
not required to find those aliases. It may note that `nlink` is larger than the
number of visible aliases.

Suggested human output:

```text
hardlink aliases crossing immutable boundaries:

inode dev=64768 ino=123456 nlink=2
  immutable:
    docs:.git/config
  mutable:
    docs:target/config-alias
```

Suggested exit status:

- `0` when no crossing aliases are found;
- non-zero when any crossing aliases are found;
- non-zero for scan errors that prevent a reliable result.

JSON output can be added later if another command needs machine-readable audit
data. The initial command should keep the surface area narrow.

### Documentation

Update `docs/workspace-portal.md` to say that read-write entries support
same-entry hard links except when either endpoint is under an immutable segment.

Update the hard-link proposal history by leaving
`docs/proposals/disable-hard-links.md` intact and adding this proposal as the
newer policy. The earlier document remains useful context for why hard links
need explicit handling.

## Non-goals

- Detecting or blocking pre-existing hard links during normal writes.
- Preventing host-side hard links created outside the portal.
- Finding aliases that are not reachable through any configured entry.
- Supporting cross-entry hard-link creation through the FUSE mount.
- Rewriting a failed hard-link request into a byte copy.
- Adding JSON output to `audit hardlinks` in the first implementation.
- Generalizing `audit` beyond the first `hardlinks` subcommand.

## Verification

1. Add or update a FUSE E2E test so `fs::hard_link` succeeds from one mutable
   path to another mutable path in the same entry.
2. In the same test, write through the new link and verify the original path and
   backing target reflect normal hard-link semantics.
3. Add FUSE E2E coverage where `.git` or `.jj` is an immutable segment and
   hard-link creation fails with `EPERM` when the source is immutable.
4. Add FUSE E2E coverage where hard-link creation fails with `EPERM` when the
   destination is immutable.
5. Keep a copy assertion proving `fs::copy` and `copy_file_range` behavior is
   unchanged.
6. Add control-plane tests for `workspace-portal audit hardlinks <workspace>`
   against a temporary stopped workspace state with one crossing alias group.
7. Add a test where `audit hardlinks` ignores ordinary mutable-to-mutable hard
   links.
8. Run `cargo test`.
9. Run the ignored FUSE suite with `./scripts/fuse-e2e-podman.sh` or
   `./scripts/fuse-e2e.sh`.

## Success criteria

- Mutable-to-mutable hard links in the same entry succeed through the mount.
- Hard links from immutable source paths fail with `EPERM`.
- Hard links to immutable destination paths fail with `EPERM`.
- Cross-entry hard links still fail with `EXDEV`.
- Cargo/rustc-style hard-link duplication can use `target` while `.jj` and
  `.git` remain path-protected.
- `workspace-portal audit hardlinks <workspace>` reports visible hard-link
  groups that cross immutable and mutable paths.
- Pre-existing hard links remain out of the runtime enforcement path.

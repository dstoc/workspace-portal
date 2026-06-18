# Proposal: disable hard links through the portal mount

## Motivation

Hard links make two paths inside an entry refer to the same inode. That behavior
is normal on a local filesystem, but it is awkward for a controlled portal: a
write to one path silently mutates the contents visible at another path, and
path-based reasoning about what changed becomes less direct.

The portal already exposes a copy path for tools that need to duplicate file
contents. Disabling hard-link creation keeps the mounted workspace's mutation
model simpler: new names are created as new filesystem objects, not as aliases
to existing ones.

The practical use case is a tool that tries hard-link duplication as an
optimization. Through the portal, that optimization should decline cleanly so
the tool can fall back to copying bytes.

## Problem statement

`PortalFs` currently implements the FUSE `link` operation in
`src/fs/callbacks.rs:1383`. The callback:

1. rejects links at the portal root;
2. resolves the source inode and destination parent;
3. rejects cross-entry links with `EXDEV`;
4. rejects directory hard links with `EPERM`;
5. calls `safe_open::hard_link`;
6. stats the new name and returns a normal entry reply.

`safe_open::hard_link` (`src/fs/safe_open.rs:286`) performs the host operation
with `linkat` after resolving both parent directories beneath the entry root.
That keeps the operation confined, but it still creates a real hard link in the
backing directory.

The current behavior is covered by
`fuse_e2e_hard_link_and_copy_cover_rustc_style_file_duplication`
(`tests/fuse_e2e.rs:795`), which verifies that:

- `fs::hard_link` succeeds through the mount;
- writing through the new path mutates the original path;
- `fs::copy` also works.

After this change, the hard-link part should fail explicitly, while the copy
part should remain supported.

## Proposal

Make hard-link creation unsupported through the portal mount by changing
`PortalFs::link` to return `EOPNOTSUPP` immediately:

```rust
fn link(
    &self,
    _req: &Request,
    _ino: INodeNo,
    _newparent: INodeNo,
    _newname: &std::ffi::OsStr,
    reply: ReplyEntry,
) {
    reply.error(Errno::EOPNOTSUPP);
}
```

Keep the callback implemented rather than deleting it. The explicit errno makes
the policy clear and avoids depending on fuser's default `ENOSYS` behavior.
`EOPNOTSUPP` communicates "this filesystem does not support this operation"
better than permission errors, because the failure applies even in read-write
entries with writable directories.

### Implementation shape

- Replace the body of `fn link` in `src/fs/callbacks.rs` with the immediate
  `EOPNOTSUPP` reply.
- Remove `safe_open::hard_link` if it has no remaining callers, or leave it only
  if a near-term test or future proposal still needs it. The preferred final
  state is no dead helper for a disabled operation.
- Update documentation in `docs/workspace-portal.md` so read-write entries no
  longer list hard-link creation as a supported operation.
- Update any security or capability summary that says hard links are supported
  through the portal.

### Interaction with copy behavior

Do not change `copy_file_range` (`src/fs/callbacks.rs:1651`) or
`copy_file_range_fallback` (`src/fs/callbacks.rs:157`). Copying produces an
independent file and remains the compatibility path for tools that attempt hard
links first.

## Non-goals

- Disabling hard links that already exist in the backing directory. Existing
  aliases are a property of the host filesystem and remain visible as ordinary
  files through the portal.
- Rewriting a failed hard-link request into a copy. The kernel operation is
  `link(2)`, and pretending it succeeded with a copy would change POSIX
  semantics in a surprising way.
- Adding per-entry hard-link policy. The proposal disables the operation for
  the whole portal mount.
- Changing rename, copy, clone, or `copy_file_range` behavior.
- Adding deeper inode alias tracking for existing hard links.

## Verification

1. Add or update an ignored FUSE E2E test so `fs::hard_link` from one path in a
   read-write entry to another path in the same entry fails with an unsupported
   operation errno.
2. In the same test, verify the destination path was not created in the mounted
   workspace or on the backing host directory.
3. Keep a copy assertion in that test, or split it into a separate
   copy-focused test, to prove `fs::copy` still duplicates contents through the
   portal.
4. Run `cargo test`.
5. Run the FUSE suite with `./scripts/fuse-e2e-podman.sh` or
   `./scripts/fuse-e2e.sh`.

## Success criteria

- `link(2)` through the portal mount always fails with `EOPNOTSUPP`.
- No backing-store hard link is created by a failed portal `link` request.
- Existing hard links in the backing store remain readable as normal files.
- `fs::copy` and `copy_file_range` behavior is unchanged.
- No unused `safe_open::hard_link` helper remains after implementation.

# Proposal: editable `readlink` symlink policy

## Motivation

`workspace-portal` currently exposes symlinks in entries as normal symlink
inodes. They are visible through `lookup`, `getattr`, and `readdir`, readable
with `readlink`, and, unless the workspace was started with `--nosymfollow`,
traversable through the portal mount.

That default is useful for compatibility, but some workspaces need a portable
way to prevent symlink traversal through the portal. `--nosymfollow` provides
that behavior when the kernel and FUSE mount path support the option, but it is
not available in every environment. The daemon needs a FUSE-layer fallback that
can deny link resolution without relying on a mount option.

The FUSE operation that enables kernel traversal is `readlink`: when the kernel
walks through a symlink in the portal, it asks the daemon for the link target.
If the daemon returns `ELOOP`, traversal fails. That also means explicit
`readlink` calls cannot inspect the target text while the policy is disabled.

The narrow change is a workspace-wide editable config option:

```toml
readlink = true
```

The default remains `true`, preserving current behavior. Setting
`readlink = false` makes the FUSE `readlink` callback return `ELOOP`.

## Problem statement

The current FUSE callback always attempts to read the backing symlink target:

```rust
fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
    // resolve inode to PortalPath ...
    match safe_open::readlink(&resolved.entry.target, &resolved.relative) {
        Ok(target) => reply.data(target.as_os_str().as_bytes()),
        Err(err) => reply.error(Errno::from_i32(err.raw_os_error().unwrap_or(libc::EIO))),
    }
}
```

That lives in `src/fs/callbacks.rs`, with host-path confinement delegated to
`safe_open::readlink` in `src/fs/safe_open.rs`.

There is no live workspace policy that can decline `readlink` before the host
operation is attempted, so there is no daemon-side way to prevent symlink
traversal in environments where `nosymfollow` cannot be used:

- `PortalState` (`src/state.rs`) stores entries, immutable segment policy, and
  read-only defaults, but no symlink read policy.
- `WorkspaceSnapshot` exposes `immutable_segments` to `workspace-portal edit`,
  but no `readlink` setting.
- `EditableConfig` (`src/daemon/edit_config.rs`) renders top-level
  `immutable_segments` and `entries`, and rejects unknown fields.
- The control protocol (`src/protocol.rs`) has `Freeze` and `Thaw` for the only
  current live workspace-wide policy, but no request for changing a boolean
  policy setting.

The existing `--nosymfollow` option is intentionally start-time only. It is
threaded through `StartCommand`, `StartArgs`, `DaemonConfig`, and
`PortalFs::mount` (`src/cli.rs`, `src/daemon.rs`, `src/daemon/runtime.rs`,
`src/fs.rs`). It cannot be the only traversal-control mechanism because some
supported environments reject or ignore the mount option before the daemon can
serve the workspace.

## Proposal

Add a workspace-wide `readlink` boolean to the editable TOML config. The option
defaults to `true`.

```toml
version = 1
readlink = true
immutable_segments = []

[entries.docs]
target = "/home/user/project/docs"
mode = "rw"
```

When `readlink` is `true`, behavior is unchanged:

- symlink inodes remain visible;
- `readlink` returns the stored link target;
- symlink traversal follows the existing mount behavior, including
  `--nosymfollow` when the workspace was started with that flag.

When `readlink` is `false`, behavior is:

- symlink inodes remain visible in `lookup`, `getattr`, `readdir`, and
  `symlink_metadata`;
- the FUSE `readlink` callback returns `ELOOP` without calling
  `safe_open::readlink`;
- symlink traversal through the portal also fails because the kernel cannot
  obtain the link target from the daemon;
- symlink creation in writable entries remains allowed unless another policy
  rejects the write.

This is a mount-wide workspace policy, not a per-entry policy.

### Naming

Use `readlink`, not `symlinks`, `follow_symlinks`, or `allow_symlinks`.

`readlink` names the exact FUSE operation being controlled. It is also precise
about the tradeoff: this daemon-side traversal control works by denying the
kernel access to symlink target text, so explicit `readlink` calls are denied
too.

### State and protocol

Store the setting in `PortalState`:

```rust
#[serde(default = "default_readlink")]
pub readlink: bool,
```

`default_readlink()` returns `true` so existing `portal.json` files preserve
current behavior after deserialization.

Surface the field through `WorkspaceSnapshot` and status JSON so the current
policy is inspectable:

```rust
#[serde(default = "default_readlink")]
pub readlink: bool,
```

Add a control request for changing the live value:

```rust
SetReadlink {
    enabled: bool,
}
```

`Daemon::handle_request` should update `PortalState::readlink`, bump the state
generation when the value changes, persist state, and return an acknowledgement.
The operation is idempotent: setting the current value again succeeds and does
not need to bump generation.

This mirrors the current live-editable policy path for immutable segments
without introducing a generic policy engine.

### Editable config

Add `readlink` to `EditableConfig`:

```rust
#[serde(default = "default_readlink")]
pub readlink: bool,
```

The renderer should always emit it explicitly, immediately after `version`:

```toml
version = 1
readlink = true
immutable_segments = []
```

Missing `readlink` falls back to `true`. That fallback allows older edit buffers
or hand-written minimal configs to keep existing behavior, while generated
buffers remain complete and reviewable.

`plan_edit` should compare `before.readlink` and `after.readlink`; when they
differ, include one `ControlRequest::SetReadlink { enabled: after.readlink }`.
The request should be planned after entry changes and before immutable segment
changes, or at another fixed point in the existing request list. The exact
position does not affect correctness, but keeping it deterministic makes tests
and error reporting simpler.

Because `EditableConfig` already uses `#[serde(deny_unknown_fields)]`, misspelled
forms such as `read_link = false` should fail loudly.

### FUSE callback behavior

Check the policy at the start of `readlink` in `src/fs/callbacks.rs`, after
rejecting `ROOT_INO` and before resolving or reading the backing path:

```rust
let state = self.state.read().unwrap().clone();
if !state.readlink {
    reply.error(Errno::ELOOP);
    return;
}
```

Returning `ELOOP` is intentional. It matches the common failure class for
refused symlink resolution and makes kernel path walking fail at the symlink
instead of allowing traversal to continue to the target. It also avoids
reporting the failure as a permissions error on the target file, which the
daemon has deliberately not resolved.

The callback should still return `EINVAL` for `ROOT_INO`. The policy only
applies to real symlink read attempts.

### Interaction with `--nosymfollow`

`readlink` and `--nosymfollow` are independent traversal controls:

- default start, `readlink = true`: symlinks are readable and traversable as
  today;
- default start, `readlink = false`: symlinks are visible, but `readlink` and
  traversal fail with `ELOOP`;
- `--nosymfollow`, `readlink = true`: symlinks are visible and readable, but
  traversal is blocked by the mount option;
- `--nosymfollow`, `readlink = false`: symlinks are visible, `readlink` fails
  with `ELOOP`, and traversal is blocked.

No mount remount is required when `readlink` changes. It is evaluated by the
daemon callback at operation time, which is the main advantage over relying only
on the environment-dependent `nosymfollow` mount option.

### Status and documentation

Human status output (`src/daemon/output.rs`) should include the setting near the
other workspace-wide policy fields, for example:

```text
READLINK: true
IMMUTABLE SEGMENTS: <none>
```

Status JSON should include `readlink` through `WorkspaceSnapshot`.

Update `README.md`, `docs/workspace-portal.md`, and `docs/security.md` to
describe the three separate symlink concerns:

- visibility of symlink inodes remains unchanged;
- `readlink = false` blocks traversal at the FUSE `readlink` operation and, as
  a consequence, also blocks disclosure of symlink target text;
- `--nosymfollow` blocks traversal at the mount layer where supported while
  still allowing target inspection when `readlink = true`.

## Non-goals

- Adding a startup CLI flag. This proposal exposes the policy only through
  `workspace-portal edit`.
- Making `readlink = false` the default. Existing workspaces keep current
  symlink compatibility unless explicitly edited.
- Adding per-entry symlink read policy to `EntryRecord`.
- Hiding symlink inodes from directory listings or changing symlink attributes.
- Disabling symlink creation. Writable entries may still create symlinks; the
  policy controls whether their target text can be read through the portal.
- Changing daemon-side host-path confinement in `safe_open`.
- Removing or changing `--nosymfollow`.

## Verification

1. Add unit tests for `PortalState` deserialization showing missing `readlink`
   defaults to `true`.
2. Add protocol tests showing `SetReadlink { enabled: false }` round-trips
   through JSON.
3. Add `EditableConfig` tests showing:
   - rendered buffers include `readlink = true`;
   - missing `readlink` parses as `true`;
   - `readlink = false` parses correctly;
   - `plan_edit` emits `SetReadlink` only when the value changes;
   - unknown spellings are rejected.
4. Add daemon runtime tests showing `SetReadlink` persists the new value and is
   reflected in subsequent status snapshots.
5. Add a focused callback or FUSE E2E test for `readlink = false`:
   - create a symlink in an entry;
   - verify `symlink_metadata` still reports a symlink;
   - verify `read_link` fails with raw OS error `ELOOP`;
   - verify opening/traversing through the symlink fails.
6. Keep `fuse_e2e_symlinks_cover_traversal_and_broken_targets` as the default
   compatibility test for `readlink = true`.
7. Keep `fuse_e2e_nosymfollow_keeps_symlinks_visible_but_blocks_traversal` as
   the `--nosymfollow` interaction test, and add or extend coverage for the
   combined `--nosymfollow` plus `readlink = false` case if the FUSE test suite
   can express the edit setup cleanly.
8. Run `cargo test`.
9. Run the FUSE suite with `./scripts/fuse-e2e-podman.sh` or
   `./scripts/fuse-e2e.sh`.

## Success criteria

- Generated edit buffers include `readlink = true` by default.
- Existing state files without a `readlink` field preserve current behavior.
- Editing `readlink = false` updates live daemon state without remounting.
- With `readlink = false`, symlink inodes remain visible but `readlink` returns
  `ELOOP`.
- With `readlink = true`, current symlink read behavior is unchanged.
- `--nosymfollow` remains an independent traversal policy.

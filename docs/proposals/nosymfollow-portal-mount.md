# Proposal: add an optional `nosymfollow` portal mount flag

## Motivation

`workspace-portal` exposes host directories through one FUSE mount. Symlinks in
an entry are currently visible, readable with `readlink`, and traversable through
the mount. That compatibility matters: repositories and package trees often use
symlinks for normal workflows, and existing users should not have that behavior
changed by default.

Some workspaces want a stricter boundary. In those cases, symlinks should remain
observable metadata, but a process should not be able to open paths through a
symlink in the portal. The kernel `nosymfollow` mount option provides exactly
that mount-wide policy: `readlink` still works, while path traversal through
symlink components is rejected during the path walk.

The narrowest change is an opt-in start flag that adds `nosymfollow` to the
portal mount options. The default remains today's behavior.

## Problem statement

`PortalFs::mount` (`src/fs.rs:125`) builds the FUSE mount configuration:

```rust
let mut config = FuserConfig::default();
config
    .mount_options
    .push(MountOption::FSName("workspace-portal".to_owned()));
if allow_other {
    config.acl = SessionACL::All;
    config.mount_options.push(MountOption::DefaultPermissions);
}
```

There is no way for `workspace-portal start` to request `nosymfollow`, and no
field carries such a choice through the daemon start path:

- `StartCommand` in `src/cli.rs:39` has flags for `--allow-other`,
  `--read-only`, backgrounding, state/socket paths, and logging, but no symlink
  traversal flag.
- `StartArgs` in `src/daemon.rs:36` carries those parsed values into
  `daemon::start`.
- `DaemonConfig` in `src/daemon/runtime.rs:33` carries mount-time daemon
  options, currently including `allow_other`.
- `Daemon::mount_workspace` (`src/daemon/runtime.rs:244`) calls
  `PortalFs::new(...).mount(&workspace, allow_other)`.

The daemon also intentionally allows in-entry symlink traversal in its confined
host resolver: `safe_open::openat2_beneath` (`src/fs/safe_open.rs:75`) uses
`RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS`, and `safe_open::open_file` documents
that symlinks are followed while they stay beneath the entry root
(`src/fs/safe_open.rs:134`).

The current behavior is expressed by
`fuse_e2e_symlinks_cover_traversal_and_broken_targets` (`tests/fuse_e2e.rs:365`):

- `symlink_metadata` observes the symlink itself;
- `readlink` returns the link target;
- `fs::read_to_string` through `docs/shortcut.txt` and
  `docs/linked-dir/payload.txt` succeeds.

That test should remain correct for the default mount. A separate
`--nosymfollow` test should assert the stricter behavior.

## Proposal

Add an opt-in `workspace-portal start --nosymfollow` flag. When present, the
portal mount includes the custom FUSE mount option:

```rust
if nosymfollow {
    config
        .mount_options
        .push(MountOption::CUSTOM("nosymfollow".to_owned()));
}
```

`fuser` 0.17 does not expose a typed `MountOption::NoSymFollow`, but it does
provide `MountOption::CUSTOM(String)` for unsupported option names, and
`option_to_string` passes the custom value through unchanged. This avoids
adding a project-level mount-option abstraction for one option.

### CLI and propagation

Add a boolean flag to `StartCommand`:

```rust
#[arg(long, help = "Disable symlink traversal through the portal mount")]
pub nosymfollow: bool,
```

Thread it through the existing start path:

- add `nosymfollow: bool` to `StartArgs`;
- set it from `cmd.nosymfollow` in `cli::run`;
- add `nosymfollow: bool` to `DaemonConfig`;
- pass it from `daemon::start` into `Daemon::new`;
- pass it from `Daemon::mount_workspace` into `PortalFs::mount`;
- extend `PortalFs::mount` to accept `nosymfollow` and conditionally push
  `MountOption::CUSTOM("nosymfollow".to_owned())`.

`spawn_background_daemon` (`src/daemon/background.rs:104`) must also pass
`--nosymfollow` to the daemon child when the parent was started with the flag,
the same way it forwards `--allow-other`, `--read-only`, `--adopt`, and
`--force`.

### Semantics

Without `--nosymfollow`, behavior is unchanged:

- symlink inodes remain visible;
- `readlink` returns the stored link text;
- symlink traversal through the portal continues to work where it works today.

With `--nosymfollow`, the policy applies to the whole portal mount:

- symlink inodes remain visible in `lookup`, `getattr`, `readdir`, and
  `symlink_metadata`;
- `readlink` continues to return the stored link text;
- `symlink(2)` creation in writable entries remains allowed by the daemon unless
  a separate policy disables it;
- path traversal through a symlink fails before the daemon opens the target path.

This is not a per-entry policy. Every entry in the workspace has the same
symlink traversal behavior because there is one FUSE mount for the portal.

### State and restart behavior

Do not store `nosymfollow` in `PortalState` or `EntryRecord`. It is a start-time
mount option, like `allow_other`, not an entry attribute. A stopped workspace can
be restarted with or without `--nosymfollow`, and the new mount uses the flag
from that start invocation.

There is no state migration.

### Failure behavior

If `--nosymfollow` is requested and the kernel or FUSE mount path rejects the
option, `workspace-portal start` should fail rather than silently mounting with
symlink traversal enabled. The error should surface through the existing
`spawn_mount2` error path; adding a more specific CLI message is useful but not
required for the first implementation.

Without `--nosymfollow`, the option is not passed and startup behavior remains
unchanged.

### Documentation updates

Update `README.md`, `docs/workspace-portal.md`, and `docs/security.md` to
describe the new start flag and the two modes:

- default mode: symlinks are visible, readable, and traversable as today;
- `--nosymfollow`: symlinks are visible and readable with `readlink`, but
  traversal through symlinks is disabled by the mount;
- daemon-side `safe_open` confinement remains a separate host-path protection.

## Non-goals

- Making `nosymfollow` the default. This proposal preserves current symlink
  traversal behavior unless the new flag is explicitly used.
- Adding per-entry symlink policy to `EntryRecord` or the control protocol.
  There is one mount, and `nosymfollow` is a mount-wide setting.
- Adding a `--no-nosymfollow` flag. The option is off by default, so an explicit
  negative flag is not needed.
- Changing `safe_open` from `RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS` to
  `RESOLVE_NO_SYMLINKS`. That would alter daemon-side host resolution and needs
  its own analysis. This proposal only changes consumer path traversal through
  the portal mount when the flag is enabled.
- Hiding symlink inodes or rewriting symlink target text.
- Disabling symlink creation. A writable entry may still create symlinks; under
  `--nosymfollow`, they just cannot be traversed through the mounted portal.

## Verification

1. Confirm `workspace-portal start --help` lists `--nosymfollow` with a clear
   description.
2. Add a focused unit test or config-level test that constructs the mount config
   path and verifies `MountOption::CUSTOM("nosymfollow".to_owned())` is absent
   by default and present when the flag is enabled.
3. Add a background-spawn test or command-construction check that verifies
   `spawn_background_daemon` forwards `--nosymfollow` to the daemon child.
4. Keep `fuse_e2e_symlinks_cover_traversal_and_broken_targets` as the default
   behavior test: traversal through in-entry symlinks should still succeed
   without the flag.
5. Add a new ignored E2E test that starts the workspace with `--nosymfollow`:
   - `symlink_metadata` observes the symlink itself;
   - `read_link` returns the target bytes;
   - traversal through `shortcut.txt` and `linked-dir/payload.txt` fails from
     the kernel's no-symlink-follow policy.
6. Confirm symlink creation still succeeds in `fuse_e2e_symlink_creation`; when
   the workspace is started with `--nosymfollow`, add a traversal failure
   assertion for the newly-created link.
7. Run the normal non-FUSE test suite with `cargo test`.
8. Run the FUSE suite with `./scripts/fuse-e2e-podman.sh` or
   `./scripts/fuse-e2e.sh` on a host where mounting with `nosymfollow` is
   supported.
9. Manually inspect `/proc/self/mountinfo` or `findmnt -no OPTIONS <workspace>`
   for live workspaces and confirm `nosymfollow` appears only when the flag was
   used.

## Success criteria

- `workspace-portal start --nosymfollow` mounts the portal with the
  `nosymfollow` option.
- Starting without `--nosymfollow` preserves existing symlink traversal
  behavior.
- Background daemon startup preserves the flag.
- Under `--nosymfollow`, symlinks remain visible and readable with `readlink`.
- Under `--nosymfollow`, opening or traversing a symlink through the portal
  mount fails.
- Existing non-symlink read/write/create/rename/copy flows continue to pass in
  both modes.

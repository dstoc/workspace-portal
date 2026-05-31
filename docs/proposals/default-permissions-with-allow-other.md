# Proposal: enable `default_permissions` when the mount uses `allow_other`

## Motivation

A workspace-portal entry maps to a host directory, and the daemon performs every
file operation with the **daemon's own uid** — there is no privilege drop and the
FUSE request's caller uid is never consulted. With the default mount (owner-only
access) that is fine: the only process that can reach the mount is the one that
owns it.

`--allow-other` changes that. It opens the mount to *other local users*, but the
daemon still opens, reads, and writes files as its own uid, and the kernel is not
told to check the displayed permissions against the accessing user. The result is
a classic FUSE confused-deputy: a lower-privileged user can reach files *through
the mount* that they could not open directly, because the daemon does the I/O
with its privileges and nothing enforces the file's own mode/owner against the
caller.

The fix is the standard one: when `allow_other` is in effect, also pass the
`default_permissions` mount option so the kernel enforces the returned
mode/uid/gid against the accessing process.

## Problem statement

`PortalFs::mount` (`src/fs.rs:125`) builds the FUSE config:

```rust
let mut config = FuserConfig::default();
config.mount_options.push(MountOption::FSName("workspace-portal".to_owned()));
if allow_other {
    config.acl = SessionACL::All;
}
```

So `allow_other` widens *who may access* the mount (`SessionACL::All`) but adds
no permission gate. There is no `MountOption::DefaultPermissions`, and `PortalFs`
implements **no `access` callback** (confirmed: no `fn access` in
`src/fs/callbacks.rs`), so the FUSE default applies and the kernel performs no
per-file permission check. Authorization therefore reduces to whatever the real
`open(2)`/`*at` calls allow — and those run as the daemon uid, not the caller.

The attributes the daemon returns already carry the *real* host identity and
mode: `attr_from_metadata` (`src/fs/attr.rs:35`) sets `uid`/`gid` from the host
`metadata` (`:68`–`:69`) and `perm` from the file's real mode (`:42`–`:46`). So
the kernel has accurate data to enforce against — it just isn't asked to.

Concrete exposure, daemon running as user `app`, mount started with
`--allow-other`, an entry exposing `/srv/app` which contains
`secret.txt` mode `0600` owned by `app`:

```text
# as another local user `bob`:
cat /workspace/data/secret.txt      # succeeds — daemon opens it as `app`
                                    # `bob` could not `cat /srv/app/secret.txt`
```

Writes are similarly under-checked (subject only to the entry's ro/rw flag, not
the file's mode/owner).

This gap predates the path-confinement work and is orthogonal to it:
confinement keeps the daemon *inside the entry*; this is about *which caller* may
use the daemon to act inside the entry.

## Proposal

When `allow_other` is enabled, also push `MountOption::DefaultPermissions`:

```rust
if allow_other {
    config.acl = SessionACL::All;
    config.mount_options.push(MountOption::DefaultPermissions);
}
```

`default_permissions` makes the kernel run a standard permission check
(`mode`/`uid`/`gid` vs. the accessing process's credentials) on each access,
using the attributes the daemon already returns. A file the caller could not open
directly is then denied at the kernel before reaching the daemon, closing the
confused-deputy path while leaving legitimate owner access unchanged.

### Why scope it to `allow_other`

The exposure exists only when non-owners can reach the mount. Without
`allow_other` the kernel already restricts the mount to the mounting user, so the
check is redundant there, and always-on `default_permissions` would add kernel
permission gating to the single-user path where it has historically been absent —
a behavior change with no security benefit for that case. Tying it to
`allow_other` is the narrowest change that closes the real gap.

### Interaction with existing checks

`default_permissions` is an *additional* gate, not a replacement:

- Read-only entries still reject writes in the daemon (`EROFS`/`EPERM` via
  `ensure_writable_entry` / `read_only_default`); the kernel check runs first on
  the file's own mode but the daemon's policy remains authoritative for ro
  entries.
- Synthesized directory permissions (`0o555` ro / `0o755` rw in
  `attr_from_metadata`) remain consistent with this: a read-only entry's
  directories present as non-writable to everyone, which the kernel will now also
  enforce.

No attribute changes are required — the daemon already returns real `uid`/`gid`
and file mode.

## Non-goals

- Privilege-dropping / `setfsuid` to the caller per request. That is a larger
  change to the I/O model; `default_permissions` achieves the needed access
  control without it.
- Implementing a custom `access` callback. `default_permissions` covers the
  standard check; a bespoke `access` is only worthwhile if non-POSIX policy is
  ever needed.
- Enabling `default_permissions` unconditionally (for owner-only mounts). Left as
  a possible future change; out of scope here because it alters behavior without
  closing a present exposure.
- Any change to path confinement (`docs/proposals/symlink-confinement.md`), which
  is independent.

## Verification

1. Start a workspace with `--allow-other`, exposing a directory that contains a
   file mode `0600` owned by the daemon's uid.
2. As a *different* local user, attempt to read that file through the mount;
   confirm it now fails with `EACCES` (previously succeeded).
3. As the owner, confirm the same read still succeeds and normal read/write/
   create/rename flows are unaffected.
4. Start a workspace **without** `--allow-other`; confirm behavior is unchanged
   (owner access works; the mount remains inaccessible to other users as before).
5. Confirm the existing FUSE E2E suite still passes under both mount modes.

A new ignored E2E test (e.g. `fuse_e2e_allow_other_enforces_file_permissions`)
can cover steps 1–3 where the harness can run as, or drop to, a second uid;
where it cannot, this is verified manually and the unit-level change is the
one-line option push.

## Success criteria

- With `--allow-other`, a file a caller cannot open directly is not readable or
  writable through the mount (kernel returns `EACCES`).
- Owner access and all existing flows are unchanged with and without
  `--allow-other`.
- `default_permissions` is present in the mount options exactly when
  `allow_other` is set.

# Workspace Portal

## Summary

`workspace-portal` is a Rust CLI and daemon that exposes selected host
directories through a single stable FUSE-mounted workspace.

Typical workflow:

```bash
workspace-portal start ./workspace --bg
workspace-portal edit ./workspace
workspace-portal status
cd workspace
ls
```

Inside the mounted workspace, users see stable top-level entries backed by real
host paths:

```text
workspace/
  project-a/
  notes/
```

The portal is not a symlink switchboard. The daemon resolves and serves host
files through FUSE.

## Current implementation

The current codebase implements:

- CLI commands:
  - `start`
  - `edit`
  - `status`
  - `stop`
  - `list`
  - `check`
  - `forget`
  - `audit hardlinks`
- a long-running control daemon over a Unix domain socket
- persisted workspace state
- workspace discovery by walking upward and checking registry state
- a mounted FUSE filesystem with real file and directory operations
- per-entry read-write and read-only mappings
- ignored FUSE E2E tests plus a Podman runner

The implementation is intentionally MVP-scoped. It is suitable for local
development workflows, but it does not attempt full POSIX-perfect behavior.

## Core concepts

### Workspace

A workspace is a directory that becomes the FUSE mountpoint managed by
`workspace-portal`.

Example:

```bash
workspace-portal start ./workspace
```

The workspace directory itself stays empty before entries are added. The daemon
does not write marker files into the mountpoint.

### Entry

An entry is a top-level name inside the workspace mapped to one host target.

Example:

```bash
workspace-portal edit ./workspace
```

This creates:

```text
./workspace/project-a
```

backed by:

```text
/home/user/code/project-a
```

### Target

A target is the canonical host directory served by the daemon.

Current behavior:

- the target must exist
- the target must be a directory
- the target is canonicalized when the edited config is applied
- the top-level entry name must be a single path component

## Command behavior

### `start`

```bash
workspace-portal start <workspace> [--bg] [--read-only] [--nosymfollow]
```

Current behavior:

- creates the workspace directory if missing
- refuses to start on a non-empty directory unless `--adopt` or `--force` is used
- mounts the FUSE filesystem at the workspace path
- persists workspace state under the XDG state root
- starts in the foreground by default
- supports `--bg` for background daemonization
- supports `--nosymfollow` for mount-wide symlink traversal control

Supported options:

```text
--bg
--socket <path>
--state-dir <path>
--allow-other
--no-allow-other
--read-only
--nosymfollow
--adopt
--force
--log-level <level>
```

Notes:

- `--allow-other` is passed through to the FUSE mount configuration
- `--nosymfollow` applies to the whole mount, not an individual entry; it is
  not stored in entry state or persisted as part of the workspace registry
- workspace discovery and restart behavior now rely on persisted registry state,
  not a marker file inside the workspace

### `edit`

```bash
workspace-portal edit [<workspace>]
```

Current behavior:

- opens the desired state as a TOML buffer with `version = 1`, `readlink = true`,
  `immutable_segments = [...]`, and `[entries.<name>]` tables
- editing that buffer can add, remove, rename, retarget, or flip entries between
  `ro` and `rw`
- the same buffer can also manage the workspace `readlink` policy and immutable
  segments
- read-write entries support same-entry hard links unless either endpoint is
  under an immutable segment

Notes:

- parse or validation errors reopen the editor with a commented error at the top
- unchanged buffers apply nothing
- flipping or removing an entry does not revoke already-open file handles

### `status`

```bash
workspace-portal status [<workspace>] [--json]
```

Current behavior:

- reports workspace path
- reports mounted state
- reports daemon state
- reports socket path
- lists current entries and their modes

JSON payload shape:

```json
{
  "workspace": "/home/user/work/current/workspace",
  "mounted": true,
  "daemon": "running",
  "socket": "/run/user/1000/workspace-portal/7f3a.sock",
  "readlink": true,
  "immutable_segments": [],
  "entries": [
    {"name": "project-a", "mode": "rw", "target": "/home/user/code/project-a"},
    {"name": "notes", "mode": "ro", "target": "/home/user/notes/current"}
  ]
}
```

### `stop`

```bash
workspace-portal stop [<workspace>] [--lazy] [--force]
```

Current behavior:

- asks the daemon to stop and unmount
- supports lazy unmount fallback
- is idempotent when the daemon socket is already gone and the workspace is
  already unmounted

### `list`

```bash
workspace-portal list
```

Current behavior:

- lists known workspaces for the current user from persisted registry state
- includes status and entry counts

### `check`

```bash
workspace-portal check [<workspace>]
```

Current behavior:

- checks `/dev/fuse`
- checks `fusermount3`
- reports workspace mount state when a workspace is supplied or discoverable
- reports socket/state visibility

### `forget`

```bash
workspace-portal forget <workspace>
```

Current behavior:

- refuses to run while the workspace daemon is reachable or the workspace is
  mounted
- removes stored state, registry, stale socket, and log metadata for the
  workspace
- does not remove the workspace directory or any entry targets

### `audit`

```bash
workspace-portal audit hardlinks <workspace>
```

Current behavior:

- scans the workspace targets for visible hard-link groups that cross immutable
  and mutable portal paths
- prints a no-findings message and exits with status `0` when nothing crosses
  an immutable boundary
- prints the matching inode groups and exits non-zero when findings are present
- uses the current workspace state, so it can audit a stopped workspace from
  persisted registry data

## Workspace discovery

Commands such as `edit`, `status`, `stop`, and `check` can discover the workspace by
walking upward from the current directory.

Current algorithm:

1. Start at `cwd`.
2. For each ancestor, derive the workspace ID from the canonical path.
3. Check for the persisted registry state file:

   ```text
   $XDG_STATE_HOME/workspace-portal/workspaces/<workspace-id>.json
   ```

4. If the registry file exists and its stored `workspace` path matches the
   ancestor, use that workspace.
5. Stop at filesystem root.
6. If no workspace is found, require the positional `<workspace>` path.

Current limitation:

- discovery is based on persisted registry state for canonical paths
- there is no longer any marker-file fallback inside the workspace

## State and paths

Current state locations:

```text
Runtime sockets:
  $XDG_RUNTIME_DIR/workspace-portal/<workspace-id>.sock

Persistent state:
  $XDG_STATE_HOME/workspace-portal/workspaces/<workspace-id>.json
```

Fallbacks:

- `XDG_STATE_HOME` falls back to `~/.local/state`
- `XDG_RUNTIME_DIR` falls back to a user-specific path under `/tmp`

Workspace IDs are derived from:

- canonical workspace path
- effective user ID

and hashed to a stable short identifier.

Current state file shape:

```json
{
  "version": 1,
  "workspace": "/home/user/work/current/workspace",
  "workspace_id": "b6c1abcd1234ef56",
  "socket": "/run/user/1000/workspace-portal/b6c1abcd1234ef56.sock",
  "state_file": "/home/user/.local/state/workspace-portal/workspaces/b6c1abcd1234ef56.json",
  "mounted": true,
  "daemon": "running",
  "read_only_default": false,
  "generation": 4,
  "entries": {
    "project-a": {
      "name": "project-a",
      "target": "/home/user/code/project-a",
      "mode": "rw",
      "generation": 3
    },
    "notes": {
      "name": "notes",
      "target": "/home/user/notes/current",
      "mode": "ro",
      "generation": 4
    }
  }
}
```

State writes are atomic via write-to-temp plus rename.

## Control protocol

The daemon uses one Unix domain socket per workspace.

Current protocol is JSON lines over the socket.

Current request types:

```json
{"op":"ping"}
{"op":"status"}
{"op":"add","name":"project-a","target":"/home/user/code/project-a","mode":"rw","replace":false}
{"op":"remove","name":"project-a"}
{"op":"stop"}
```

Current response types:

```json
{"kind":"ack","message":"pong"}
{"kind":"status","workspace":{...}}
{"kind":"error","code":"entry_not_found","error":"..."}
```

Current control-socket properties:

- per-workspace socket path
- socket directory mode `0700`
- socket file mode `0600`
- control access is limited to the owning user

## Filesystem semantics

### Top-level namespace

The root of the portal contains only mapped entries:

```text
/workspace/
  project-a/
  notes/
```

Nested mount-point names are rejected.

Example rejected shape:

```toml
[entries."vendor/foo"]
target = "/home/user/foo"
mode = "rw"
```

### Path translation

A mounted path resolves to:

- top-level entry name
- relative path under that entry
- canonical host target for the entry

Example:

```text
/workspace/project-a/src/lib.rs
  entry: project-a
  relative: src/lib.rs
  target: /home/user/code/project-a/src/lib.rs
```

### Symlinks

Current behavior:

- by default, symlinks within a target are visible through the portal,
  `readlink` is implemented, and symlink traversal works
- with `readlink = false`, symlink inodes remain visible but `readlink`
  returns `ELOOP` and path traversal through the symlink is blocked at the FUSE
  `readlink` step
- with `--nosymfollow`, symlinks remain visible and `readlink` still returns
  the stored link text when `readlink = true`, but path traversal through
  symlink components is disabled by the mount where supported
- broken symlinks remain visible as symlinks and reads fail with `ENOENT`

Daemon-side `safe_open` confinement is a separate host-path protection; the
workspace `readlink` policy controls whether link text is disclosed and whether
symlink traversal is allowed at the FUSE `readlink` callback, while
`--nosymfollow` controls consumer-side traversal through symlinks in the
mounted workspace.

### Read-only entries

Current read-only behavior:

- allow lookup, getattr, readdir, open for read, read, readlink
- reject create, mkdir, symlink, link, unlink, rmdir, rename, truncate, and
  writes
- return `EROFS`, `EPERM`, `EACCES`, or related errno values as appropriate for
  the specific operation

### Read-write entries

Current mounted implementation covers the common operations needed by normal
development workflows:

- lookup
- forget
- getattr
- setattr
- readdir
- opendir
- open
- create
- mkdir
- symlink
- unlink
- rmdir
- rename
- read
- readlink
- write
- flush
- release
- fsync
- copy and copy_file_range (with a manual read/write fallback)
- releasedir
- fsyncdir
- statfs (reports the backing store's real capacity)
- `poll` (returns readiness derived from the open handle's kind and mode)
- `ioctl` (returns `ENOTTY`; not supported)

### Revocation behavior

Removing an entry through `edit` uses soft revocation, and it is the only
behavior:

- new lookups fail immediately
- removed entries disappear from the namespace
- existing open file descriptors continue to work until closed

There is intentionally no hard-revocation mode: removing an entry never forcibly
tears down handles that are already open. If forced teardown is ever needed it
would be a separate feature (FUSE inode invalidation), not a flag.

### Rename behavior

Current behavior:

- same-entry renames are allowed
- cross-entry renames are rejected

### Hard links

Current behavior:

- same-entry hard links are allowed in read-write entries
- hard links are rejected when either endpoint's entry-relative path contains
  an immutable segment
- cross-entry hard links remain rejected

### Caching

The implementation uses conservative TTLs and synthetic inode assignment.

The current behavior is intentionally correctness-biased over aggressive caching.

## Errors and current limits

The implementation currently aims to preserve common filesystem errno values
instead of collapsing everything to `EIO`, but it still does not claim full
POSIX-perfect behavior.

Known current limits:

- Linux and `fuse3` focused
- top-level entries only
- no subtree-level read-only policy inside an entry
- soft revocation only by design (no forced teardown of open handles)
- state and discovery are registry-based; there is no in-workspace metadata file

Daemon-side path resolution is confined beneath each entry target using
`openat2(RESOLVE_BENEATH)`, so the daemon never reads or writes outside the
entry even if the backing store is mutated under it. Symlinks inside an entry
are served verbatim; the consumer's kernel resolves them in the consumer's own
namespace, so an absolute or escaping symlink resolves against the consumer's
root (not the host's) — the mount is a confined view, not a sandbox beyond that.

## Testing and verification

Current automated coverage includes:

- unit tests for pure filesystem/path/state/protocol logic
- control-plane integration tests in [tests/control_plane.rs](../tests/control_plane.rs)
- ignored FUSE E2E tests in [tests/fuse_e2e.rs](../tests/fuse_e2e.rs)

Current FUSE E2E coverage includes:

- mount/start/stop
- add/rm visibility
- read/write flows
- directory lifecycle
- file lifecycle (create/append/overwrite/truncate/fsync)
- flush without a preceding fsync
- read-only rejection
- cross-entry rename rejection
- rename destination is immediately openable
- symlink traversal and broken-symlink behavior
- symlink creation
- copy and copy_file_range (rustc-style file duplication)
- statfs backing-capacity reporting
- setattr timestamp persistence
- soft revocation/coherency checks
- restart and remount recovery

Containerized runner:

```bash
./scripts/fuse-e2e-podman.sh
```

Current default test command inside the container:

```bash
cargo test --offline --locked --test fuse_e2e -- --ignored --test-threads=1
```

Recommended validation order while changing mounted behavior:

1. `cargo test --locked`
2. `cargo check --locked`
3. `./scripts/fuse-e2e-podman.sh`

## Near-term future work

Useful next extensions that are not implemented today:

- subtree-level read-only rules within an entry
- explicit shutdown-with-open-handles semantics and tests
- multi-process concurrency tests
- stronger path confinement using `openat`/`openat2`-style resolution
- CI execution of the Podman FUSE E2E harness
- extended-attribute support: `getxattr` currently returns `ENODATA` and
  `setxattr`/`listxattr`/`removexattr` are unimplemented, so `cp -a`, SELinux
  labeling, and file capabilities silently lose xattrs
- I/O hot-path efficiency: `write`/`lookup`/`getattr` clone the full
  `PortalState` per call and `readdir` re-reads and re-stats the whole directory
  on every invocation (O(n²) for large directories); correct for typical sizes
  but worth profiling before large-repo use

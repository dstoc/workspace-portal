# workspace-portal

`workspace-portal` is a Rust CLI and daemon that provides a stable FUSE-mounted
workspace for exposing selected host directories.

It is intended for container-based local development workflows where you want a
single persistent workspace path, while still being able to add or remove
project directories dynamically without restarting the container.

The intended workflow is:

```bash
workspace-portal start ./workspace --bg
workspace-portal add ~/code/project-a project-a
workspace-portal add ~/notes/current notes --ro
workspace-portal freeze vendor
workspace-portal status
```

Inside the mounted workspace, the user sees stable top-level entries backed by
real host paths:

```text
workspace/
  project-a/
  notes/
```

## Requirements

### Runtime

- Linux
- FUSE support
- `fusermount3`
- permission to access `/dev/fuse`

## Install

Install from github:

```bash
cargo install --locked --git https://github.com/dstoc/workspace-portal workspace-portal
```

## Quick Start

Start a workspace in the background:

```bash
workspace-portal start ./workspace --bg
```

Add a writable entry:

```bash
workspace-portal add --workspace ./workspace ~/code/project-a project-a
```

Add a read-only entry:

```bash
workspace-portal add --workspace ./workspace --ro ~/notes/current notes
```

Freeze a segment name anywhere inside entries:

```bash
workspace-portal freeze --workspace ./workspace vendor
```

When you run commands from inside the workspace, `--workspace <path>` is
optional because the CLI can discover the workspace automatically.

Check the workspace:

```bash
workspace-portal status --workspace ./workspace
```

Sample output:

```text
Workspace: /home/user/workspace/workspace-portal/workspace
Mount:     mounted
Daemon:    running
Socket:    /run/user/1000/workspace-portal/7f3a.sock
IMMUTABLE SEGMENTS: vendor

ENTRY     TARGET                    MODE
-----     ------                    ----
project-a /home/user/code/project-a rw
notes     /home/user/notes/current  ro
```

List known workspaces:

```bash
workspace-portal list
```

Sample output:

```text
WORKSPACE                              STATUS   ENTRIES
/home/user/workspace/workspace-portal/workspace running  2
```

Stop and unmount:

```bash
workspace-portal stop --workspace ./workspace
```

## CLI

### `start`

Start the daemon and mount a workspace:

```bash
workspace-portal start <workspace> [--bg] [--read-only] [--nosymfollow]
```

Options:

- `--bg` runs the daemon in the background
- `--socket <path>` overrides the control socket
- `--state-dir <path>` overrides the state directory
- `--allow-other` enables FUSE `allow_other`
- `--no-allow-other` disables it explicitly
- `--read-only` makes the workspace read-only by default
- `--nosymfollow` keeps symlinks visible and readable with `readlink`, but
  disables traversal through symlink components in the mount
- `--adopt` uses an existing workspace directory
- `--force` overrides stale state or mount conditions

### `add`

Add a host directory as a top-level entry:

```bash
workspace-portal add <target> <mount-point> [--workspace <path>] [--ro|--rw]
```

Options:

- `--workspace <path>` skips workspace discovery
- `--ro` adds a read-only entry
- `--rw` adds a writable entry
- `--replace` replaces an existing entry

### `rm`

Remove a top-level entry:

```bash
workspace-portal rm <mount-point> [--workspace <path>]
```

Removing an entry drops it from the namespace immediately; file handles that are
already open continue to work until they are closed.

### `freeze`

Freeze a segment name workspace-wide:

```bash
workspace-portal freeze <segment> [--workspace <path>]
```

If `vendor` is frozen, any subtree rooted at a path component exactly named
`vendor` becomes immutable through the mount. For example, `project-a/vendor`
and `project-a/src/vendor` are frozen, while `vendors` and `vendor2` are not.

Reads still work. Mutations through the mount fail with `EPERM`, including
create, write, rename, unlink, `mkdir`, and metadata changes. Creating a new
matching segment is also denied.

### `thaw`

Remove a workspace-wide immutable segment rule:

```bash
workspace-portal thaw <segment> [--workspace <path>]
```

### `edit`

Edit the whole entry set at once in your editor:

```bash
workspace-portal edit [--workspace <path>]
```

Opens the current entries in `$VISUAL`/`$EDITOR`/`vi` as an `ENTRY TARGET MODE`
table. On save, the difference is applied to the running mount — add, remove,
rename, retarget, or flip entries between `ro` and `rw`. Flipping or removing an
entry leaves file handles that are already open undisturbed; only later opens
see the change. An unchanged or invalid buffer applies nothing.

Immutable segment rules are separate from `edit`. Use `freeze` and `thaw` to
manage them. Like `rw` to `ro` flips, freezing does not retroactively revoke
already-open writable file handles; only later path-based mutations and writable
opens see the new rule.

### `status`

Show workspace status:

```bash
workspace-portal status [--workspace <path>] [--json]
```

Human output includes an `IMMUTABLE SEGMENTS` line. JSON output includes a
top-level `immutable_segments` array.

### `stop`

Stop the daemon and unmount the workspace:

```bash
workspace-portal stop [--workspace <path>] [--lazy] [--force]
```

### `list`

List known workspaces:

```bash
workspace-portal list
```

### `check`

Report FUSE and workspace prerequisites:

```bash
workspace-portal check [--workspace <path>]
```

## State and Paths

The tool keeps runtime and state data in XDG-style locations.

Typical layout:

```text
$XDG_RUNTIME_DIR/workspace-portal/*.sock
$XDG_STATE_HOME/workspace-portal/workspaces/*.json
```

The workspace itself also contains marker metadata so commands can rediscover
the mounted workspace by walking upward from the current directory.

## Testing

### Fast local tests

These run without a live FUSE mount:

```bash
cargo test
```
### FUSE end-to-end tests

The FUSE suite is intentionally ignored by default:

```bash
cargo test --test fuse_e2e -- --ignored --test-threads=1
```

### Podman harness

Run the ignored FUSE suite in a containerized Linux environment:

```bash
./scripts/fuse-e2e-podman.sh
```

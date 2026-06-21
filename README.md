# workspace-portal

`workspace-portal` is a Rust CLI and daemon that provides a stable FUSE-mounted
workspace for exposing selected host directories.

It is intended for container-based local development workflows where you want a
single persistent workspace path, while still being able to add or remove
project directories dynamically without restarting the container.

The intended workflow is:

```bash
workspace-portal start ./workspace --bg
workspace-portal edit ./workspace
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

Edit entries, immutable segments, and symlink policy:

```bash
workspace-portal edit ./workspace
```

When you run commands from inside the workspace, the workspace path is optional
because the CLI can discover the workspace automatically.

Check the workspace:

```bash
workspace-portal status ./workspace
```

Sample output:

```text
Workspace: /home/user/workspace/workspace-portal/workspace
Mount:     mounted
Daemon:    running
Socket:    /run/user/1000/workspace-portal/7f3a.sock
READLINK: true
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
workspace-portal stop ./workspace
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
- `--nosymfollow` disables symlink traversal in the mount where supported
- `--adopt` uses an existing workspace directory
- `--force` overrides stale state or mount conditions

### `edit`

Edit the whole entry set at once in your editor:

```bash
workspace-portal edit [<workspace>]
```

Opens the current desired state in `$VISUAL`/`$EDITOR`/`vi` as a TOML buffer
with `version = 1`, `readlink = true`, an `immutable_segments = [...]` array, and
`[entries.<name>]` tables containing `target` and `mode`.
Editing this buffer can add, remove, rename, retarget, or flip entries between
`ro` and `rw`, and it can also manage the `readlink` policy and immutable
segments. Read-write entries also support same-entry hard links unless either
endpoint is under an immutable segment. An unchanged or
invalid buffer applies nothing; parse or validation errors reopen the editor
with a commented error at the top.

As before, flipping or removing an entry leaves file handles that are already
open undisturbed; only later opens see the change. Likewise, freezing does not
retroactively revoke already-open writable file handles; only later
path-based mutations and writable opens see the new rule.

Symlink inodes remain visible through the portal. Setting `readlink = false`
blocks the FUSE `readlink` operation with `ELOOP`, which also prevents traversal
through symlinks. `--nosymfollow` is separate: it blocks traversal at the mount
layer where supported while still allowing `readlink` when the workspace policy
is true.

### `status`

Show workspace status:

```bash
workspace-portal status [<workspace>] [--json]
```

Human output includes `READLINK` and `IMMUTABLE SEGMENTS` lines. JSON output
includes top-level `readlink` and `immutable_segments` fields.

### `stop`

Stop the daemon and unmount the workspace:

```bash
workspace-portal stop [<workspace>] [--lazy] [--force]
```

### `list`

List known workspaces:

```bash
workspace-portal list
```

### `check`

Report FUSE and workspace prerequisites:

```bash
workspace-portal check [<workspace>]
```

### `forget`

Remove stored metadata for a stopped workspace:

```bash
workspace-portal forget <workspace>
```

### `audit`

Audit hard links that cross immutable boundaries, or symlink targets that
escape an entry:

```bash
workspace-portal audit hardlinks <workspace>
workspace-portal audit symlinks <workspace>
```

`audit hardlinks` scans the workspace targets, prints any crossing hard-link
groups, and exits non-zero when findings are present. `audit symlinks` scans
the workspace targets for symlink target text that would resolve outside an
entry if followed, prints matching entry-relative paths and stored targets, and
exits non-zero when findings are present. If no findings are present, each
command prints its no-findings message and exits `0`.

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

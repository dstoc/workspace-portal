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

ENTRY      MODE TARGET
-----      ---- ------
project-a  rw   /home/user/code/project-a
notes      ro   /home/user/notes/current
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
workspace-portal start <workspace> [--bg] [--read-only]
```

Options:

- `--bg` runs the daemon in the background
- `--socket <path>` overrides the control socket
- `--state-dir <path>` overrides the state directory
- `--allow-other` enables FUSE `allow_other`
- `--no-allow-other` disables it explicitly
- `--read-only` makes the workspace read-only by default
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
- `--follow-symlinks` / `--no-follow-symlinks` control target handling policy

### `rm`

Remove a top-level entry:

```bash
workspace-portal rm <mount-point> [--workspace <path>]
```

Options:

- `--soft` uses soft revocation behavior
- `--hard` requests hard revocation semantics where supported

### `status`

Show workspace status:

```bash
workspace-portal status [--workspace <path>] [--json]
```

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

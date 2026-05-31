# Proposal: `edit` the workspace entries in an editor

## Motivation

Changing what a running portal exposes is currently a sequence of discrete
`workspace-portal add` / `workspace-portal rm` invocations. Reworking several
entries at once — renaming a couple, flipping one from `rw` to `ro`, dropping a
stale one — means remembering the exact flags for each and running them in the
right order. There is no way to see the whole set and edit it as a unit.

`workspace-portal edit` should open the current entry set in the user's editor,
and on save apply the difference to the live mount. Crucially, flipping an entry
between `ro` and `rw` (or renaming/removing one) must not disturb file handles
that are already open — in particular it must not invalidate an in-flight
read-only handle.

## Problem statement

The daemon already owns the live entry set and already accepts mutations over a
control socket, so the substrate for `edit` exists:

- `Daemon` holds `state: Arc<RwLock<PortalState>>` (`src/daemon/runtime.rs:27`)
  and shares the same `Arc` with the mounted `PortalFs`
  (`src/daemon/runtime.rs:224`). Mutations under the write lock are immediately
  visible to the FUSE layer; there is no separate "reload".
- `Daemon::handle_request` (`src/daemon/runtime.rs:135`) already implements
  `Add { name, target, mode, replace }` and `Remove { name, revocation }` over a
  newline-delimited JSON protocol (`src/protocol.rs:9`), and the CLI already
  speaks it via `send_request` (`src/daemon.rs:396`).

What is missing is (1) a command that round-trips the entry set through an
editor, and (2) a clear statement — backed by a test — that applying the result
does not invalidate open handles.

The handle-safety property is already true of the code, and the proposal must
not regress it. An open file is represented by
`OpenHandle { ino, file: File, kind, writable }` (`src/fs/runtime.rs:22`),
stored in `runtime.handles`. The `writable` flag and the underlying `File` fd
are captured at open time; reads and writes operate on that fd. The access check
`ensure_writable_entry` (`src/fs/resolve.rs:128`) runs only on the open/resolve
path (`resolve_write_path`, `resolve_parent_child_writable`) — it is never
re-evaluated against live `state` for a handle that is already open. Therefore an
entry's `mode` changing in `state.entries` cannot affect a handle that predates
the change: a handle opened `ro` stays `ro`, a handle opened `rw` keeps writing.
This holds **only** if applying an edit mutates `state.entries` in place and
never rebuilds the mount or clears `runtime.handles`.

## Proposal

Add an `edit` subcommand that edits a *projection* of the entry set — not the
raw `portal.json` — and applies the result as the minimal diff over the existing
control socket.

### CLI surface

Add `Edit(EditCommand)` to `Commands` (`src/cli.rs:23`) and an `edit` arm in
`run()` that calls a new `daemon::edit`:

```
workspace-portal edit [--workspace <path>]
```

`--workspace` matches the discovery-override flag every other subcommand already
carries. No `--ro`/`--rw` flags: the edited document carries the modes.

### What the user edits: the entry projection, not raw state

`portal.json` is the whole `PortalState` — `version`, `socket`, `mounted`,
`daemon`, `generation`, `workspace_id` — none of which the user should hand-edit.
The editable surface is exactly the per-entry fields already projected by
`EntryState { name, target, mode }` (`src/protocol.rs:55`). `edit` fetches the
authoritative live set with `ControlRequest::Status` (falling back to the
persisted snapshot when the daemon is down), renders the `Vec<EntryState>` to a
temp file, and parses the edited file back into a `Vec<EntryState>`.

### Editor round-trip and fallback

1. Resolve the editor: `$VISUAL`, then `$EDITOR`, then `vi`.
2. Write the rendered entries to a temp file with the format-appropriate
   extension.
3. Launch the editor on the temp file and wait for it to exit.
4. If the editor exits non-zero, or the file is byte-identical to what was
   written, print `no changes` and exit 0 without contacting the daemon.
5. Parse and validate the edited file (see below). On a parse/validation error,
   print the error and re-open the editor on the same buffer so the user's edits
   are not lost; repeat until valid or the user leaves it unchanged.

Validation reuses what already exists: `paths::validate_entry_name` for each
`name`, rejection of duplicate names, and a non-empty `target`. Target
canonicalization and existence remain the daemon's job, performed on `Add` via
`canonicalize_target` (`src/daemon/runtime.rs:150`); a bad target surfaces as a
per-entry daemon error.

### Applying the edit as a minimal diff (handle preservation)

`edit` diffs the parsed entries against the snapshot it started from and sends
only what changed, reusing the existing requests:

- entry added, or its `target`/`mode` changed → `Add { replace: true }`
- entry present before but absent now → `Remove { revocation: Soft }`
- entry unchanged → **no request is sent**

This is what keeps open handles intact, and why the diff is computed rather than
blindly re-adding everything:

- An `ro`↔`rw` flip becomes a single `Add { replace: true }`, which calls
  `state.add_entry(entry, replace = true)` (`src/state.rs:159`). That swaps the
  `EntryRecord` in `state.entries` and bumps its generation. It does **not** walk
  `runtime.handles`, so every handle opened before the flip keeps its captured
  `writable` flag and fd. New opens see the new mode.
- An unchanged entry produces no request, so its `EntryRecord`, generation, and
  any open handles are provably untouched.
- A removed entry drops out of `state.entries`; per the existing soft-revocation
  behavior, open fds keep working (`open_handle_metadata`,
  `src/fs/runtime.rs:163`), matching today's `rm` semantics.

The whole edited document is validated client-side **before** any request is
sent, so a typo in one entry never results in a partially applied config.

This proposal deliberately does **not** add a "reconfigure everything" control
request, unmount/remount, or any clearing of `runtime.handles`. The narrow
diff over existing requests is both the smallest change and the one that makes
handle preservation obvious. (A single atomic `Reconfigure { entries }` request
would remove the residual risk of a mid-sequence daemon error leaving a
partially applied set; it is noted as possible future work, not part of this
proposal, because it widens the protocol for a case the client-side
pre-validation already largely covers.)

### Config format for the editor buffer

The buffer format is a real ergonomic choice because hand-editing is the point.
We will implement **one** of the following.

**Option A — JSON.** Render the `Vec<EntryState>` with `serde_json` (already a
dependency).
- Pros: zero new dependencies; identical serialization to everything else in the
  crate; trivial to parse back.
- Cons: no comments, so a user can't annotate ("# temporary"); trailing-comma
  and quoting papercuts are exactly the friction an interactive editor exposes
  most. JSON is unpleasant to hand-edit, which undercuts the feature.

**Option B — TOML.** Render as an array of tables via a new `toml`
dependency:
```toml
[[entry]]
name = "docs"
target = "/home/user/project/docs"
mode = "ro"

[[entry]]
name = "build"
target = "/tmp/build-out"
mode = "rw"
```
- Pros: designed for humans editing by hand — comments, forgiving whitespace, no
  trailing-comma traps; maps directly onto `Vec<EntryState>` with the existing
  `serde` derives and the `mode` `"ro"`/`"rw"` strings already used by
  `AccessMode` (`src/state.rs:27`). Roughly a `toml::from_str` / `toml::to_string`
  swap.
- Cons: one new dependency (`toml`). It is a second on-disk representation
  alongside the JSON `portal.json` (acceptable: the buffer is transient, not the
  source of truth).

**Option C — our own line format.** One entry per line, columns in
`ENTRY TARGET MODE` order, preceded by two `#`-commented header lines — a column
header and a dashed rule — matching the `status` layout (see below). The entry
rows are rendered with whitespace-aligned columns:
```
# ENTRY  TARGET               MODE
# -----  ------               ----
docs     /home/user/project   ro
build    /tmp/build-out       rw
```
Lines beginning with `#` (the two header lines and any the user adds) are
ignored on parse; the alignment is cosmetic and the user need not preserve it.
Because the columns are anchored at both ends — `name` has a restricted charset
(`validate_entry_name`) and `mode` is a fixed `ro`/`rw` token — parsing takes the
first whitespace-delimited token as `name`, the last as `mode`, and the trimmed
span between them as `target`. That keeps a `target` path containing spaces
unambiguous without requiring quoting.
- Pros: maximally terse and diffable; trivial to eyeball, and the `#`-commented
  `ENTRY`/`----` header makes the format self-documenting inside the buffer; `#`
  comments are easy; no serialization framework or new dependency needed.
- Cons: we write and own a small hand-rolled parser plus its error messages.
  This is largely a one-time cost rather than an ongoing burden: the buffer is a
  throwaway edit surface, never persisted (the source of truth stays
  `portal.json`), so there is no on-disk format to migrate and no external
  consumer to keep compatible — the format can be changed freely in any later
  release. The remaining real cost is getting the parser's error messages as
  good as `serde`'s for free, and the anchored-column rule above only works while
  the schema stays this flat (three fields with constrained ends).

Reuse in `status`: the Option C column layout should also back the
human-readable `status` entry list. Today `print_status`
(`src/daemon/output.rs:25`) already prints aligned columns, but in
`ENTRY MODE TARGET` order under a bare `ENTRY`/`----` header. The shared renderer
reorders these to `ENTRY TARGET MODE` (so `mode` anchors the parse) and emits the
same `ENTRY`/`----` header — printed bare in `status`, `#`-commented in the
`edit` buffer. Users then see one consistent representation across the two
commands, and the renderer is exercised on every `status` call rather than only
during an edit. This shared use is a point in Option C's favor — the line format
earns its keep in two places — and it does not apply to Options A/B, whose
JSON/TOML buffers would not be a sensible `status` display.

Recommendation: **Option C (line format)**. For a flat three-field record the
column layout is the most readable thing to hand-edit, the header+example lines
make it self-documenting, and it adds no dependency. Its one notable cost — a
hand-rolled parser — is bounded precisely because the buffer is transient: it is
never persisted, so the format carries no migration or compatibility obligation
and can be revised freely later. Option B (TOML) remains the natural fallback if
the per-entry schema ever grows beyond a flat handful of fields, at which point a
real serializer earns its dependency; Option A (JSON) keeps exactly the editing
friction the feature is meant to remove.

## Non-goals

- A live transport for `edit`. It reuses the existing control socket and the
  existing `Add`/`Remove` requests; no new protocol op is added.
- An atomic whole-set `Reconfigure` request. Client-side pre-validation covers
  the common failure; full atomicity is deferred until there is a concrete need.
- Migrating an open handle's access when its entry's `mode` changes. By design,
  live handles keep the access they were opened with; only future opens see the
  new mode.
- Wiring up hard vs soft revocation. `edit` uses the current `Remove`
  (soft) behavior unchanged; revocation semantics are a separate concern.
- Editing non-entry state (`socket`, `workspace`, `read_only_default`, etc.).
  Those are not part of the editable projection.
- Auto-reloading `portal.json` when it changes on disk outside `edit`.

## Verification

- Unit test (`src/state.rs` tests): after `add_entry(replace = true)` flips an
  entry from `ReadWrite` to `ReadOnly`, an `OpenHandle` constructed with
  `writable = true` still reports `writable` and its `mode`-independent fd is
  unchanged — i.e. `state.add_entry` never touches `runtime.handles`.
- Unit test for the diff: given a starting `Vec<EntryState>` and an edited one,
  the planner emits `Add { replace: true }` only for added/changed entries,
  `Remove` only for dropped entries, and nothing for unchanged entries.
- Unit test: an edited buffer with a duplicate `name` or an invalid entry name
  is rejected before any request is constructed.
- Unit test: a byte-identical edit produces an empty diff (no requests).
- E2E test (`tests/fuse_e2e.rs`, following the existing `Fixture`/`run`
  helpers): open a file under an `rw` entry and hold the fd; run `edit` to flip
  that entry to `ro`; confirm the held fd can still `write`, while a fresh `open`
  for write returns `EACCES`/`EPERM`. Then run the full suite via
  `./scripts/fuse-e2e-podman.sh`.
- E2E test: `$EDITOR` set to a script that leaves the buffer unchanged results in
  no entry changes and a `no changes` message.

## Success criteria

- `workspace-portal edit` opens the current entries in `$VISUAL`/`$EDITOR`/`vi`
  and, on a valid save, applies the difference to the running mount.
- Flipping an entry `ro`↔`rw` (or renaming/removing one) via `edit` causes no
  I/O error and no access-mode change on any handle open at the time; unchanged
  entries are not touched at all.
- An invalid or unchanged edited buffer applies nothing and never leaves a
  partially applied entry set.
- Exactly one buffer format is implemented.
- If Option C is chosen, the `status` entry list and the `edit` buffer rows are
  produced by the same column renderer (`ENTRY TARGET MODE` columns under a
  shared `ENTRY`/`----` header), with no second, divergent layout.
- All existing tests continue to pass.

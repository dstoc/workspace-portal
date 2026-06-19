# Proposal: TOML-backed `edit` config

## Motivation

`workspace-portal edit` currently opens only the entry table in an editor. That
was enough when the editable workspace surface was just
`EntryState { name, target, mode }`, but it no longer covers workspace-wide
configuration such as immutable segment rules.

Today a user who wants to rework entries and immutable segments has to combine
two editing styles:

- `workspace-portal edit` for entries
- `workspace-portal freeze` / `workspace-portal thaw` for immutable segment
  names

That split is awkward when the goal is to review and change the whole editable
workspace config as one desired state.

## Problem statement

The current edit implementation is intentionally narrow:

- `daemon::edit` (`src/daemon.rs`) fetches only `workspace.entries` from
  `ControlRequest::Status`, renders them with `entry_format::render_entries`,
  parses them back with `entry_format::parse_entries`, and applies the
  `entry_format::plan_edit` diff.
- `entry_format` (`src/daemon/entry_format.rs`) owns a custom
  whitespace-delimited table format. That format is readable for flat entry
  rows, but it has no natural place for workspace-level settings.
- Immutable segment rules are already live and persisted state on
  `PortalState::immutable_segments` (`src/state.rs`), surfaced over the control
  protocol through `WorkspaceSnapshot::immutable_segments` and
  `ControlRequest::{Freeze, Thaw}` (`src/protocol.rs`).

The missing piece is an editable config document that can represent all
user-editable workspace settings, including defaults, without exposing raw
daemon state such as `workspace_id`, `socket`, `mounted`, `daemon`,
`generation`, or entry generations.

## Proposal

Change the `workspace-portal edit` buffer from the custom entry table to a TOML
desired-state document.

The persisted daemon state remains `portal.json`. TOML is only the
human-edited projection used by `edit`; it is not the source of truth on disk
and it is not the control protocol format.

### Editable schema

Use a complete document with top-level workspace settings and an `entries` map:

```toml
version = 1
immutable_segments = ["vendor", "node_modules"]

[entries.docs]
target = "/home/user/project/docs"
mode = "rw"

[entries.cache]
target = "/tmp/project-cache"
mode = "ro"
```

This maps to an internal editable shape like:

```rust
#[derive(Clone, Debug, Deserialize, Serialize)]
struct EditableConfig {
    #[serde(default = "editable_config_version")]
    version: u32,

    #[serde(default)]
    immutable_segments: BTreeSet<String>,

    #[serde(default)]
    entries: BTreeMap<String, EditableEntry>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct EditableEntry {
    target: PathBuf,

    #[serde(default)]
    mode: AccessMode,
}
```

`entries` is a map rather than an array of tables. The table key is the entry
name, so the editable document follows the same shape as
`PortalState.entries: BTreeMap<String, EntryRecord>` and duplicate entry names
cannot appear in the TOML data model.

`immutable_segments` lives at the top level rather than under a `[policy]`
table. There is only one workspace-wide policy setting today, and the top level
keeps the file shorter and easier to scan. A future version can introduce a
`[policy]` table if several related settings justify the extra grouping.

### Complete desired state

The generated file should always include every editable setting, even when it is
set to its default value:

```toml
version = 1
immutable_segments = []

[entries.docs]
target = "/home/user/project/docs"
mode = "rw"
```

The parsed document is the complete desired state. Missing fields fall back to
their defaults:

- missing `immutable_segments` means `[]`, so deleting the line clears all
  immutable segment rules
- missing `entries` means no entries
- missing entry `mode` means `rw`, matching `AccessMode::default()`

The renderer should still emit defaults explicitly so the buffer is
self-contained and reviewable. Defaults are fallback behavior for hand-edited
documents, not an excuse to omit settings from generated output.

### Empty entries

When there are no entries, generate a commented example:

```toml
version = 1
immutable_segments = []

# [entries.docs]
# target = "/path/to/docs"
# mode = "rw"
```

The example is not parsed as state, so saving the untouched buffer remains a
no-op. If the user uncomments it without changing the placeholder path, normal
target validation and daemon-side canonicalization produce the usual error.

When one or more entries exist, do not include the example. The real entries are
enough guidance and the buffer should stay focused on the actual desired state.

### Validation

Validate the whole parsed document before sending any control request:

- `version` must be `1`
- every entry name must pass `paths::validate_entry_name`
- every entry target must be non-empty after display/trim
- every immutable segment must pass
  `paths::validate_immutable_segment_name`
- every mode must deserialize as `ro`, `readonly`, `rw`, or `readwrite`, using
  the existing `AccessMode` deserializer

The parser should reject unknown fields. In an editor-facing config, a typo
should fail loudly instead of silently doing nothing:

```toml
immutable_segment = ["vendor"] # invalid: should be immutable_segments
```

Rejecting unknown fields is especially important because deleting the correct
`immutable_segments` line intentionally clears the rule set; a misspelled field
must not look like a successful edit.

### Applying the config

`edit` should fetch one authoritative `before` snapshot, render the full TOML
document, parse the edited document into an `EditableConfig`, then produce a
minimal request plan.

Entry planning preserves the current behavior:

- entry added, or its `target`/`mode` changed -> `Add { replace: true }`
- entry present before but absent after -> `Remove`
- entry unchanged -> no request

Immutable segment planning is the set difference between the current and desired
sets:

- segment present after but absent before -> `Freeze { segment }`
- segment present before but absent after -> `Thaw { segment }`
- segment unchanged -> no request

The plan should be computed after all validation succeeds. That preserves the
current user-facing guarantee that a parse or validation failure does not apply
any partial edit.

This proposal keeps the existing sequence of control requests. It does not add
an atomic whole-config protocol operation. A future `Reconfigure` request may be
worth adding if `edit` grows enough settings that partial daemon-side failure
becomes a practical problem, but entries plus immutable segment rules can reuse
the existing `Add`, `Remove`, `Freeze`, and `Thaw` operations.

### Editor round trip

Keep the existing editor behavior:

1. Resolve `$VISUAL`, then `$EDITOR`, then `vi`.
2. Write the generated TOML to a temp file.
3. Launch the editor and wait.
4. If the editor exits non-zero, print `no changes` and exit 0.
5. If the buffer is byte-identical to the generated TOML on the first pass,
   print `no changes` and exit 0.
6. On parse or validation error, print the error, write the error into the top
   of the temp file as TOML comments, and reopen the same temp file until the
   user fixes it or leaves it unchanged from the previous failed pass.

Use a `.toml` temp-file extension so editors can select TOML syntax
highlighting.

The in-buffer error should be easy to see without changing the TOML data model:

```toml
# workspace-portal edit error:
# invalid immutable segment name: "../vendor"
# Fix the config below, then save and exit.

version = 1
immutable_segments = ["../vendor"]
```

Before inserting a new error block, remove any previous
`workspace-portal edit error` block that this command inserted. That prevents
stale errors from accumulating after repeated validation attempts. User-written
comments elsewhere in the file should be left untouched.

### Status output

This proposal does not require changing `status`.

The human `status` output can keep using the existing entry table and
`IMMUTABLE SEGMENTS` line. `status --json` already exposes entries and
immutable segment names. `edit` is the only command whose buffer format changes.

### Dependencies

Add a TOML parser/serializer dependency. Prefer the `toml` crate unless a
specific need for preserving comments or formatting appears during
implementation.

The generated document is canonical and comments are limited to the empty-entry
example, so round-tripping user comments is not required. The user's edited file
is parsed into desired state and then discarded, just like the current custom
entry-table buffer.

## Suggested implementation shape

1. Replace `src/daemon/entry_format.rs` with a more general edit-config module,
   or add `src/daemon/edit_config.rs` and leave `entry_format` only for status
   table rendering if it still has a caller.
2. Introduce `EditableConfig` and `EditableEntry` in that module.
3. Add `from_snapshot` / `render` / `parse` / `validate` helpers around
   `WorkspaceSnapshot`.
4. Replace `plan_edit(before_entries, after_entries)` with a planner that takes
   before and after editable configs and emits `Vec<ControlRequest>`.
5. Update `daemon::edit` to fetch the full `WorkspaceSnapshot`, not only
   `workspace.entries`, before rendering.
6. Change the temp filename from `.conf` to `.toml`.

The existing `EditArgs`, CLI surface, and editor loop can stay in place.

## Migration plan

There is no persisted format migration. Existing `portal.json` files continue
to load through `PortalState::load_from_path`, and the JSON control protocol
continues to use `serde_json`.

The only user-visible compatibility break is the interactive `edit` buffer
format. That is acceptable because the buffer is transient and generated on
each invocation. Tests and scripted editors that modify the old column layout
must be updated to edit TOML instead.

## Non-goals

- Migrating persisted workspace state from JSON to TOML.
- Changing the JSON control protocol.
- Adding a live config file watcher or automatic reload when a user edits
  `portal.json` or any TOML file outside `workspace-portal edit`.
- Preserving arbitrary comments or formatting from a previous `edit` session.
  Each invocation generates a fresh canonical document from live state.
- Adding a generic policy engine. The only new editable workspace-wide setting
  in this proposal is the existing immutable segment set.
- Adding an atomic whole-config `Reconfigure` control request.
- Changing immutable-segment filesystem semantics.

## Verification

1. Unit test: rendering an empty snapshot produces `version = 1`,
   `immutable_segments = []`, and a commented `[entries.docs]` example.
2. Unit test: rendering a non-empty snapshot includes explicit `mode` values,
   explicit `immutable_segments`, and no commented example.
3. Unit test: parsing a map-shaped TOML document returns the expected entries
   and immutable segment set.
4. Unit test: deleting `immutable_segments` parses as an empty set.
5. Unit test: deleting an entry's `mode` parses that entry as `rw`.
6. Unit test: invalid entry names, invalid immutable segment names, empty
   targets, unsupported `version`, and unknown fields are rejected before any
   request plan is produced.
7. Unit test: planning emits `Add`/`Remove` only for changed entries and
   `Freeze`/`Thaw` only for changed immutable segment membership.
8. Unit test: after a parse or validation failure, the reopened buffer contains
   a top-of-file commented error block, and a later failure replaces that block
   rather than appending another one.
9. E2E test in `tests/fuse_e2e.rs`: run `edit` with a scripted TOML editor that
   changes an entry from `rw` to `ro` and adds `vendor` to
   `immutable_segments`; confirm status JSON reports both changes and the held
   write-handle behavior from the existing edit test is preserved.
10. E2E test: run `edit` with `/bin/true`; confirm it reports `no changes` and
   leaves entries and immutable segment rules untouched.

## Success criteria

- `workspace-portal edit` opens a TOML document representing all editable
  workspace settings.
- The generated TOML always includes explicit defaults.
- An empty workspace gets a commented example entry without changing desired
  state.
- Users can add, change, rename, and remove entries through the `[entries]` map.
- Users can add and remove immutable segment rules through the top-level
  `immutable_segments` array.
- Invalid TOML or invalid values apply no changes.
- Parse and validation errors are visible inside the reopened editor buffer as
  comments.
- Existing persisted state and protocol formats remain JSON.

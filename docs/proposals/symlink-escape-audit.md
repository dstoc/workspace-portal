# Proposal: symlink escape audit

## Motivation

`workspace-portal` intentionally exposes symlinks as symlink inodes. When
`readlink = true`, consumers can inspect the link target and, unless the mount
was started with `--nosymfollow`, traverse through the symlink. That behavior is
compatible with ordinary project trees, but it can make a portal view harder to
review: a workspace may contain symlinks whose target text points outside the
entry target.

An escaping symlink is not, by itself, a daemon-side host escape. The daemon's
own host path resolution is confined by `safe_open`, and consumer-side symlink
resolution happens in the consumer's namespace. Still, many workspaces want to
know whether the exposed tree contains symlink target text such as `/etc` or
`../../outside`. This is especially useful before enabling symlink traversal for
a containerized workload.

The portal already has an audit command family for hard-link alias diagnostics.
Symlink escape detection fits the same model: explicit, manual, diagnostic, and
non-mutating.

## Problem statement

Current symlink behavior is split across the FUSE layer and host confinement:

- `PortalFs::symlink` in `src/fs/callbacks.rs` allows symlink creation in
  writable paths by calling `safe_open::symlink`.
- `PortalFs::readlink` returns the stored target text when `PortalState.readlink`
  is true, and returns `ELOOP` when `readlink` is false.
- `safe_open::readlink` in `src/fs/safe_open.rs` reads the symlink target via a
  confined parent fd and does not follow the link.
- `safe_open` confines daemon-side path resolution beneath the entry target, so
  daemon operations do not follow escaping symlinks into host paths.

There is no command that scans configured entries and reports symlink target text
that would escape an entry if followed by a consumer. A user can inspect
individual links with `find` or `readlink`, but that does not integrate with
workspace state, stopped workspaces, or the existing audit output pattern.

## Proposal

Add a second audit type:

```bash
workspace-portal audit symlinks <workspace>
```

The command scans all configured entry targets and reports symlink inodes whose
stored target text resolves lexically outside the containing entry. It uses the
current workspace state in the same style as `status` and `audit hardlinks`:
when the daemon socket is live it asks the daemon for status, otherwise it uses
persisted registry state.

This audit is diagnostic only. It does not change runtime symlink traversal,
`readlink` policy, `--nosymfollow`, or daemon-side confinement.

### Escape semantics

For each symlink found under an entry target, classify the stored target text
relative to the symlink's containing directory:

- absolute targets are findings;
- relative targets that lexically climb above the entry root are findings;
- relative targets that remain within the entry root are not findings;
- broken targets that still remain lexically inside the entry are not findings;
- unreadable symlinks or scan errors make the audit fail because the result is
  unreliable.

Examples for an entry named `docs` whose target root contains `src/link`:

```text
src/link -> ../README.md       # inside, not reported
src/link -> ../../outside      # outside, reported
src/link -> /etc/passwd        # outside, reported
src/link -> missing/file       # inside lexical target, not reported
```

The first implementation should use lexical target analysis only. It should not
resolve symlink chains or inspect whether non-final path components are symlinks
on disk. That keeps the command predictable, cheap relative to full resolution,
and aligned with what users see in `readlink` output.

### CLI shape

Extend the existing audit command tree in `src/cli.rs`:

```rust
AuditCommands::Hardlinks(HardlinkAuditCommand)
AuditCommands::Symlinks(SymlinkAuditCommand)
```

The user-facing syntax is:

```bash
workspace-portal audit symlinks <workspace>
```

Use a required positional workspace, matching `audit hardlinks`.

Suggested output when findings exist:

```text
symlinks resolving outside entry targets:

docs:src/escape -> ../../outside
docs:absolute -> /etc/passwd
```

Suggested output when no findings exist:

```text
no symlinks resolving outside entry targets found
```

Suggested exit status:

- `0` when no escaping symlink targets are found;
- non-zero when any escaping symlink targets are found;
- non-zero for scan errors that prevent a reliable result.

### Scanner shape

Add `daemon::audit_symlinks` in `src/daemon.rs`, reusing the existing
`load_workspace_snapshot` helper added for `audit hardlinks`.

Traversal should mirror the hard-link audit:

- scan every configured `WorkspaceSnapshot.entries` target;
- use `fs::symlink_metadata`/`lstat` semantics;
- recurse into real directories;
- do not follow symlinked directories;
- for symlink files, call `fs::read_link` on the backing path;
- sort findings deterministically by entry name and relative path.

Lexical classification can be implemented without canonicalizing the target,
because canonicalization would fail for broken but syntactically in-entry links
and would follow filesystem state. A helper can normalize path components
against the symlink parent:

```rust
fn symlink_target_escapes_entry(
    link_relative: &Path,
    target: &Path,
) -> bool
```

Rules:

1. If `target.is_absolute()`, return `true`.
2. Start from `link_relative.parent()` or the empty path.
3. Process each target component:
   - `.` does nothing;
   - normal components push;
   - `..` pops one component if possible, otherwise escapes;
   - root/prefix components escape or are treated as invalid for Unix paths.
4. If processing completes without popping above the entry root, return `false`.

The audit should report the link target text exactly as stored. It should not
rewrite, absolutize, or canonicalize the output.

### Documentation

Update `docs/workspace-portal.md` to list `audit symlinks` alongside
`audit hardlinks` and document that it reports symlink target text that escapes
entry targets.

Update `docs/security.md` to mention the audit as a hygiene check. The wording
should preserve the existing security model: escaping symlink target text is not
itself a host escape because daemon-side resolution is confined and
consumer-side resolution occurs in the consumer namespace.

## Non-goals

- Changing `PortalFs::readlink` behavior.
- Changing `readlink = false` or `--nosymfollow` semantics.
- Blocking symlink creation based on target text.
- Resolving symlink chains or proving full transitive filesystem reachability.
- Treating broken in-entry symlinks as findings.
- Adding JSON output in the first implementation.
- Generalizing the audit command beyond the concrete `symlinks` subcommand.

## Verification

1. Add a control-plane test for `workspace-portal audit symlinks <workspace>`
   against a stopped workspace state with one absolute escaping symlink and one
   relative escaping symlink. Assert the command exits non-zero and prints both
   entry-relative paths and stored targets.
2. Add a control-plane test where in-entry relative symlinks and broken
   in-entry symlinks are ignored. Assert the command exits `0` and prints the
   no-findings message.
3. Add a help test for `workspace-portal audit symlinks --help`.
4. Run `cargo test --test control_plane`.
5. Run `cargo clippy --all-targets --all-features -- -D warnings`.

No FUSE-specific test is required for the first implementation because the audit
operates on persisted workspace state and backing targets, not on live FUSE
behavior.

## Success criteria

- `workspace-portal audit symlinks <workspace>` exists and loads live or
  persisted workspace state.
- The audit reports absolute symlink targets.
- The audit reports relative symlink targets that lexically escape the entry
  root.
- The audit ignores symlink targets that remain lexically inside the entry root,
  even if the target is broken.
- The audit does not follow symlinked directories while scanning.
- Findings produce a non-zero exit status; no findings produce exit status `0`.
- Runtime symlink behavior remains unchanged.

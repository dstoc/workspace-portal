# Proposal: remove the `--follow-symlinks` / `--no-follow-symlinks` flags

## Motivation

`workspace-portal add` advertises a symlink-handling policy it does not have.
The `--follow-symlinks` and `--no-follow-symlinks` flags are parsed, validated
against each other, carried into `AddArgs`, and documented in the README — but
no code reads them. They are pure surface area: a knob wired to nothing.

A flag that looks like policy but changes nothing is worse than no flag. A user
who runs `add --no-follow-symlinks` reasonably believes they have prevented
symlink resolution; they have not. The honest move is to delete the knob until
there is a real policy behind it.

## Problem statement

The flags exist end to end but terminate in nothing:

- `AddCommand` declares both flags (`src/cli.rs:104` and `:110`).
- `validate_add` rejects passing both at once (`src/cli.rs:281`).
- `run` copies them into `AddArgs` (`src/cli.rs:217`).
- `AddArgs` stores them (`src/daemon.rs:57`).
- `daemon::add` (`src/daemon.rs:172`) never reads either field. It always calls
  `canonicalize_target` (`src/daemon/workspace.rs:141`), which runs
  `Path::canonicalize` — i.e. it *always* resolves the target through symlinks,
  regardless of the flags.

The design doc already admits this: "the flags are accepted by the CLI, but the
implementation still canonicalizes targets rather than providing a
differentiated symlink policy" (`docs/workspace-portal.md`, `add` section), and
lists it under §Known current limits.

The flags also conflate two unrelated things. They read as if they govern
symlink *traversal inside* an entry, but the only thing they could plausibly
affect is whether the *target argument itself* is canonicalized when the entry
is registered. Traversal confinement is a separate, real effort
(`docs/proposals/symlink-confinement.md`), and that proposal makes the entry
boundary unconditional — there is no supported mode where escaping symlinks are
followed. So even the target-canonicalization reading has no future home.

## Proposal

Delete the flags and their plumbing. `add` continues to canonicalize the target
argument exactly as it does today (`canonicalize_target`), which remains the
correct and only behavior: an entry target must be a real, existing host
directory, and storing its canonical path keeps state stable across later
symlink changes.

### Removals

- `src/cli.rs`: drop the `follow_symlinks` and `no_follow_symlinks` fields from
  `AddCommand` (`:104`–`:114`); drop the conflict check from `validate_add`
  (`:281`–`:285`); drop the two assignments in `run` (`:217`–`:218`).
- `src/daemon.rs`: drop the `follow_symlinks` / `no_follow_symlinks` fields from
  `AddArgs` (`:57`–`:58`).
- `README.md`: remove the `--follow-symlinks` / `--no-follow-symlinks` bullet
  from the `add` options list.
- `docs/workspace-portal.md`: remove the two flags from the `add` flag list
  (`:161`–`:162`), the "Current note on symlink flags" note (`:170`–`:173`), and
  the corresponding §Known current limits bullet (`:498`–`:499`).

`canonicalize_target` and the rest of the `add` path are unchanged.

### Behavior after removal

- `add <target> <name>` canonicalizes `<target>` and registers the canonical
  path, identical to today's default (neither flag set).
- The previously-valid `add --follow-symlinks ...` and
  `add --no-follow-symlinks ...` invocations now fail with clap's standard
  "unexpected argument" error.

## Non-goals

- Changing how the target argument is resolved. Canonicalization stays.
- Symlink *traversal* policy inside an entry — owned by
  `docs/proposals/symlink-confinement.md`, which makes confinement
  unconditional.
- Re-introducing a symlink policy flag later. If a genuine policy is ever
  needed, it can be proposed against real behavior at that time.

## Verification

1. `cargo build` succeeds with no `dead_code`/unused-field warnings related to
   the removed fields.
2. `workspace-portal add --help` no longer lists the two flags.
3. `workspace-portal add --no-follow-symlinks <t> <n>` exits non-zero with an
   "unexpected argument" message.
4. `workspace-portal add <t> <n>` still registers an entry whose stored target
   is the canonicalized path (existing control-plane test
   `control_plane_lifecycle_works_with_isolated_xdg_roots` continues to pass).
5. `grep -rn "follow_symlinks" src README.md docs` returns nothing.

## Success criteria

- No `follow_symlinks` / `no_follow_symlinks` identifiers remain in `src/`.
- The README and design doc no longer mention the flags.
- `add`'s observable behavior is unchanged from today's default path.

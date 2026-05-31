# Proposal: remove the hard-revocation knob (`rm --hard` / `--soft`)

## Motivation

`workspace-portal rm` advertises two revocation modes — soft and hard — but only
one exists. Soft revocation is the actual, implemented behavior: removing an
entry drops it from the namespace immediately while letting already-open file
handles continue. Hard revocation (forcibly tearing down open handles and
invalidating inodes) was never built. The `--hard`/`--soft` flags and the
`RevocationMode` they produce travel all the way to the daemon, which then only
logs the mode and does the soft thing regardless.

As with the `--follow-symlinks` flags, this is surface that promises a capability
the engine does not deliver. A user who runs `rm --hard` to force out a process
still holding a descriptor gets the soft behavior and no error. Until there is a
real hard-revocation implementation, the honest move is to remove the knob and
let `rm` mean exactly what it does: soft revocation, the only behavior.

## Problem statement

The mode is plumbed end to end and consumed nowhere meaningful:

- `RmCommand` declares `--hard` and `--soft` (`src/cli.rs:131`, `:134`).
- `validate_rm` rejects both at once (`src/cli.rs:290`).
- `run` maps them to `RevocationMode::Hard`/`Soft` (`src/cli.rs:227`).
- `RemoveArgs` carries `revocation` (`src/daemon.rs:65`); `daemon::remove`
  forwards it in `ControlRequest::Remove { name, revocation }` (`src/daemon.rs:208`).
- The wire protocol carries it (`src/protocol.rs:17`).
- The daemon handler receives it and *only interpolates it into a log message* —
  `format!("removed {} ({revocation:?})", removed.name)` (`src/daemon/runtime.rs:169`).
  The actual removal is `state.remove_entry(&name)` (`:165`), which is the same
  for both modes.
- `RevocationMode` itself (`src/state.rs:55`–`:66`) exists solely to feed this.

The soft behavior these flags pretend to modulate is genuinely implemented and
worth keeping: `getattr` serves attributes for a removed-but-still-open inode via
an `fstat` fallback (`src/fs/callbacks.rs:366`), and `read` deliberately operates
on the open handle without re-resolving through state
(`src/fs/callbacks.rs:1045`). That machinery is correct and stays. What is being
removed is the *choice* — there is only one behavior, so it needs no name and no
flag.

The design doc already records the gap: "`--hard` is parsed and carried through
the protocol surface, but there is no stronger hard-revocation implementation
yet" (`docs/workspace-portal.md`, `rm` section), and "no stronger
hard-revocation semantics" under §Known current limits.

## Proposal

Remove the revocation knob and the `RevocationMode` type. `rm` keeps its current
(soft) semantics as its only, unnamed behavior.

### Removals

- `src/cli.rs`: drop `--hard` and `--soft` from `RmCommand` (`:131`–`:135`);
  drop `validate_rm`'s conflict check (`:290`–`:295`) and its call site
  (`:223`); drop the `RevocationMode` mapping in `run` (`:227`–`:231`), passing
  only `mount_point`/`workspace` to `RemoveArgs`.
- `src/daemon.rs`: drop the `revocation` field from `RemoveArgs` (`:65`); build
  `ControlRequest::Remove { name }` in `daemon::remove` (`:208`).
- `src/protocol.rs`: change `Remove { name, revocation }` to `Remove { name }`
  (`:17`–`:20`); update the protocol round-trip test if it references
  `revocation`.
- `src/daemon/runtime.rs`: change the `Remove { name, revocation }` match arm to
  `Remove { name }` (`:161`); drop `revocation` from the ack message (`:169`),
  e.g. `format!("removed {}", removed.name)`.
- `src/state.rs`: delete the `RevocationMode` enum and its `Default` impl
  (`:55`–`:66`) once no longer referenced.
- `README.md`: remove the `--soft`/`--hard` options from the `rm` section.
- `docs/workspace-portal.md`: simplify the `rm` section (`:175`–`:191`) to state
  soft revocation as the behavior with no mode flags; update the protocol
  example that shows `"revocation":"soft"` (`:352`) to `{"op":"remove","name":"project-a"}`;
  remove the "no stronger hard-revocation semantics" limit bullet (`:496`) — or
  reword it as an explicit, intentional design choice rather than a missing
  feature.

### Wire-format note

`ControlRequest` is an ephemeral socket message between the CLI and the daemon,
never persisted; `PortalState` and `WorkspaceSnapshot` do not store revocation,
so there is no on-disk migration. The only compatibility surface is a
mixed-version pairing during upgrade: a new CLI sends `{"op":"remove","name":...}`
without `revocation`, which an *old* daemon (whose struct lacks a default for
`revocation`) would reject. Because the CLI and daemon are the same binary,
upgrading already means `stop` + `start`, which restarts the daemon. This
proposal does not add a compatibility shim; the documented upgrade step (restart
the daemon) covers it. (`serde_json` ignores unknown fields by default, so the
reverse pairing — old CLI, new daemon — already tolerates the extra field.)

## Non-goals

- Changing soft-revocation behavior. The namespace-drop + open-handle-survival
  semantics and their `getattr`/`read` fallbacks are unchanged.
- Implementing hard revocation. If forcibly closing open handles is ever wanted,
  it should be proposed as a real feature (with FUSE inode invalidation and
  tests) — not reintroduced as a flag first.
- Removing the `--lazy`/`--force` knobs on `stop`, which are unrelated and do
  have distinct behavior.

## Verification

1. `cargo build` succeeds with no unused-variant/dead-code warnings for
   `RevocationMode`.
2. `workspace-portal rm --help` no longer lists `--hard`/`--soft`.
3. `workspace-portal rm --hard <name>` exits non-zero with an "unexpected
   argument" message.
4. `workspace-portal rm <name>` still removes the entry and the entry no longer
   appears in `status` (existing
   `control_plane_lifecycle_works_with_isolated_xdg_roots` continues to pass).
5. An entry with an open read handle, when removed, still serves the in-flight
   read (existing soft-revocation E2E coverage,
   `fuse_e2e_soft_revocation_and_status_coherency_*`, continues to pass).
6. `grep -rn "RevocationMode\|revocation" src README.md docs` returns nothing
   (or only the reworded "intentional design choice" note).

## Success criteria

- No `RevocationMode` type and no `revocation` field remain in `src/`.
- `rm` removes an entry with no mode flags and unchanged soft-revocation
  behavior.
- The README and design doc describe `rm` as soft revocation only, without
  implying a hard mode exists.

# Proposal: make the `add`/`remove` transport-failure fallback verify the requested post-condition

## Motivation

When `workspace-portal add` or `rm` cannot get a response from the daemon, it
falls back to inspecting persisted state and may report success. The intent is
sound — absorb the race where the daemon *committed* a mutation but the response
was lost on the way back — but the check is too loose. It tests only whether an
entry with the given *name* is present (add) or absent (remove), ignoring
whether the entry actually matches what was requested. As a result the CLI can
report success for a change that never happened.

## Problem statement

`daemon::add` (`src/daemon.rs:192`):

```rust
match send_request(&ctx.socket, &request) {
    Ok(response) => ensure_response_ok(response),
    Err(err) => {
        let (_, persisted) = load_workspace_context(args.workspace)?;
        if persisted.entry(&mount_point).is_some() {
            Ok(())                 // <-- success if *any* entry of this name exists
        } else {
            Err(err)
        }
    }
}
```

`daemon::remove` (`src/daemon.rs:212`) is the mirror image, returning `Ok` when
`persisted.entry(&mount_point).is_none()`.

The fallback only runs when `send_request` returns a *transport* error
(connection refused, write failure, connection closed with no response, decode
failure). Protocol-level errors such as `EntryExists`/`EntryNotFound` arrive as
`Ok(ControlResponse::Error { .. })` and are handled by `ensure_response_ok`
(`src/daemon.rs:562`), not this branch.

The legitimate case the fallback exists for: `handle_request` persists state
(`src/daemon/runtime.rs:156`, `:167`) *before* `handle_connection` writes the
reply (`src/daemon/runtime.rs:128`). If the connection drops after the daemon
committed but before the client reads the response, `send_request` returns
`Err` even though the mutation succeeded and the registry already reflects it.
Surfacing that as failure would be wrong and would push users into confusing
retries (`add` → `EntryExists`, `rm` → `EntryNotFound`).

The bug: the fallback confirms the *name*, not the *requested state*. Concrete
failure — daemon unreachable, registry already holds `docs -> /old`:

```bash
workspace-portal add /new docs --replace   # transport fails
# fallback sees an entry named "docs" exists -> reports success
# but the registry still says docs -> /old; /new was never applied
```

The user is told the entry now points at `/new`; it does not. The remove side
has a softer version of the same flaw: it treats "name absent" as success even
when the entry never existed and the daemon was simply unreachable.

## Proposal

Keep the fallback, but make it succeed only when the persisted registry already
satisfies the *exact post-condition* the request asked for. Otherwise propagate
the original transport error.

### `add` post-condition

After a transport failure, reload the registry and return `Ok` only if it
contains an entry named `mount_point` whose `target` equals the canonicalized
request target **and** whose `mode` equals the requested mode:

```rust
Err(err) => {
    let (_, persisted) = load_workspace_context(args.workspace)?;
    match persisted.entry(&mount_point) {
        Some(entry) if entry.target == request_target && entry.mode == mode => Ok(()),
        _ => Err(err),
    }
}
```

`request_target` is the value already canonicalized at `src/daemon.rs:174`; the
daemon canonicalizes again in `handle_request` (`src/daemon/runtime.rs:150`), so
both sides store the same canonical path and the comparison is well-defined.
This makes the earlier `docs -> /new` example correctly fail: the post-condition
(`target == /new`) is not met, so the transport error propagates.

### `remove` post-condition

Removal's post-condition is simply "the entry is gone", which the current check
already expresses. The remaining concern is reporting success for an entry that
*never existed* when the daemon was merely unreachable. Tighten it so the
fallback only treats absence as success when the daemon was reachable at the
start of the call (the response was lost mid-flight), and otherwise reports the
transport failure:

- Gate the fallback on `socket_is_live(&ctx.socket)` having been true before the
  send (capture it once at the top of `remove`), mirroring how `status`/`edit`
  already probe liveness (`src/daemon.rs:227`, `:424`).
- With the daemon live-then-dropped, "name absent in registry" confirms the
  removal committed → `Ok`. With the daemon never reachable, return the
  transport error instead of inferring success from absence.

Applying the same liveness gate to `add` is reasonable for symmetry, but the
exact-match post-condition already eliminates `add`'s false positives, so the
gate is optional there.

### Accepted idempotent outcome

One benign case remains by design: `add` *without* `--replace` for an entry that
already exists with the identical target and mode. The daemon would answer
`EntryExists`, but if transport fails the post-condition (`target` and `mode`
match) is satisfied and the fallback returns `Ok`. Reporting "already in the
requested state" as success is acceptable idempotency and is preferable to a
spurious failure. This is noted explicitly so it is a chosen behavior, not an
oversight.

## Non-goals

- Removing the fallback entirely. The lost-response-after-commit race is real
  (state is persisted before the reply is written), and failing those calls
  would produce confusing retries.
- Changing the daemon's commit/reply ordering or adding request idempotency
  tokens. That is a larger protocol change; the post-condition check solves the
  observed problem without it.
- Any change to `send_request` or `ensure_response_ok` semantics.

## Verification

1. Start a daemon, add `docs -> /a`. Stop the daemon process abruptly (so the
   socket is dead). Run `add /b docs --replace`; confirm it now **fails** with a
   daemon-unreachable error instead of reporting success, and the registry still
   shows `/a`.
2. Add `docs -> /a` with the daemon live; confirm success and registry shows
   `/a` with the requested mode.
3. Simulate a lost response (e.g. a test daemon that commits then closes the
   socket without replying): confirm `add` of the committed entry returns `Ok`
   because the registry matches the post-condition.
4. With a dead daemon, `rm nonexistent`: confirm it reports the transport
   failure rather than success.
5. With a live-then-dropped daemon after a committed remove: confirm `rm`
   returns `Ok` (entry absent, daemon was reachable).
6. Existing `control_plane_lifecycle_works_with_isolated_xdg_roots` continues to
   pass.

A unit-level test is feasible by factoring the post-condition check into a pure
helper (`fn add_postcondition_met(persisted: &PortalState, name, target, mode) -> bool`)
and testing it against constructed `PortalState` values, alongside the existing
state tests in `src/state.rs`.

## Success criteria

- `add` over an unreachable daemon reports success only when the persisted entry
  exactly matches the requested name, target, and mode.
- `add` that would change an existing entry's target/mode reports the transport
  failure when the daemon is unreachable, instead of false success.
- `rm` over a never-reachable daemon reports the transport failure; `rm` whose
  removal committed before the response was lost still reports success.
- No regression in the daemon-live happy paths.

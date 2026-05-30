# Proposal: stop forcing a full fsync in `flush`

## Motivation

`flush` fires on every `close(2)` of a file descriptor. Development workloads
close enormous numbers of files in bursts: `cargo` and `rustc` emit thousands of
object and intermediate files, `npm`/`pnpm` unpack tens of thousands of small
files, and bundlers rewrite large trees. If each close forces a synchronous,
durable flush to disk, those workloads pay one `fsync` per file and stall on I/O
that the caller never asked to be durable.

POSIX `close()` does not promise durability, and neither does FUSE `flush`.
Durability is the job of `fsync`/`fdatasync`, which this codebase already
implements separately. Forcing it in `flush` is both unnecessary and a
significant, silent throughput tax on exactly the build/install workflows the
portal targets.

## Problem statement

`fn flush` in `src/fs/callbacks.rs:1075` calls `handle.file.sync_all()` for
every writable handle:

```rust
Some(handle) if handle.writable => match handle.file.sync_all() {
    Ok(()) => reply.ok(),
    ...
```

`sync_all()` is `fsync(2)` — it flushes file data *and* metadata to the backing
device. Doing this on every close means a `cargo build` that closes N output
files performs N synchronous `fsync`s, serialized through the single FUSE
request path, regardless of whether anything needed to be durable.

Meanwhile real durability requests are already handled correctly elsewhere:
`fn fsync` (`src/fs/callbacks.rs:1109`) calls `sync_data()`/`sync_all()` based on
the `datasync` flag, and `fn fsyncdir` exists too. Tools that need durability
(editors doing atomic-save, databases) already issue an explicit `fsync` before
`close`, and that path is untouched by this proposal.

## Proposal

Make `flush` a non-durable operation: validate the handle and return success
without calling `sync_all()`.

### New behavior

```rust
fn flush(&self, _req, _ino, fh, _lock_owner, reply) {
    let runtime = self.runtime.lock().unwrap();
    match runtime.handles.get(&fh.0) {
        Some(_) => reply.ok(),
        None    => reply.error(Errno::EBADF),
    }
}
```

- A known handle (writable or not) returns `ok` immediately.
- An unknown handle still returns `EBADF`, preserving the current contract that
  `flush` validates the descriptor.

Because writes are already passed straight through to the backing file on each
`write` call (`src/fs/callbacks.rs:1069`, `write_at`), there is no portal-side
buffer that `flush` needs to drain. The data is already in the host page cache;
`flush` returning without `fsync` simply lets the host's normal writeback policy
apply, exactly as it would for a direct write to the backing directory.

### Durability is unchanged for callers that ask for it

`fsync`/`fdatasync` through the mount continue to map to `sync_all`/`sync_data`
via the existing `fn fsync`. Any tool relying on durability (atomic save =
`write` + `fsync` + `rename`) keeps the same guarantees. The only thing removed
is implicit, unrequested durability on plain `close`.

## Non-goals

- Changing `fsync`/`fsyncdir` semantics. Explicit durability requests stay
  fully synchronous.
- Adding write buffering or write-back batching inside the portal. Writes remain
  pass-through; this proposal only removes a redundant sync.
- Adding a configuration knob to restore fsync-on-flush. If a durability-on-close
  mode is ever wanted it can be proposed separately with a concrete use case;
  there is none today.

## Verification

1. Mount a read-write entry over a scratch directory.
2. Write a file, `close` it (no explicit `fsync`), and read it back through the
   mount and on the host path; confirm contents match.
3. Write a file, call `fsync` explicitly, and confirm it still succeeds
   (`fn fsync` path unchanged).
4. Create-and-close many files in a loop (e.g. 5000) and confirm completion time
   drops substantially versus the fsync-on-flush behavior, with no data loss in
   a subsequent read-back.
5. Confirm `close` of a stale/unknown handle still surfaces `EBADF` where
   applicable.
6. Run the full E2E suite: `./scripts/fuse-e2e-podman.sh`.

The existing
`fuse_e2e_file_lifecycle_covers_create_append_overwrite_truncate_and_fsync`
test already exercises write/close/fsync correctness and must continue to pass;
extend it (or add `fuse_e2e_flush_does_not_require_fsync`) to assert that a
plain write+close round-trips without an explicit fsync.

## Success criteria

- `flush` no longer calls `sync_all()`.
- Plain write-then-close round-trips correctly through the mount.
- Explicit `fsync`/`fdatasync` through the mount still flush durably.
- Bulk create-and-close workloads are no longer serialized behind a per-file
  `fsync`.
- All existing tests continue to pass.

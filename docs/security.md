# Security design: containment of container-exposed mounts

## Purpose and primary goal

`workspace-portal` exposes selected host directories through a single, stable
FUSE mount. Its intended deployment is container-based development: the workspace
path is bind-mounted into a container so that code running there sees a curated
set of project directories, added and removed dynamically without restarting the
container.

The **primary security goal is containment**:

> A consumer that can reach the mount — typically a process inside a container —
> must only be able to read and write within the host directories explicitly
> exposed as entry targets. It must not be able to use the mount to reach host
> paths outside those targets.

Everything below is organized around that goal: what the boundary is, how it is
enforced, and — just as important — what it deliberately does *not* promise.

## Actors and trust

- **Daemon** — runs on the host as one uid. Trusted. Performs all file I/O with
  that uid's privileges; it does **not** drop privileges to the caller. Holds the
  live entry set and serves the FUSE mount.
- **Control plane** — the CLI talking to the daemon over a Unix socket. Trusted
  and owner-only; it defines and mutates the entry set.
- **Consumer** — code that accesses the mount, e.g. a process in a container.
  Treated as **potentially adversarial with respect to containment**: it may try
  to read or write outside the exposed targets, including by planting symlinks or
  racing the backing store.
- **Backing store** — the host directories behind the entries. May change
  concurrently, whether through the mount or by other host processes.

## Assets

1. Host files and directories **outside** the exposed entry targets
   (confidentiality and integrity).
2. Integrity of the entry set / control plane (only the owner may decide what is
   exposed).

## The containment boundary

Containment rests on two independent mechanisms — one provided by the kernel, one
enforced by the daemon — plus lexical validation as defense in depth.

### 1. Symlink resolution happens in the consumer's namespace (foundation)

The daemon does **not** follow symlinks on the consumer's behalf. When a lookup
hits a symlink, the daemon reports it as a symlink and returns its target bytes
via `readlink` (`src/fs/callbacks.rs`); the **consumer's kernel** then resolves
that target, in the **consumer's mount namespace**, against the **consumer's
root**.

Consequence: an absolute or `..`-climbing symlink inside an entry resolves
relative to the *container's* root, not the host's. A container reading
`/work/data/escape/etc/hostname`, where `escape -> /`, reads the *container's*
`/etc/hostname`; it cannot reach the host's. This is a property of Linux VFS path
resolution (symlink targets are resolved against `current->fs->root`), not
something the daemon must actively enforce, and it is why a plain passthrough of
symlinks does not leak host files across the container boundary.

This mechanism only holds if the consumer is in a **separate mount namespace**
that does not itself include the host paths in question — i.e. a real container.
For a same-namespace consumer see "Non-goals" below.

The editable workspace `readlink` policy changes whether this resolution step
is available through the portal. Symlink inode visibility does not change, but
`readlink = false` makes the FUSE `readlink` callback fail with `ELOOP`, which
blocks both target-text disclosure and traversal through that symlink.

### 2. The daemon resolves host paths confined beneath the entry target

Where the daemon *itself* resolves a host path (for `open`, `stat`, `readdir`,
create/remove/rename, etc.), it must never step outside the entry target — even
if the backing store is mutated between when an inode was cached and when the
daemon acts on it (a TOCTOU race). Otherwise the daemon could be tricked into
reading an out-of-entry host file and serving its bytes back across the namespace
boundary.

This is enforced in `src/fs/safe_open.rs`: every host operation resolves an
entry-relative path against a pinned directory fd for the entry root using
`openat2(2)` with `RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS`, and leaf
create/remove operations use the `*at` syscalls against a confined parent dir fd.
The kernel fails the resolution (`EXDEV`) the instant any component — a symlink
swapped in by a racing writer, a `..`, or a `/proc` magic link — would leave the
entry root. Symlinks that stay *within* the entry still resolve, so legitimate
in-entry links keep working.

The optional `workspace-portal start --nosymfollow` flag is separate from this
daemon-side confinement. It changes the mount-wide traversal policy: symlink
inodes remain visible and `readlink` still works when the workspace `readlink`
policy is true, but path traversal through symlink components is disabled by
the mount where supported. It is not a per-entry policy and it is not
persisted in entry or workspace state.

Properties:

- **TOCTOU-safe.** The entry root is opened `O_PATH | O_DIRECTORY | O_NOFOLLOW`
  (fails closed if the root itself was swapped to a symlink) and pins the inode;
  subsequent resolution is relative to that fd and cannot be redirected by a path
  swap.
- **Fail-closed.** On a kernel without `openat2` (< 5.6) the operations error
  rather than falling back to an unconfined `join` + std-fs path. There is no
  mode in which escaping resolution is permitted.
- **Verified.** `safe_open` unit tests assert `EXDEV` on escaping symlinks and
  `..`, and that in-entry symlinks still resolve;
  `fuse_e2e_backing_store_swap_stays_confined_to_entry` exercises the held-fd
  race end to end. See `docs/proposals/symlink-confinement.md` for the full
  rationale.

### 3. Lexical path validation (defense in depth)

Before any resolution, portal paths are parsed by `parse_portal_path`
(`src/fs/path.rs`), which rejects `..`, `.`, embedded root components, NUL bytes,
and non-UTF-8 entry names. Entry names are additionally validated
(`paths::validate_entry_name`) to reject `/`, `..`, and separators. This blocks
the obvious traversal attempts independently of the kernel-level confinement.

### 4. Read-only entries

Entries may be read-only. Writable opens and all mutating operations are rejected
for read-only entries and for a read-only-default workspace
(`ensure_writable_entry`, `read_only_default`). This is enforced by the daemon
regardless of the file's own mode bits.

### 5. Immutable segments are path-policy protection

Workspace state may declare immutable segment names such as `.git` or `.jj`.
When a mutating operation targets an entry-relative path containing one of those
segments, the daemon rejects it with `EPERM`. This is enforced by
`ensure_mutable_relative_path` and applies to fresh writable opens, create,
mkdir, symlink, unlink, rmdir, rename, truncate, metadata mutation, the write
side of copy operations, and hard-link endpoints.

Hard links are deliberately allowed for compatibility with build tools, but only
within a single read-write entry and only when both endpoint paths are mutable.
For example, linking `target/a.o` to `target/a.copy.o` is allowed, while linking
`.git/config` to `target/config-alias` or linking `target/config` to
`.git/config-alias` is rejected.

This is a **path-policy** guarantee, not a full inode-integrity guarantee. If a
backing inode already has aliases under both immutable and mutable paths, writes
through the mutable alias can still affect the immutable path's contents. The
runtime does not scan for or block those aliases on every write; use
`workspace-portal audit hardlinks <workspace>` to find visible hard-link groups
that cross immutable and mutable portal paths, and use
`workspace-portal audit symlinks <workspace>` as a hygiene check for symlink
target text that lexically escapes an entry. That symlink audit is diagnostic
only: escaping target text is not itself a host escape because daemon-side
resolution stays confined beneath the entry target and consumer-side resolution
still happens in the consumer's namespace.

## Control-plane authorization

The entry set — *what is exposed* — is the other asset. The control socket is
created `0600` and its runtime/state directories `0700`
(`src/daemon/runtime.rs`), so only the owning uid can connect to it. A consumer
in a container that has only the mount bind-mounted cannot reach the control
socket and therefore cannot add, remove, retarget, or flip entries. Changing what
is exposed is an owner-only operation.

## Threats and mitigations

| Attempt by a consumer | Outcome |
| --- | --- |
| `..` in a path through the mount | Rejected lexically at parse; also `EXDEV` at resolution |
| Absolute / escaping symlink inside an entry, followed by the consumer | Resolved in the consumer's namespace → reaches the consumer's own filesystem, never the host's |
| Race the backing store: swap an in-entry component to a symlink that escapes, then drive a cached inode | Daemon resolution is `RESOLVE_BENEATH` → `EXDEV`; no out-of-entry host data served |
| `/proc`-style magic-link traversal during daemon resolution | Blocked by `RESOLVE_NO_MAGICLINKS` |
| Write to a read-only entry | Rejected (`EROFS`/`EPERM`) |
| Hard-link from `.git`/`.jj` or another immutable segment to a mutable path, or into an immutable segment | Rejected (`EPERM`) when either endpoint path contains an immutable segment |
| Alter the exposed entry set from inside the container | Control socket is `0600`/owner-only; unreachable |
| Old kernel without `openat2` | Fail-closed: operations error rather than resolve unconfined |

## Non-goals and explicit limitations

These are deliberate. Stating them is part of the design.

- **The mount is not a sandbox for a same-namespace, same-uid consumer.** A
  process sharing the daemon's mount namespace and uid can already read those
  files directly; the mount grants it nothing extra. Containment is meaningful
  against a consumer confined to a *separate* namespace (a container), which is
  the target deployment.
- **No privilege separation.** The daemon performs I/O as its own uid and does
  not consult the caller's uid. This matters specifically for `allow_other` (see
  Residual risks).
- **Symlink *content* is not confined.** A consumer with write access to an entry
  can create a symlink pointing anywhere. This is safe because the daemon never
  follows it out of the entry (mechanism 2) and the consumer's own kernel
  resolves it within the consumer's namespace (mechanism 1) — only daemon-side
  *resolution* is confined, not what a link records.
- **Not a defense against a compromised daemon or a malicious entry target.** If
  the owner exposes a sensitive directory, it is exposed; containment is about not
  exceeding the *declared* targets, not about vetting them.

## Residual risks

- **`allow_other` without `default_permissions` (confused deputy).** With
  `--allow-other`, the mount is reachable by other local users, but the daemon
  still does I/O as its own uid and the kernel is not asked to check the
  displayed mode/owner against the caller (no `default_permissions`, no `access`
  callback). A lower-privileged user could then reach files through the mount
  that they could not open directly. This is the one outstanding gap;
  `docs/proposals/default-permissions-with-allow-other.md` proposes the fix
  (enable `MountOption::DefaultPermissions` when `allow_other` is set). It does
  not affect the default (owner-only) mount.
- **Unbounded daemon-side growth (local DoS).** The inode cache and open-handle
  maps grow with lookups/opens and shrink only on `forget`/`release`; a consumer
  issuing many lookups can grow daemon memory. Not a containment breach.
- **Pre-existing hard-link aliases across immutable boundaries.** Immutable
  segments are enforced by endpoint path, not by global inode ownership. A hard
  link created outside the portal, or created before a segment became immutable,
  can make the same inode visible under both an immutable path and a mutable
  path. Normal writes through the mutable path are not runtime-blocked. Run
  `workspace-portal audit hardlinks <workspace>` to detect visible aliases of
  this kind.
- **Escaping symlink target text inside an entry.** A symlink whose stored
  target text points outside the entry is not a host escape by itself, because
  daemon-side resolution remains confined and consumer-side resolution happens
  in the consumer namespace. Run `workspace-portal audit symlinks <workspace>`
  to spot this hygiene issue before enabling symlink traversal for a workload.
- **Environmental dependencies.** `chmod`/`utimens` confinement uses
  `/proc/self/fd` on a pinned `O_PATH` fd (safe, but requires `/proc` mounted in
  the daemon's namespace); confinement requires Linux ≥ 5.6 for `openat2` and is
  fail-closed otherwise.

## Deployment guidance (container exposure)

To get the intended containment when exposing the mount into a container:

1. Run the consumer in its **own mount namespace** that does **not** otherwise
   include the host paths you are protecting; bind-mount only the workspace into
   it. This is what makes mechanism 1 meaningful.
2. Do **not** bind-mount or otherwise expose the control socket / runtime
   directory into the container; keep entry management owner-only on the host.
3. Avoid `--allow-other` unless `default_permissions` is in effect (see Residual
   risks). For a single-user dev box the default owner-only mount is the safe
   choice.
4. Expose the narrowest entry targets that the workload needs, read-only where
   possible.
5. Run on a kernel with `openat2` (≥ 5.6) so daemon-side confinement is active.

## Verification

The containment invariant is checked at two levels:

- **Unit (deterministic, no FUSE):** `src/fs/safe_open.rs` tests assert escaping
  symlinks and `..` resolve to `EXDEV`, and in-entry symlinks still resolve.
- **End to end:** `fuse_e2e_backing_store_swap_stays_confined_to_entry`
  (`tests/fuse_e2e.rs`) holds an open handle, swaps an in-entry component for an
  escaping symlink, and asserts the daemon never serves metadata for a path
  outside the entry; the broader FUSE suite covers normal and in-entry-symlink
  behavior.
- **Hard-link policy:** `fuse_e2e_hard_link_rejects_immutable_source_and_destination`
  checks that hard-link creation fails when either endpoint path is under an
  immutable segment. `audit_hardlinks_reports_crossing_immutable_and_mutable_aliases`
  checks that the CLI reports visible pre-existing aliases that cross immutable
  and mutable paths.

Run the full FUSE suite on a FUSE-capable host:

```bash
cargo test --test fuse_e2e -- --ignored --test-threads=1
```

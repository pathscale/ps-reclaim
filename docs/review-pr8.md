# Source-only follow-up to PR #7

This branch is stacked on `perf/handle-api`, including its updated head
`460688aced65a407412382654b7007cdc6c1aa73`. It is best-effort source, not
validated HFT software. No compilation, formatter, tests, benchmarks, Miri or
Loom runs were performed. Parent PR #7 enabled CI for stacked PRs during this
work; the workflow now explicitly skips compiled jobs while `fix/reclaim-review`
is a draft. Other PRs are unaffected. Do not mark this PR ready or remove that
guard until compilation is authorized. Skipped checks are not validation.

The parent update independently improved the benchmark and restricted native
TLS to Unix. This branch retains that restriction and extends the benchmark
with reader readiness, pre-release timing, persistent cross-arm registrations,
counterbalanced warmups, exact destructor counts, and in-burst advancement.

## Reclamation cutoff

Previously a reclaimer scanned participants and then extracted from the live
garbage vector. A new reader could pin after its slot was scanned; a retirement
published after that scan could have the same epoch as earlier garbage and be
freed immediately despite that reader.

Each queued retirement now receives a monotonically increasing sequence under
the garbage mutex. Advancement snapshots the next sequence under that same
mutex before its SeqCst fence and participant scan. Extraction requires both
`sequence < cutoff` and an expired epoch. Consequently the scan can only free
garbage published before its fence, even when other retirement/advance calls
interleave or remove entries. A vector length would not be a stable cutoff.
The existing publication/scan fences are retained; this is not a substitute
for a memory-model proof of the entire algorithm.

The fix adds eight bytes of sequence metadata per retirement and a checked
increment in the existing retire critical section. Closure boxing moves before
the mutex acquisition, though vector growth and extraction allocation can still
occur under it. Sequence exhaustion panics before enqueueing instead of wrapping.
Only 40 epoch bits fit in a published pin, so advancement saturates at that
ceiling. At saturation a reader in the domain conservatively blocks queued
retirements until a quiescent scan. Pin's atomic publication path is unchanged.
Advancement now pays a compare-exchange update instead of a wrapping fetch-add.
The registration diagnostic counter also saturates rather than wrapping into
already-leased exclusive indices; that check is confined to slot acquisition.

`advance_up_to` bounds callback count, not lock wait, scan work, allocation, or
the time any callback takes. Writers are not lock-free or bounded-latency.

## Registration API and lifetime

Replace `domain.handle()` / `handle.pin()` with:

```rust
let registration = ps_reclaim::Handle::new(); // once per worker
let domain = ps_reclaim::Domain::new();
let guard = domain.pin_with(&registration);
// Load and use protected pointers here, while both borrows remain live.
drop(guard);
```

A registration can pin any number of domains over its lifetime, with four
normal concurrent pins and counted wildcard overflow. It and its guards remain
non-Send/non-Sync. The guard borrows both registration and domain; it remains
two words, so returned-view ABI costs still need target measurements. Implicit
TLS and explicit registrations are independent and each consumes a slot.

Lease teardown closes a native cache before recycling. Registration is rejected
after native TLS teardown instead of publishing a pointer backed by a destroyed
lease. Windows no_std keeps both cache and owner together in FLS, even with the
nightly feature; a per-thread cache must not outlive a per-fiber owner. Stable
Unix key-based TLS also keeps both together.

An exclusive slot with any live pin is never recycled, including forgotten
guards and guards stored in older TLS destructors. This deliberately leaks a
registration in exceptional teardown cases rather than allowing a new owner to
erase protection. The late guard can clear its publication, but the abandoned
slot is not recovered. Repeated exceptional teardown can exhaust exclusive
capacity. Guard drop does not initialize platform TLS and only clears a cached
mask if its participant still matches, avoiding damage to a recreated cache.

## Regression sources added, not executed

- Deterministic post-scan retirement interleaving through a private no-op hook.
- Epoch saturation with a held reader.
- Saturated registration counter cannot reissue an exclusive slot.
- One handle visiting 512 domains; overlapping and out-of-order guard drops.
- Independent TLS and explicit protection; forgotten guard ownership.
- Nested wildcard overflow and panic unwinding.
- Two independent shared-overflow handles; dropping one cannot erase the other.
- A guard in older TLS whose destructor can run after the registration lease.
- Compile-fail documentation for Send/Sync and both borrowed lifetimes.

The overflow suite is isolated; tests sharing a process with a temporary
wildcard are serialized. These are deterministic/ordinary regression sources,
not a Loom model or evidence that weak-memory interleavings were exhaustively
checked. Windows fiber deletion/switching and every TLS teardown configuration
still need dedicated execution. Panic-abort configurations require their own
validation plan.

## Before production use

When compilation is authorized, validate std, std+spin-garbage, Unix no_std
keys, Unix no_std nightly, and Windows no_std (with and without nightly).
Exercise doctests, regression tests, concurrent retirement/scanning, registration
recycling, Miri-compatible paths, and an actual reduced-memory-model test.
Review saturation/progress behavior and teardown leaks against deployment
uptime and worker lifecycle. Then measure real returned views, writer tails,
contention/preemption and allocator behavior on the deployment CPU. See
`tls-cost.md` for the revised benchmark's limits; the old speed claims are not
treated as validation of this branch.

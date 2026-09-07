# TLS and explicit registrations: evidence and limits

## Status of the numbers from PR #7

The earlier M4 Max results are historical, exploratory measurements of the
old harness, not measurements of this branch. That harness timed new thread
creation, registration and teardown; measured each arm in a fixed block order;
leaked its domain; and never called `advance`. Its no-op retirements occurred
60 times per 4,000-update burst, not on every update. Cold-start and scheduling
effects cannot be separated from pin-path effects in those results.

| Historical configuration | TLS throughput, M updates/s | Handle throughput, M updates/s |
| --- | --- | --- |
| std | 17.4 / 21.8 / 24.5 / 18.4 | 16.6 / 20.7 / 23.5 / 18.3 |
| no_std, pthread keys | 8.7 / 15.1 / 14.4 / 13.0 | 17.6 / 31.2 / 30.6 / 19.6 |
| no_std, nightly attribute | 18.4 / 23.4 / 24.3 / 17.3 | 17.7 / 22.2 / 23.2 / 16.5 |

These observations motivate a controlled experiment. They do not establish that
`no_std` is generally half as fast, that a handle fully recovers the loss, or
that a spin mutex explains the difference between handle builds. Changing
`std` also changes registration and initialization machinery, TLS teardown,
lock implementation, and code generation. No causal conclusion follows from
the pin source being identical. The historical inline-TSD negative result
does not isolate those other effects either.

## What the revised harness measures

`benches/burst.rs` now uses eight persistent workers per reader-think setting,
with both registration paths initialized before timing. Six warmup rounds
precede 24 measured rounds. Each round measures TLS, handle and a repeated TLS
control in one of all six arm orders. Every arm is first, middle and last
equally often. The workers, domain and atomic book are reused across arms.

A ready barrier excludes worker setup. A separate start barrier releases the
workers. Drain time ends at the maximum writer completion timestamp, before
report delivery or thread teardown. CPU is process CPU from just before release
until all reader reports arrive: it is a different window from drain time.
Reader work depends on scheduling and think time, so its count is reported.
It is not valid to describe CPU differences as per-operation savings for
identical work. Some very short bursts may finish before a reader runs.

Each writer retires one owned allocation every 64 updates and immediately calls
`advance_up_to(8)`. An observable counter verifies all 60 callbacks per burst
have run after the quiescent final drain. Raw rows now also report callbacks
completed by the last writer and the pending count before cleanup: their sum
must equal the per-burst retirement count. Final cleanup has separate wall/CPU
measurements, plus an end-to-end wall interval through cleanup (including
observer accounting and reader shutdown). Equal update counts do not imply
equal reclamation work inside the writer-only window. The retired payload is not the atomic book and is
never published to a reader: this exercises reclamation work, not pointer
safety. Correctness tests cover protection separately.

Update samples include clock overhead, the pin, atomic update and periodic
retirement/reclamation. The harness includes allocator costs, barrier skew,
channel/report overhead in the CPU window, and OS scheduling. It does not pin
threads to cores, control frequency/NUMA, or represent a production feed.
P99.9 of pooled synthetic updates is not an HFT tail-latency guarantee.

Raw CSV-prefixed rows are emitted only after all workers join, preserving round,
arm position, counts and timing pairs before aggregate sorting. Matching TLS
controls can rule out some order effects, but share code layout, participant
placement and scheduling confounders. They do not identify a TLS mechanism or
establish that handles have more predictable production latency.

## Isolating the garbage mutex

The opt-in `spin-garbage` feature changes only the domain garbage-list mutex in
a std build; std TLS, registry free-list mutex, and OnceLock remain unchanged.
This provides a narrower comparison than switching the whole crate to no_std.
It is an experiment control, not a recommended HFT configuration. Spinning
under contention or holder preemption can make tails worse; extraction scans
an unbounded queue and can allocate under the lock.

Commands for a later, explicitly authorized validation run (not executed for
this source-only change):

```sh
cargo bench --bench burst
cargo bench --bench burst --features spin-garbage
cargo bench --bench burst --no-default-features --features libc,spin
cargo +nightly bench --bench burst --no-default-features --features libc,spin,nightly
```

Alternate complete process/configuration order too; within-process arm ordering
does not control cross-build drift. Record the exact SHA, compiler, flags,
allocator, OS, CPU, topology, placement and frequency settings, plus raw
per-burst results before drawing a conclusion. Repeat on the deployment target.
No new performance results have been collected for this branch.

## API cost still matters

Construct one `Handle::new()` per worker and pass it to `domain.pin_with(&handle)`
across domains. Do not construct one per read or per table. Each handle and
each implicit TLS registration consumes a separate slot; 255 are exclusive,
then a shared wildcard pin delays reclamation in all domains.

`HandleGuard` occupies two words while the TLS `Guard` occupies one. A returned
view containing a guard and a reference may change its calling convention and
spill behavior. Real no-inline returned-view workloads, not just local pin/drop
loops, still need measurement and generated-code inspection. The branch makes
no claim of equivalent results between native TLS and explicit handles.

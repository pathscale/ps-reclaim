# Design notes

Decisions that are not obvious from the code, and the reasoning behind them.
Kept because most of them were expensive to reach, and one of them is a design
that was tried, found unsound, and must not be re-proposed.

## The garbage queue

A `VecDeque`. Extraction pops from the front, appends ineligible records to the
back, and stops at the callback limit or once every initially queued record has
been inspected exactly once. The sequence cutoff and the strict epoch comparison
both remain. That combination handles **out-of-order retirement epochs** without
assuming eligibility is FIFO — two threads can read epoch `E` and `E+1` and push
in the other order, so a prefix assumption would be wrong.

**Callback order is deliberately unspecified.**

This exists because bounded drains used to be quadratic: `extract_if(..).take(k)`
compacts the unchecked tail on drop, so draining `N` in batches of `k` moved
about `N²/k` records under the garbage mutex. Measured at Arctic's
`advance_up_to(256)`, a 128,000 backlog cost **23.29 ms; it now costs 1.28 ms**,
and the ratio against an unbounded drain is 1x at every size instead of climbing
to 53x.

**What it still does not bound:** scans over blocked records, allocator latency,
lock acquisition, or destructor duration. Queue growth and extracted-batch growth
can both allocate while holding the mutex, and the extracted batch now carries
retirement metadata rather than bare closures, so it is larger than before.

## Callback panics

If an invoked callback unwinds, the batch owner returns every **uninvoked**
callback to the queue with its original sequence and epoch. The invoked one is
not retried. Requeueing may allocate.

Domain destruction cannot restore work into a dying domain: remaining captures
are dropped **without** invoking their callbacks. So the contract is simply that
**cleanup callbacks should not panic.**

## Epoch lifetime

The epoch saturates at 40 bits rather than wrapping, because wrapping was
unsound. At a million epoch-incrementing scans per second that is roughly a
**12.7-day** lifetime, which is an operational constraint, not a bug.

- `epoch_headroom()` makes the remaining increments observable.
- `renew_epoch()` takes `&mut Domain`, so live guards and concurrent operations
  exclude it by construction. In an exclusive maintenance window it resets the
  epoch to one and marks existing retirements epoch zero without invoking them,
  leaving their sequence numbers alone, so later readers cannot delay that old
  work. It is `O(pending)` metadata work and keeps the queue's capacity.

**This does not make an indefinitely-active domain rollover-safe.** A deployment
that cannot arrange an exclusive window needs a different representation or a
domain-replacement strategy. Treat it as an open gate, not a solved problem.

### A rejected design, recorded so it is not re-proposed

A transparent modular epoch — decode packed low bits relative to a full epoch,
cap advancement by the oldest scanned pin — is **unsound**, by this schedule:

1. A reader samples an epoch, then pauses before publishing, for a packed period.
2. An advance scans its still-idle slot, then pauses before updating the clock.
3. The reader publishes its now-ancient low bits, fences, and reads the old root.
4. A writer unlinks and retires the root; the earlier advance increments the
   clock, letting those ancient low bits alias a newer epoch.
5. A later scan mistakes that live reader for newer than the retirement.

The age cap derived from step 2 cannot constrain a pin the scan never saw.

## Pin publication ordering, and a disagreement worth recording

The store that publishes a pin is `Relaxed`, paired with the `SeqCst` fence on
the next line. That is what `master` shipped and what the loom models drive.

**It was changed to `Release` and then changed back, and the argument for
`Release` is worth keeping** because it is not obviously wrong:

> A scanner observing a repin thus acquires accesses preceding it, without
> relying on a same-thread relaxed store extending a release sequence. The
> paired SeqCst fences remain; `Release` alone does not close store buffering.
> This is a stronger ordering, not a zero-cost claim: **measure code generation
> and returned-view latency on the deployment CPU.**

It was reverted on exactly the measurement that argument asked for. On aarch64 a
`Release` store is `stlr` rather than `str`, on a path Arctic takes for every
lookup, and through `wt-benchmarks` the reclamation cost went **1.18 → 9.33 ns**
at t1 and **1.45 → 8.58 ns** at t8, isolated against the benchmark's own no-op
SMR control.

So: the fences carry the protocol, `Relaxed` is sufficient for it, loom agrees,
and the stronger ordering costs four to six times on the real workload. If the
release-sequence argument is ever shown to matter for a case the fences do not
cover, this is the note to reopen — with a failing model first.

## What the tests reach, and what they do not

- **AtomicPtr publication** uses both TLS and explicit guards, repeated
  repinning, registration after the start barrier, and two concurrent
  reclaimers. The published root has no mutex, so nothing supplies the ordering
  the fences are supposed to. Deferred atomic poison detects early reclamation
  without executing a dangling access.
- **Continuous-reader progress** asserts completion and backlog *before*
  releasing the last reader. Its channel synchronisation is deliberate for a
  progress test and is **not** weak-memory evidence.
- **Nested wildcard** lives in its own integration-test process so it cannot
  stall unrelated progress assertions.
- **Windows FLS** switches between separately pinned parent and child fibers,
  deletes the quiescent child, and repeats. It has to actually run on Windows,
  with and without the nightly feature, and it never has.
- **`ps_loom`** models two registrations, repinning, root unlinking, cutoff
  extraction and two reclaimers, with a reduced saturating epoch range. It
  mirrors the protocol rather than substituting loom atomics through the whole
  crate. **Not modelled:** TLS/FLS, wildcard aggregation, allocation, teardown,
  exclusive renewal. Default bound is two preemptions and 20,000 permutations.

Neither tool is proof. [Loom documents incomplete C11
coverage](https://github.com/tokio-rs/loom#unsupported-features) and [Miri
documents incomplete weak-memory
exploration](https://github.com/rust-lang/miri). Mutation-check production
changes, not just the litmus copies — a check that cannot fail is worse than
none, and this repository has shipped several.

## Registration quarantine

An exclusive lease whose pins have not all cleared enters a quarantine list.
Cold registration takes ordinary free slots first, then recovers a quarantined
slot whose wildcard and normal pins all read idle under `Acquire`. Once a lease
is relinquished its owner can publish nothing further — only remaining guards
can release — so that monotonic check is sufficient for reuse. **No slot is ever
cleared to make it reusable**, and guard drop gains no load, RMW, allocation,
lock or callback for this.

A genuinely forgotten guard still holds protection and consumes capacity.
`registry_stats()` reports quarantined, still-pinned quarantined, and available
exclusive slots, as a cold diagnostic snapshot rather than a reservation.

**Miri's allocation-leak check cannot see this class of leak.** The slots stay
reachable through a globally rooted registry; what is lost is free-list
capacity. Direct capacity assertions are the only thing that catches it.

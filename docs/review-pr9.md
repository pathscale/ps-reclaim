# PR #9: whole-source audit follow-up

Source-only, best effort. Based on PR #7 head
`2827b65fab7a769811372176f0233b49322e467b` on `perf/handle-api`, which already
contains the earlier PR #8 work. The existing `fix/reclaim-review` branch was
advanced to the initial parent and rebased again as PR #7 advanced; inherited
work is not duplicated. The parent formatting, validation fixes, manual-only
expensive workflows, small Loom handshake model and opt-in inline-TSD experiment
are retained. No compilation, formatter, test, benchmark, Miri or
Loom execution was performed for this follow-up. Parent results do not validate
these changes. All compiled workflow jobs skip this branch until explicitly
authorized; skipping is not passing.

## Queue: bounded batches no longer compact the unchecked tail

The garbage queue is now a `VecDeque`. Extraction pops from the front, appends
ineligible records to the back, and stops at the callback limit or after each
initially queued record has been inspected once. Sequence cutoff and strict
epoch comparisons remain. This handles out-of-order retirement epochs without
assuming FIFO eligibility. Callback order is deliberately unspecified.

Draining a fully eligible N-record backlog in k-record batches no longer moves
N-k, N-2k, ... unchecked records. Each removal is a ring-queue pop. A source
regression checks that the first unchecked record keeps its address after a
bounded drain. This does not bound scans of blocked records, allocator latency,
lock acquisition or destructor duration. Both queue growth and extracted-batch
growth may allocate under the mutex. The extracted batch now retains retirement
metadata, not just closures; it is larger than before. No speedup is measured.

If an invoked callback unwinds, a batch owner returns every uninvoked callback
to the queue with its original sequence and epoch. The invoked callback is not
retried. Requeueing may allocate, and domain destruction still cannot restore
work into a dying domain. Its panic contract is explicit: remaining captures
are dropped without invoking the callbacks. Cleanup callbacks should not panic.

## Epochs: safe maintenance, not an unverified rollover protocol

The 40-bit saturation safety backstop remains. At one million epoch-incrementing
scans per second, its roughly 12.7-day lifetime remains an operational constraint.
`epoch_headroom()` makes the remaining increments observable. `renew_epoch()`
requires `&mut Domain`, so safe active guards and concurrent operations exclude
renewal. During an exclusive maintenance window it resets the epoch to one and
marks existing retirements epoch zero without invoking them. Their sequence
numbers are unchanged. Future readers therefore cannot delay that old work.
The operation takes O(pending) metadata work and preserves allocated queue
capacity. For Arc-owned domains, obtain genuine exclusive ownership first.

This mitigation does NOT make an indefinitely active domain rollover-safe.
Deployments unable to arrange exclusive maintenance still need a different
representation/protocol or a domain replacement strategy. Do not ignore this
remaining HFT deployment gate.

A transparent modular-epoch draft was rejected during source review. Merely
decoding packed low bits relative to a full epoch and capping advancement by
the oldest scanned pin is insufficient:

1. A reader samples an epoch, then pauses before publishing for a packed period.
2. An advance scans its still-idle slot and pauses before updating the clock.
3. The reader publishes its ancient low bits, fences and reads the old root.
4. A writer unlinks and retires the root; the earlier advance increments the
   clock, allowing the ancient low bits to alias a newer epoch.
5. Another scan can then mistake that live reader for newer than its retirement.

An age cap derived from step 2 cannot constrain a pin it did not see. That
design was removed, not shipped as a source-only speculative safety fix.

Normal pin publication is now Release, in both APIs. A scanner observing a
repin thus acquires accesses preceding it, without relying on a same-thread
relaxed store extending a release sequence. The paired SeqCst fences remain;
Release alone does not close store buffering. This is a stronger ordering,
not a zero-cost claim: measure code generation and returned-view latency on
the deployment CPU. Epoch advancement uses one CAS, without retrying to
increment on behalf of a stale scan; the API documents attempted advancement.

## Registration: cold-path quarantine recovery

An exclusive lease whose pins have not all cleared enters a quarantine list.
Cold registration first uses ordinary free slots, then recovers a quarantined
slot whose wildcard and normal pins all read idle with Acquire ordering. Once
the lease is relinquished there can be no new publications by that owner, only
remaining guard releases, so this monotonic check is sufficient for reuse.
No slot is cleared to make it reusable. Guard drop gains no new load, RMW,
allocation, lock or callback to perform this recovery.

A genuinely forgotten guard still retains protection and consumes capacity.
`registry_stats()` reports quarantined, still-pinned quarantined and available
exclusive slots. It is a cold diagnostic snapshot, not a reservation. Miri's
allocation-leak checking cannot detect lost indices in a globally rooted
registry; direct capacity assertions are required.

The teardown regression now repeats the late-guard lifecycle beyond registry
capacity, checks early-free protection, and asserts all exclusive capacity is
available after each join. A static OnceLock owns its domain instead of an
unreachable leaked Box, so this fixture no longer needs `-Zmiri-ignore-leaks`.
An isolated registry unit test also verifies that a remaining wildcard prevents
reuse after the normal pin has cleared.

## What the new test sources do and do not cover

- AtomicPtr publication stress uses both TLS and explicit guards, repeated
  repinning, registration after the start barrier, and two concurrent reclaimers.
  The published root has no mutex. Storage remains allocated until readers join;
  deferred atomic poison detects early reclamation without deliberately executing
  a dangling access on a broken implementation.
- Continuous-reader progress uses controlled overlapping handovers and asserts
  completion/backlog before releasing the last reader. Channel synchronization
  is intentional for this progress test, not weak-memory evidence.
- Nested wildcard testing lives in a separate integration-test process so it
  cannot stall unrelated progress assertions. It releases ordinary pins first,
  leaving the overflow wildcard as the only protection of its distinct domain.
- A real Windows no_std fiber test switches between separately pinned parent and
  child fibers, then deletes the quiescent child and repeats. It must actually
  run on Windows, both with and without the nightly feature.
- The opt-in `ps_loom` configuration now includes the parent's small handshake
  model plus `loom_protocol`, using instrumented atomics, mutexes and a checked
  non-atomic payload. It models two registrations, repinning, root unlinking,
  cutoff extraction and two reclaimers with a reduced saturating epoch range.
  It mirrors the protocol rather than substituting Loom atomics into the entire
  crate. TLS/FLS, wildcard aggregation, allocation, teardown and exclusive renewal
  are not modeled. Default coverage is bounded to two preemptions and 20,000
  permutations; environment settings can enlarge it.
- Separate Loom store-buffering litmus tests contain missing-reader-fence and
  missing-scanner-fence negative controls. They are expected to fail internally;
  those outcomes have NOT been observed for these unexecuted sources.

Even executed models would not constitute full proof: [Loom documents incomplete
C11 coverage](https://github.com/tokio-rs/loom#unsupported-features), and
[Miri documents incomplete weak-memory exploration](https://github.com/rust-lang/miri).
Run mutation checks against production changes too, not just the litmus copies.

## Benchmark and CI changes

The burst harness checks timed callbacks plus residual pending work against
retirements before cleanup. It reports separate cleanup wall/CPU time and an
end-to-end completion interval. Raw rows preserve round and arm position instead
of relying only on independently sorted medians. Matching TLS controls does not
identify a mechanism or establish production predictability. No affinity,
frequency, NUMA or migration control is added; no new performance claim is made.
Returned-view and real feed measurements remain necessary.

The automatic workflow retains the parent's small build/test/lint scope.
Expensive checks remain manual and select one job per dispatch. The manual
sources cover std+spin-garbage, Unix keys/native TLS, macOS inline TSD with an
arm64 assertion, Windows std/FLS, doctests, Miri and both reduced models. Platform
checks select one OS per dispatch instead of multiplying runner count. The
inherited inline-TSD experiment still relies on an undocumented ABI; CI is not a
platform guarantee for it. Every compiled job is gated off for
`fix/reclaim-review`. Only remove that gate after authorization.

Future validation, NOT executed here: run the workflow matrix, confirm positive
and negative model results, mutation-check the actual pin/scan fences and cutoff,
exercise callback panic/reentrancy and slot capacity, then measure large blocked
backlogs and complete returned views on deployment hardware. A passing source
whitespace check is not a Rust typecheck or a concurrency result.

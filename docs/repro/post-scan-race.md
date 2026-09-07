# A falsifiable test for the pre-existing post-scan race

Status: source written and inspected; **not compiled or executed**. The results
below are predictions from the source, not observed test output. The previously
added callback-only test remains; this adds real thread scheduling, a published
allocation, an actual destructor, and a way to test the original implementation.

## The test

`src/domain/post_scan_race.rs` contains
`post_scan_retirement_must_not_destroy_a_pinned_object`. It uses the ordinary
TLS `Domain::pin`, not the new handle API. A reader's slot is registered before
the scan, so the schedule does not rely on registration/high-water-mark races.

The reclaimer runs on a separate scoped thread. The test thread performs both
reader and unlinking-writer operations while retaining the reader guard. That
is legal: pin, load, unlink, retire, and continue holding the pin. Channels
enforce the interleaving; there are no sleeps or probabilistic racing loops.
Thirty-second receive timeouts only turn a broken handshake into a failure.
The pause represents a legal preemption point in the original code. Added
channel synchronization cannot repair a scan result that is never refreshed.

| Step | Reclaimer | Reader / unlinking writer |
| --- | --- | --- |
| 1 | Not started | Queue seed garbage at epoch 1, with no held pin. |
| 2 | Scan all slots; record `min_pinned = 2`; pause before extraction. | Wait for scan-complete message. |
| 3 | Paused | Pin at epoch 1 and load/dereference the published payload. |
| 4 | Paused | Unlink the payload and retire its owning Box at epoch 1; retain the guard. |
| 5 | Resume extraction and return. | Wait for the reclaimer to finish, still pinned. |
| 6 | Finished | Record destructor count and queue length before dropping the guard. |

The old predicate is only `retirement_epoch < min_pinned`. The newly retired
payload has epoch 1, and the already-completed scan recorded 2. Thus the old
code extracts both seed and payload, calling the payload's destructor while
the reader's guard is still live. This interleaving requires no weak-memory
reordering: the stale *scan result* is enough.

With the fix, the scan's sequence cutoff is 1, while the subsequently queued
payload receives sequence 1. The additional `sequence < cutoff` condition is
false, so only the seed is extracted. After the reader unpins, another advance
must destroy the payload and leave an empty queue.

The test deliberately never dereferences the pointer after reclamation resumes.
It detects early destruction through `Payload::drop` incrementing a separately
owned atomic. It therefore does not need a crash, allocator address reuse, or
intentional use-after-free to expose the violation. The raw allocation may leak
on a harness failure before unlinking; the ordinary success/failure comparison
cleans up before asserting the captured safety observation.

## Expected distinguishing outcomes — not run

| Observation before guard drop | Original implementation | Cutoff fix |
| --- | --- | --- |
| Payload destructor count | 1 | 0 |
| Queued retirements | 0 | 1 |
| First advance's callback count | 2 | 1 |
| Safety assertion | FAIL | PASS |

Both should eventually record exactly one payload destruction and an empty
queue after unpin/cleanup. The distinguishing failure message is:

> retired object was destroyed while its reader guard was still live

If a future run does not produce this distinction, investigate that outcome;
this document does not turn an unexecuted test into empirical proof.

## Checking the exact pre-fix implementation

`post-scan-race-before-fix.patch` targets PR #7's exact revision
`460688aced65a407412382654b7007cdc6c1aa73`. It adds:

- The identical regression-test file used by the fixed branch.
- A private `advance_with` wrapper with a no-op callback on the public path.
- A scheduling callback immediately after the existing participant scan.

It does **not** change the original scan, retirement, epoch update, extraction
predicate, or locking. This tests the actual pre-fix reclaimer, not a separate
handwritten model of the suspected bug. A `git apply --unidiff-zero --check`
text check was performed against source extracted from that revision, and the two test files were
compared byte-for-byte. Those are text checks only.

For a later authorized run, use a disposable **local** checkout of that exact
revision and apply the patch with `git apply --unidiff-zero` (the patch has
zero context so it does not embed whitespace-only context lines). Do not push
the reproduction to PR #7: its CI would compile. Run the same selector on the
patched old checkout and PR #8:

```sh
# NOT executed for this source-only request. Requires permission to compile.
cargo test --lib domain::post_scan_race::post_scan_retirement_must_not_destroy_a_pinned_object -- --exact
```

The old checkout should fail the safety assertion; the fixed checkout should
pass. This only addresses this specific interleaving. It is not an exhaustive
memory-model proof or validation of the other changes in PR #8.

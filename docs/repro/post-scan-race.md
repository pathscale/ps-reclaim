# Reproducing the post-scan race

`advance_up_to` scans the participants, computes the oldest epoch anyone is
pinned in, and only then locks the garbage list to take its batch. Nothing holds
a lock across those two steps, so a retirement published in between was judged
by a decision made before it existed and could be freed under a live reader.
Pre-existing on `master`; closed here by a sequence cutoff captured under the
garbage mutex before the scan.

## Making it fail

Two tests cover it, both in-tree and both passing. To confirm they have teeth,
delete the cutoff from the drain predicate in `src/domain.rs`:

```rust
// around line 349
if retirement.sequence < cutoff && retirement.epoch < min_pinned {
//                    ^^^^^^^^^^^^^^^^^^^^^^^^^ remove this clause
```

Then:

```sh
cargo test --lib post_scan
```

Both fail immediately, deterministically:

```
domain::post_scan_race::post_scan_retirement_must_not_destroy_a_pinned_object
  panicked at src/domain/post_scan_race.rs:99
domain::tests::a_post_scan_retirement_waits_for_its_reader
  panicked at src/domain.rs:804
```

Restore the clause and both pass. Verified on this branch.

## The two tests, and why there are two

`domain::tests::a_post_scan_retirement_waits_for_its_reader` is single-threaded
and drives the schedule through the `advance_with` seam, so it is fast and has
no timing in it at all.

`domain::post_scan_race` is the heavier one: real threads, a published
`AtomicPtr`, an actual destructor, and channels enforcing the interleaving
rather than sleeps or racing loops. It uses the ordinary TLS `Domain::pin`, not
the handle API, and registers the reader's slot before the scan so the schedule
does not depend on a registration race.

## Note on an earlier version of this file

This document used to ship a 130-line `.patch` reconstructing the pre-fix source,
and described the test as "not compiled or executed" with "predictions from the
source, not observed test output". Both are gone: the patch duplicated a file
already in the tree to rebuild a state git can already produce, and the tests
have since been compiled, run, and shown to fail without the fix. The one-line
edit above replaces all of it.

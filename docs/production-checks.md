# Checking this crate before you put it on a feed

Baseline report for PR #7 before the PR #9 source-only follow-up. Preserve the
measurements below as historical evidence, not validation of the new source.
PR #9 changes the queue, quarantine recovery, test isolation, fixture lifetime,
model coverage and manual workflow commands. Its new sources have NOT been run;
see [the current follow-up notes](review-pr9.md) for coverage and limitations.
In particular, registration-capacity loss is not detected by Miri's allocation
leak check, and the new static-owned teardown fixture needs no ignore-leaks flag.

What to run before deploying, what each check proves, and — more usefully —
what each one does not. Nothing here runs in CI: CI builds, tests and lints, and
everything below is deliberately manual, because these are questions you ask
when you are about to trust the crate, not on every push.

Timings are from an M4 Max, quiet machine.

## The short version

```sh
cargo test                                                        # 35 tests,  ~1.4s
cargo test --no-default-features --features libc,spin             # 35 tests,  ~6s
RUSTFLAGS="--cfg ps_loom" cargo test --release --test loom_model  # 3 models,  <1s
cargo +nightly miri test --lib --test concurrency --test contract \
  --test handle_overflow --test handles --test overflow_slots     # ~7 min
MIRIFLAGS="-Zmiri-strict-provenance -Zmiri-ignore-leaks" \
  cargo +nightly miri test --test tls_teardown                    # seconds
```

Run the whole set before a deployment. Run the loom model whenever the pin or
advance path changes, because it is the only check that speaks to weak memory
ordering and it costs under a second.

## 1. The loom model — the one that matters on aarch64

```sh
RUSTFLAGS="--cfg ps_loom" cargo test --release --test loom_model
# deeper, still fast:
RUSTFLAGS="--cfg ps_loom" LOOM_MAX_PREEMPTIONS=5 cargo test --release --test loom_model
```

Loom explores thread interleavings and memory orderings exhaustively under the
C++11 model. That is what a weakly ordered machine exposes and what x86 hides,
so **this is the check that speaks to a Graviton deployment**, and it does so
from any machine — you do not need ARM hardware to find an ARM bug.

Three models, and two of them are meant to fail:

```
both_fences_present_is_safe ........................ ok
without_the_pin_fence_a_live_object_is_reclaimed ... ok  (should_panic)
without_the_scan_fence_a_live_object_is_reclaimed .. ok  (should_panic)
```

Both fences are parameters so the model is *shown* to fail without them. A check
that cannot fail proves nothing, and two checks in this repository already made
that mistake: see section 5.

**Covers:** one reader and one reclaimer, one participant slot — the fence in
`pin` against the fence in `advance`.

**Does not cover:** the sequence cutoff, registration growth, the wildcard slot,
out-of-order guard drops, more than one reader, or `Handle`/`pin_with`. Extending
it is cheap now the harness exists, and the cutoff is the first thing to add.

`loom` is a dev-dependency only under `cfg(ps_loom)`, so an ordinary build never
fetches it.

## 2. Miri — undefined behaviour, not ordering

Two passes, because one test has to leak and the rest must not:

```sh
cargo +nightly miri test --lib --test concurrency --test contract \
  --test handle_overflow --test handles --test overflow_slots
MIRIFLAGS="-Zmiri-strict-provenance -Zmiri-ignore-leaks" \
  cargo +nightly miri test --test tls_teardown
```

`tests/tls_teardown.rs` needs a `&'static Domain` that outlives thread-local
destructors, so it leaks by construction. Do **not** turn leak checking off
globally to paper over that: this crate hands registry slots out and back, and a
leaked slot is exactly what the check exists to catch.

Miri needs the component on the toolchain you invoke, not on the pinned one:
`rustup +nightly component add miri`.

**Covers:** provenance, aliasing, uninitialised reads, use-after-free on the
paths those tests reach.

**Does not cover:** weak memory ordering on real hardware. Miri models the C++
memory model rather than a chip, and its exploration is not exhaustive. It is
not a substitute for section 1.

## 3. The alternate builds

CI runs default features only. These are what consumers actually select:

```sh
cargo test --no-default-features --features libc,spin
cargo test --no-default-features --features libc,spin,spin-garbage
cargo test --features spin-garbage
cargo test --no-default-features --features libc,spin,inline-tsd
```

`inline-tsd` is worth running anywhere, not just macOS: everywhere else it must
fall back to the libc call and keep working, and that fallback is what this
proves.

Not covered anywhere: **Windows FLS storage and its teardown**. If you ship on
Windows, that path has never been executed.

## 4. Known flake, and it will bite a full run

`tests/concurrency.rs::a_thread_does_not_reclaim_under_its_own_pin` fails
roughly **1 to 3 times in 12**, on every configuration measured. Because a
failing target aborts the run, a full `cargo test` then reports 9 tests instead
of 35 and looks like a coverage difference. It is not.

It asserts a retirement is reclaimed once its own guard drops, which the design
does not promise while any thread is pinned through the overflow slot's
wildcard — that counter is deliberately not domain-filtered, where the `pins`
array is. Seven tests in one binary reach the overflow slot.

Re-run before believing a failure. It wants a private slot or a weaker
assertion, and until then it makes every full-suite result unreliable.

## 5. What none of these checks reach

Two tests look like they cover things they do not, both found by review rather
than by failing:

- **`a_reader_never_observes_a_reclaimed_payload`** publishes through
  `Arc<Mutex<Arc<AtomicUsize>>>`. The reader's unlock synchronises with the
  writer's lock, which orders the pin ahead of the scan by itself. **That test
  cannot fail for want of a fence.** An `AtomicPtr` publication test with no
  auxiliary synchronisation is still missing.
- **`reclamation_progresses_under_continuous_readers`** stops and joins the
  readers, calls `advance()` eight times, and only then asserts. An
  implementation that reclaims nothing under traffic and drains afterwards
  passes it. **Progress under load is untested**, and its failure mode is an
  overnight OOM rather than a latency blip.

## 6. Known production risks, with their triggers

Ranked by what actually bites, not by severity in the abstract.

**Bounded drains are quadratic.** `extract_if(..).take(limit)` compacts the
unchecked tail on drop, so draining a backlog of N in batches of k moves about
N²/k entries, all under the garbage mutex. Measured, at Arctic's
`advance_up_to(256)`:

| backlog | k=256 | unbounded | ratio |
|---:|---:|---:|---:|
| 8,000 | 0.20 ms | 0.07 ms | 3x |
| 32,000 | 3.00 ms | 0.25 ms | 12x |
| 64,000 | 8.32 ms | 0.46 ms | 18x |
| 128,000 | 23.29 ms | 0.44 ms | 53x |

The trigger is concrete: `Map::range` hands its guard to the iterator, so a range
scan pins for the whole traversal while writers retire. Backlog is roughly
retire-rate times hold-time. **A ten-second scan at 8,000 retirements/sec builds
~80,000 and costs ~10 ms inside the mutex**, against a 2–10 ms burst window. With
`spin-garbage` every other writer burns a core waiting through it. If your reads
are point lookups this never fires.

**Registration exhaustion.** A lease released while a guard is still live
abandons its exclusive slot, and nothing returns it. 255 of those and every
domain falls to the shared wildcard, which stops reclamation process-wide. Needs
thread churn with live guards, so a process with long-lived pinned threads never
sees it. Note that **Miri's leak check does not catch this** — the slot stays
reachable through the global registry, and what is lost is free-list capacity.

**Epoch saturation is not your problem.** The epoch advances only when a scan
finds garbage, and Arctic advances once per 256 retirements. At 8,000
retirements/sec that is ~31 epochs/sec, so 2⁴⁰ is roughly **1,100 years**. Even
at a million retirements/sec it is about 9 years. A restarted process never
approaches it.

**A panicking callback drops the rest of its batch** without running them. If
your retirements are plain deallocation this cannot fire; if they can panic and
you recover from unwinding, queued cleanup disappears.

# What dropping `std` costs the pin path, measured

`Domain::pin` takes three thread-local lookups and no lock. With `std` those are
`thread_local!`; without it they are `pthread_key_t` slots. This is what the
substitution costs, end to end, on the workload that matters rather than on a
microbenchmark of `with()`.

Everything here was run on an M4 Max, aarch64 `apple-darwin`, release with LTO
and one codegen unit.

## The headline

**A pin goes from 3.28 ns to 7.00 ns without `std`**, and one Arctic lookup
contains exactly one pin. An Arctic `get` over 100,000 `u64` keys costs 7.84 to
7.97 ns, so the pin is about 42% of a read.

**An earlier version of this document turned that into "roughly 47% on every
Arctic read". Withdraw that number.** It came from one sample of each arm, and
the benchmark it came from has since been shown to swing that arm 2x run to run:
the same configuration read 28.97 and 17.84 on consecutive runs. The ratio was
about right and the confidence was not.

What replaced it is a benchmark that carries its own null, in `benches/burst.rs`
here, and the numbers below are from that. The fold and the handle at the end of
this document are both measured on it.

## The pin itself

Two builds of this crate, `--features std` against
`--no-default-features --features libc,spin`, each printing ns per pin. Two
builds means two processes, so the null is not optional: the first block is the
**same build** run alternately, which is the floor below which nothing is a
result.

```text
                  1-thread   nested   4-thread   8-thread
  null, run A       3.294     3.359     3.411      3.346
  null, run A'      3.270     3.319     3.317      3.355
  null, run A       3.269     3.392     3.346      3.368
  null, run A'      3.274     3.338     3.371      3.360

  std               3.284     3.236     3.320      3.357
  no_std            7.004     6.063     7.512     33.931
  std               3.253     3.229     3.328      3.343
  no_std            7.067     6.146     8.844     29.324
  std               3.262     3.152     3.332      3.354
  no_std            6.989     5.900    19.481     44.889
```

Single-threaded the result is clean: **3.27 against 7.00, a 2.1x increase**,
against a null that moves by 0.03. Nested pins, which is the shape a tree
traversal produces, go 3.2 to 6.0.

**The 4- and 8-thread columns are not a result and should not be quoted.** The
`no_std` arm there is unstable across runs, 7.5 to 19.5 and 24.8 to 44.9, and
the cause is not established. What is established is that it is not the
mechanism scaling badly, which the next section shows.

## The primitive does not degrade with thread count

Three lookups per iteration either way, matching what `pin` does, timed on the
slowest thread:

```text
  threads   thread_local!   pthread keys   ratio
        1            0.56           8.60    15.4x
        2            0.32           5.45    17.3x
        4            0.29           4.94    16.9x
        8            0.30           5.35    17.6x
```

The ratio is flat. `pthread_getspecific` is not a contention point, so whatever
makes the contended `pin` numbers above unstable is somewhere else, and is worth
finding before anyone reads a scaling story into them.

**That ratio overstates the case and is here only for its flatness.** At 0.3 ns
for three lookups the `thread_local!` arm has been collapsed by the optimiser,
which is legitimate for it and impossible for an opaque `pthread_getspecific`
call. The honest figure for the cost is the end-to-end 2.1x above, where both
arms do the same real work.

## The fix, measured, and then done

Three of the four thread-locals on this path (`MINE`, `SHARED`, `PIN_MASK`) have
no destructor and hold nine bytes between them. One lookup instead of three:

```text
          three lookups   one lookup   saved
    r1           5.25         1.69     3.56 ns
    r2           4.54         1.65     2.89 ns
    r3           4.56         1.64     2.92 ns
```

**2.9 to 3.6 ns against a regression of 3.7**, so folding them recovers nearly
all of it. That fold has since landed, and the full picture is now this, ns per
pin+drop, standalone, `#[inline(never)]`:

```text
                        pthread keys   #[thread_local]   std thread_local!
  unfolded, 4 lookups      5.97-6.07         3.44-3.48           3.40-3.57
  folded, 2 lookups        3.29-3.45         1.74-1.85           1.77-1.78
```

The fold is about 1.8x in every column and the mechanism about 1.75x in both
rows: independent, and together 6.0 to 1.8.

## And a third option that beats both

A `Handle` the caller carries, instead of a registration the pin looks up. Pin
plus drop is 1.75 ns on stable `no_std`, which is where `std` and the nightly
attribute land. On the burst benchmark, four reader think times, M updates/s,
with each block's null in brackets:

```text
  std                  thread-local 17.4 21.8 24.5 18.4   handle 16.6 20.7 23.5 18.3
  no_std, keys         thread-local  8.7 15.1 14.4 13.0   handle 17.6 31.2 30.6 19.6
  no_std, attribute    thread-local 18.4 23.4 24.3 17.3   handle 17.7 22.2 23.2 16.5
```

Nothing where thread-locals are already cheap; **2.05, 2.12, 2.12 and 1.50x on
stable `no_std`**. Three caveats travel with that number and should not be
dropped: with `READERS = 0` it is 1.2x, so the 2x is readers starving writers of
CPU rather than each write getting faster; reading the TSD slot with inline asm
saves a similar 1.3 ns per pin and changes *nothing* here, which means the
handle is probably winning by skipping `Tls::with`'s lazy-init path rather than
its lookup; and the handle on `no_std` is 1.4x faster than the handle on `std`,
which the pin path cannot explain and which is most likely `spin::Mutex` beating
`std::sync::Mutex` on the retire path.

## What this does not say

One machine, one libc, one operation. On ELF the gap should be **wider**, not
narrower: local-exec makes a `const`-initialised `thread_local!` a register read
plus an offset with no call at all, while `pthread_getspecific` stays a call.
Measure again on Linux before relying on the 2.1x.

## Reproducing

Two crates that both `#[path]`-include one body, differing only in how they
depend on this one:

```toml
# arm A
ps-reclaim = { path = "..." }
# arm B
ps-reclaim = { path = "...", default-features = false, features = ["libc", "spin"] }
```

with a body that pins `ITERS` times and divides, run alternately so the machine
drifts through both arms equally, and the same build run against itself first to
establish the floor.

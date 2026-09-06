# What dropping `std` costs the pin path, measured

`Domain::pin` takes three thread-local lookups and no lock. With `std` those are
`thread_local!`; without it they are `pthread_key_t` slots. This is what the
substitution costs, end to end, on the workload that matters rather than on a
microbenchmark of `with()`.

Everything here was run on an M4 Max, aarch64 `apple-darwin`, release with LTO
and one codegen unit.

## The headline

**A pin goes from 3.28 ns to 7.00 ns, and one Arctic lookup contains exactly one
pin.** An Arctic `get` over 100,000 `u64` keys costs 7.84 to 7.97 ns, so the pin
is 42% of a read today and dropping `std` would add roughly 47% to every read.

That is not a cost to absorb quietly, and it is also mostly recoverable: see the
fold at the end.

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

## The fix, also measured

Three of the four thread-locals on this path (`MINE`, `SHARED`, `PIN_MASK`) have
no destructor and hold nine bytes between them. They could be one struct behind
one lookup:

```text
          three lookups   one lookup   saved
    r1           5.25         1.69     3.56 ns
    r2           4.54         1.65     2.89 ns
    r3           4.56         1.64     2.92 ns
```

**2.9 to 3.6 ns, against a regression of 3.7.** Folding them recovers nearly all
of it, which would put the `no_std` pin within about a nanosecond of the `std`
one and take the Arctic read penalty from ~47% to under 10%.

This is a change to the hot path of a lock-free crate. It belongs in its own
change with its own measurement, and it is the thing to do before anyone ships a
`no_std` build of Arctic.

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

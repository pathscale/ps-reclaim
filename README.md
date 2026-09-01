# ps-reclaim

Grace-period reclamation for lock-free readers, with a stated contract.

A reader loads a pointer from a shared slot and dereferences it without a
lock. A writer unlinks that pointer and wants the memory back. This decides
when that is safe.

## The contract

Three guarantees, one test each in `tests/contract.rs`:

1. A retirement does not run while a reader that predates it is live.
2. **A reader that starts after a retirement does not delay it.** Under
   continuous read traffic there may never be an instant with zero readers, and
   a scheme that waits for one never reclaims at all.
3. Reclamation only happens in `advance()`. A read never pays for garbage.

Guarantee 2 is why this exists. `seize` and a plain global reader counter do
not provide it.

## Cost

Pin cost, M4 Max, `cargo bench --bench pin`, ns/op:

| readers | ps-reclaim | crossbeam-epoch | seize |
| --- | --- | --- | --- |
| 1 | **1.42** | 1.92 | 2.30 |
| 2 | **1.43** | 3.25 | 2.32 |
| 4 | **1.43** | 5.72 | 2.35 |
| 8 | **1.45** | 9.00 | 2.39 |
| degradation | 1.02x | 4.7x | 1.04x |

`crossbeam-epoch` runs a global collect every 128 pins, and that walk grows
with the reader count, so its read path is not flat. `seize` is flat and
cheapest but fails guarantee 2. This is flat and keeps guarantee 2.

Read the numbers before changing anything here.

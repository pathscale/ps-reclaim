# ps-reclaim

Grace-period reclamation for lock-free readers, with a stated contract.

A reader loads a pointer from a shared slot and dereferences it without a
lock. A writer unlinks that pointer and wants the memory back. This decides
when that is safe.

## The contract

Three guarantees, one test each in `tests/contract.rs`:

1. A retirement does not run while a reader that predates it is live.
2. Readers in strictly later epochs do not delay older retirements. Same-epoch
   readers can delay them, even if they started after retirement. Wildcard pins
   delay every domain. At 40-bit epoch saturation, new retirements require a scan
   without matching pins. Monitor `epoch_headroom()` and arrange exclusive
   maintenance with `renew_epoch()` before saturation.
3. Reclamation is driven by `advance()` / `advance_up_to()` and by domain
   destruction. Read-side unpin never invokes retirement callbacks.

Unlike a plain global reader counter, normal operation can reclaim while readers
overlap continuously. This statement is not a comparative result for other
reclamation libraries. Epoch renewal is not transparent rollover: it requires
`&mut Domain`, excluding active guards and concurrent operations. At one million
epoch-incrementing scans per second, a fresh domain has about 12.7 days of
headroom. Callback order is unspecified; cleanup should not
panic. See `Domain::retire` for unwind behavior.

`advance_up_to` bounds callbacks, not total scan time, allocator work, lock wait,
or callback duration. Retire/advance are not lock-free writer operations.
Exclusive registrations abandoned with outstanding guards are quarantined until
those guards drop, then recovered on a subsequent cold registration path.
Truly forgotten guards continue to consume capacity. Use `registry_stats()` for
cold-path capacity diagnostics, not allocation-leak reports.

## Cost

Historical pin cost, M4 Max, old `cargo bench --bench pin`, ns/op. These are not
measurements of this source-only follow-up or an HFT latency guarantee:

| readers | ps-reclaim | crossbeam-epoch | seize |
| --- | --- | --- | --- |
| 1 | **1.42** | 1.92 | 2.30 |
| 2 | **1.43** | 3.25 | 2.32 |
| 4 | **1.43** | 5.72 | 2.35 |
| 8 | **1.45** | 9.00 | 2.39 |
| degradation | 1.02x | 4.7x | 1.04x |

The old harness did not establish simultaneous worker readiness and included
first registration. Its comparison also includes each library's adapter costs.
Do not infer scalability or causal explanations from this table alone. See
[measurement limits](docs/tls-cost.md) and the [PR #9 source notes](docs/review-pr9.md).

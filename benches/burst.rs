//! What a thread-local lookup costs a pin, under a burst rather than in a loop.
//!
//! # Why this exists, and why it is not `benches/pin.rs`
//!
//! `pin.rs` measures a pin in a tight loop. That is the right shape for
//! comparing reclamation schemes and the wrong one for comparing *lookup
//! mechanisms*, because a loop lets the optimiser hoist a `thread_local!`
//! address out of it and never lets it hoist an opaque `pthread_getspecific`.
//! Measured that way, the answer is a statement about the optimiser.
//!
//! This is the burst shape a market-data feed produces, deliberately the same
//! as `parking_lot_lite_hack`'s `benches/burst.rs` so the two read against each
//! other: 200 symbols by 20 levels rewritten per burst, four writers each
//! owning a contiguous shard, four readers querying throughout, twenty bursts.
//! What decides whether a feed keeps up is how fast a burst drains and how bad
//! the worst update in it is, so that is what is reported.
//!
//! # The two arms
//!
//! Identical in every respect but the pin: same sharding, same key order, same
//! counts, same latency sampling, same branch. One pins through `Domain::pin`,
//! which finds this thread's registration in thread-local storage; the other
//! through a `Handle`, which was handed it. **Whatever they differ by is what
//! the lookup costs**, and that depends entirely on the build:
//!
//! ```text
//! cargo bench --bench burst
//! cargo bench --bench burst --no-default-features --features libc,spin
//! ```
//!
//! With `std` a thread-local is an offset from the thread pointer and the two
//! arms are the same, so the handle is not worth its API cost. Without `std` it
//! is a `pthread_getspecific` call and the handle is roughly twice the
//! throughput at a third of the CPU.
//!
//! The book is a flat `Vec<AtomicU64>`, not a map: these arms are not measuring
//! a data structure. Every read and every write happens under a pin, which is
//! the traffic being measured, with a retirement every 64 updates so the
//! reclaimer runs rather than idles.

use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ps_reclaim::Domain;

/// Symbols on the feed.
const SYMBOLS: u64 = 200;
/// Book levels rewritten per symbol per burst.
const LEVELS: u64 = 20;
/// Threads delivering the burst. Deliberately short of the core count: at one
/// thread per core the harness has no headroom and the arms drift by more than
/// they differ.
const WRITERS: usize = 4;
/// Consumers querying throughout.
const READERS: usize = 4;
const BURSTS: usize = 20;

fn cpu() -> Duration {
    // SAFETY: `getrusage` fills the `rusage` for `RUSAGE_SELF` or returns
    // non-zero without writing.
    let mut usage: libc::rusage = unsafe { core::mem::zeroed() };
    assert_eq!(
        unsafe { libc::getrusage(libc::RUSAGE_SELF, &raw mut usage) },
        0
    );
    let part =
        |t: libc::timeval| Duration::new(t.tv_sec as u64, (t.tv_usec as u32).saturating_mul(1_000));
    part(usage.ru_utime) + part(usage.ru_stime)
}

/// One burst. `use_handle` picks how a pin finds this thread's registration.
///
/// The branch sits inside the loop rather than outside it so that both arms pay
/// it. It is perfectly predicted and identical in each, which is the point: the
/// only thing that differs between the arms is the pin.
fn burst(think: u32, use_handle: bool) -> (Duration, Duration, Vec<Duration>) {
    let domain: &'static Domain = Box::leak(Box::new(Domain::new()));
    let book: Arc<Vec<AtomicU64>> = Arc::new((0..SYMBOLS * LEVELS).map(AtomicU64::new).collect());
    let stop = Arc::new(AtomicBool::new(false));
    let samples = Arc::new(std::sync::Mutex::new(Vec::<Duration>::new()));

    let before = cpu();
    let started = Instant::now();
    std::thread::scope(|scope| {
        for _ in 0..READERS {
            let book = Arc::clone(&book);
            let stop = Arc::clone(&stop);
            scope.spawn(move || {
                let handle = if use_handle {
                    Some(domain.handle())
                } else {
                    None
                };
                let mut acc = 0u64;
                let mut key = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    key = key.wrapping_add(2_654_435_761) % (SYMBOLS * LEVELS);
                    if let Some(h) = &handle {
                        let g = h.pin();
                        acc ^= book[key as usize].load(Ordering::Relaxed);
                        black_box(&g);
                    } else {
                        let g = domain.pin();
                        acc ^= book[key as usize].load(Ordering::Relaxed);
                        black_box(&g);
                    }
                    for _ in 0..think {
                        core::hint::spin_loop();
                    }
                }
                black_box(acc);
            });
        }
        let writers: Vec<_> = (0..WRITERS)
            .map(|w| {
                let book = Arc::clone(&book);
                let samples = Arc::clone(&samples);
                scope.spawn(move || {
                    let handle = if use_handle {
                        Some(domain.handle())
                    } else {
                        None
                    };
                    let mut mine = Vec::with_capacity((SYMBOLS / WRITERS as u64 * LEVELS) as usize);
                    // Each writer owns a contiguous slice of symbols, as a feed
                    // handler shard would.
                    let per = SYMBOLS / WRITERS as u64;
                    let from = w as u64 * per;
                    let mut n = 0u64;
                    for symbol in from..from + per {
                        for level in 0..LEVELS {
                            let key = symbol * LEVELS + level;
                            let at = Instant::now();
                            if let Some(h) = &handle {
                                let g = h.pin();
                                book[key as usize].store(key.wrapping_mul(31), Ordering::Relaxed);
                                black_box(&g);
                            } else {
                                let g = domain.pin();
                                book[key as usize].store(key.wrapping_mul(31), Ordering::Relaxed);
                                black_box(&g);
                            }
                            n += 1;
                            if n.is_multiple_of(64) {
                                domain.retire(|| ());
                            }
                            mine.push(at.elapsed());
                        }
                    }
                    samples.lock().expect("not poisoned").extend(mine);
                })
            })
            .collect();
        for writer in writers {
            writer.join().expect("writer");
        }
        stop.store(true, Ordering::Relaxed);
    });
    let drained = started.elapsed();
    let latencies = core::mem::take(&mut *samples.lock().expect("not poisoned"));
    (drained, cpu() - before, latencies)
}

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort_unstable();
    v[v.len() / 2]
}

fn pct(v: &[Duration], p: f64) -> Duration {
    v[((v.len() as f64 * p) as usize).min(v.len() - 1)]
}

fn main() {
    let updates = SYMBOLS * LEVELS;
    println!(
        "\n  {SYMBOLS} symbols x {LEVELS} levels per burst, {WRITERS} writers, {READERS} readers, {BURSTS} bursts.\n\
         \n  thru  = updates per second while a burst is draining, in millions.\n  \
           drain = time for a whole burst to land; the feed sends one every 500 ms.\n  \
           p50..max = latency of one update, across every update of every burst.\n\
         \n  The arms differ only in how a pin finds this thread's registration.\n  \
           The third row is the first arm again, so the table carries its own\n  \
           noise floor: whatever it differs from row one by is drift."
    );

    for think in [0u32, 100, 1_000, 10_000] {
        println!("\n  reader think time: {think}");
        println!("                        thru   cpu ms    drain ms          update latency ms");
        println!(
            "  pin path              M/s   median   median   worst    p50     p99   p99.9     max"
        );
        for (name, use_handle) in [
            ("thread-local", false),
            ("handle", true),
            ("null (thread-local)", false),
        ] {
            let mut drains = Vec::with_capacity(BURSTS);
            let mut cpus = Vec::with_capacity(BURSTS);
            let mut latencies = Vec::with_capacity(BURSTS * updates as usize);
            for _ in 0..BURSTS {
                let (drain, burst_cpu, mut samples) = burst(think, use_handle);
                drains.push(drain);
                cpus.push(burst_cpu);
                latencies.append(&mut samples);
            }
            latencies.sort_unstable();
            let drain_median = median(drains.clone());
            let worst = drains.into_iter().max().unwrap_or_default();
            let thru = updates as f64 / drain_median.as_secs_f64() / 1e6;
            println!(
                "  {name:<20} {thru:>5.2} {:>8.2} {:>8.2} {:>7.2} {:>7.3} {:>7.3} {:>7.3} {:>7.3}",
                median(cpus).as_secs_f64() * 1e3,
                drain_median.as_secs_f64() * 1e3,
                worst.as_secs_f64() * 1e3,
                pct(&latencies, 0.50).as_secs_f64() * 1e3,
                pct(&latencies, 0.99).as_secs_f64() * 1e3,
                pct(&latencies, 0.999).as_secs_f64() * 1e3,
                latencies.last().copied().unwrap_or_default().as_secs_f64() * 1e3,
            );
        }
    }
}

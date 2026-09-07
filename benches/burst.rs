//! What the pin path costs a burst of writes, with readers running throughout.
//!
//! # What this measures, and what an earlier version of it measured instead
//!
//! A review of the first version established that it measured mostly the wrong
//! thing, and the correction is worth stating because the mistake is easy:
//!
//! - It spawned eight threads *inside* the timed interval and joined them
//!   inside it too, so thread creation, first-touch registration, buffer
//!   allocation and reader shutdown were all charged to "burst drain". Measured
//!   directly, that envelope with **zero updates in it** is 0.191 ms on the
//!   thread-local arm and 0.154 ms on the handle arm, against reported drains
//!   of 0.28 and 0.13. The lifecycle was most of the number, and it already
//!   differed between the arms by 1.24x before any work happened.
//! - It never reclaimed. Nothing called `advance`, the domain was leaked, and
//!   the retirements were no-op closures, so the registry scan and the
//!   cross-core traffic that a real reclamation workload generates never
//!   happened at all.
//!
//! So this version:
//!
//! - Spawns workers **once**, has them register **once**, and starts timing
//!   only after every one of them is parked on a barrier.
//! - Times from the barrier release to the last writer finishing, on a second
//!   barrier. Readers keep running across bursts and are stopped afterwards, so
//!   their shutdown is outside every measured interval.
//! - Reclaims for real: retirements own an allocation whose `Drop` increments a
//!   counter, `advance_up_to` runs between bursts, and the harness **asserts**
//!   that the deferred work actually executed. A run that reclaims nothing is a
//!   failed run, not a fast one.
//! - Runs the arms in a counterbalanced order, `A B B A`, and reports each
//!   position separately, because a null measures repeatability at a position
//!   rather than the absence of a position effect.
//! - Reports reader operation counts, so the CPU column describes comparable
//!   work rather than whichever arm let its readers spin more.
//!
//! # The two arms
//!
//! Identical but for the pin: one finds this thread's registration in
//! thread-local storage, the other was handed it. Run both builds:
//!
//! ```text
//! cargo bench --bench burst
//! cargo bench --bench burst --no-default-features --features libc,spin
//! ```
//!
//! Per-update `Instant::now()` is still in the writer loop, because tail
//! latency is the point of a burst benchmark. It is a real share of a ~200 ns
//! update and both arms pay it identically; read the arms against each other,
//! never as absolute throughput.

use std::hint::black_box;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::time::{Duration, Instant};

use ps_reclaim::Domain;

/// Symbols on the feed.
const SYMBOLS: u64 = 200;
/// Book levels rewritten per symbol per burst.
const LEVELS: u64 = 20;
/// Threads delivering the burst.
const WRITERS: usize = 4;
/// Consumers querying throughout.
const READERS: usize = 4;
const BURSTS: usize = 20;
/// Updates between retirements, per writer.
const RETIRE_EVERY: u64 = 64;

/// Counts destructors that actually ran, so a run that reclaims nothing fails.
static RECLAIMED: AtomicUsize = AtomicUsize::new(0);

/// A retirement with real work in it: an allocation whose `Drop` is observable.
struct Deferred(#[allow(dead_code)] Box<[u64; 8]>);

impl Drop for Deferred {
    fn drop(&mut self) {
        RECLAIMED.fetch_add(1, Ordering::Relaxed);
    }
}

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

struct Outcome {
    drains: Vec<Duration>,
    latencies: Vec<Duration>,
    cpu: Duration,
    reader_ops: u64,
    reclaimed: usize,
}

fn run_arm(use_handle: bool, think: u32) -> Outcome {
    let domain = Domain::new();
    let book: Vec<AtomicU64> = (0..SYMBOLS * LEVELS).map(AtomicU64::new).collect();
    let stop = AtomicBool::new(false);
    let reader_ops = AtomicU64::new(0);
    let samples: Mutex<Vec<Duration>> = Mutex::new(Vec::new());
    // Writers plus this thread. Readers are deliberately not in the barriers:
    // they run continuously across every burst.
    let release = Arc::new(Barrier::new(WRITERS + 1));
    let done = Arc::new(Barrier::new(WRITERS + 1));

    let before_reclaimed = RECLAIMED.load(Ordering::Relaxed);
    let mut drains = Vec::with_capacity(BURSTS);
    let mut before_cpu = Duration::ZERO;
    let mut arm_cpu = Duration::ZERO;

    std::thread::scope(|scope| {
        for _ in 0..READERS {
            let (domain, book, stop, reader_ops) = (&domain, &book, &stop, &reader_ops);
            scope.spawn(move || {
                // Register before anyone starts timing.
                let handle = use_handle.then(|| domain.handle());
                let mut acc = 0u64;
                let mut key = 0u64;
                let mut ops = 0u64;
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
                    ops += 1;
                    for _ in 0..think {
                        core::hint::spin_loop();
                    }
                }
                reader_ops.fetch_add(ops, Ordering::Relaxed);
                black_box(acc);
            });
        }

        for w in 0..WRITERS {
            let (domain, book, samples) = (&domain, &book, &samples);
            let (release, done) = (Arc::clone(&release), Arc::clone(&done));
            scope.spawn(move || {
                let handle = use_handle.then(|| domain.handle());
                let per = SYMBOLS / WRITERS as u64;
                let from = w as u64 * per;
                let mut mine = Vec::with_capacity(BURSTS * (per * LEVELS) as usize);
                let mut n = 0u64;
                for _ in 0..BURSTS {
                    // Every worker is registered, allocated and parked here.
                    // Nothing before this point is inside a measured interval.
                    release.wait();
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
                            if n.is_multiple_of(RETIRE_EVERY) {
                                let junk = Deferred(Box::new([key; 8]));
                                domain.retire(move || drop(junk));
                            }
                            mine.push(at.elapsed());
                        }
                    }
                    done.wait();
                }
                samples.lock().expect("not poisoned").extend(mine);
            });
        }

        // Warm: let the readers reach steady state before the first burst.
        std::thread::sleep(Duration::from_millis(2));
        before_cpu = cpu();
        for _ in 0..BURSTS {
            release.wait();
            let at = Instant::now();
            done.wait();
            // The last writer has finished. Readers are still running, and
            // their shutdown is outside every interval this records.
            drains.push(at.elapsed());
            // Reclaim between bursts rather than never.
            domain.advance_up_to(4096);
        }
        arm_cpu = cpu() - before_cpu;
        stop.store(true, Ordering::Relaxed);
    });

    // Drain whatever the last burst left, so the assertion below is about the
    // whole arm rather than about timing.
    while domain.advance_up_to(4096) != 0 {}

    Outcome {
        drains,
        latencies: core::mem::take(&mut *samples.lock().expect("not poisoned")),
        cpu: arm_cpu,
        reader_ops: reader_ops.load(Ordering::Relaxed),
        reclaimed: RECLAIMED.load(Ordering::Relaxed) - before_reclaimed,
    }
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
        "\n  {SYMBOLS} symbols x {LEVELS} levels per burst, {WRITERS} writers, {READERS} readers,\n  \
           {BURSTS} bursts. Workers are spawned and registered once, before any timing.\n  \
           Each burst is timed from a release barrier to the last writer finishing;\n  \
           readers run throughout and are stopped outside every interval.\n\
         \n  Arms run counterbalanced, tls-handle-handle-tls, three times over. Read the\n  \
           spread WITHIN an arm across its six positions first: if that rivals the gap\n  \
           between the arms, the row is a position effect and not a result.\n\
         \n  reclaimed = deferred destructors that actually ran. Zero is a failed run.\n  \
           rd kops   = reader operations completed, in thousands. The arms do NOT do\n  
           equal reader work: cheaper pins let readers spin faster, so the\n  
           faster arm faces MORE read traffic on the same cache lines. That\n  
           confound runs against the faster arm, not for it."
    );

    for think in [0u32, 100, 1_000, 10_000] {
        println!("\n  reader think time: {think}");
        println!(
            "                     thru    cpu ms    drain ms         update latency ms       rd kops  reclaimed"
        );
        println!(
            "  pos  arm          M/s     total   median   worst    p50     p99   p99.9    max     (k)"
        );
        for (pos, use_handle) in [
            (1, false),
            (2, true),
            (3, true),
            (4, false),
            (5, false),
            (6, true),
            (7, true),
            (8, false),
            (9, false),
            (10, true),
            (11, true),
            (12, false),
        ] {
            let out = run_arm(use_handle, think);
            assert!(
                out.reclaimed > 0,
                "no deferred work ran: this arm measured nothing about reclamation"
            );
            let mut lat = out.latencies;
            lat.sort_unstable();
            let drain_median = median(out.drains.clone());
            let worst = out.drains.into_iter().max().unwrap_or_default();
            let thru = updates as f64 / drain_median.as_secs_f64() / 1e6;
            println!(
                "  {pos}    {:<11} {thru:>5.2} {:>8.2} {:>8.2} {:>7.2} {:>7.3} {:>7.3} {:>7.3} {:>6.3} {:>7.1} {:>8}",
                if use_handle { "handle" } else { "thread-local" },
                out.cpu.as_secs_f64() * 1e3,
                drain_median.as_secs_f64() * 1e3,
                worst.as_secs_f64() * 1e3,
                pct(&lat, 0.50).as_secs_f64() * 1e3,
                pct(&lat, 0.99).as_secs_f64() * 1e3,
                pct(&lat, 0.999).as_secs_f64() * 1e3,
                lat.last().copied().unwrap_or_default().as_secs_f64() * 1e3,
                out.reader_ops as f64 / 1e3,
                out.reclaimed,
            );
        }
    }
}

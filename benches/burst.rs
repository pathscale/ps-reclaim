//! Synthetic pinned book updates, not an HFT latency certification.
//!
//! Persistent workers warm both registrations before timing. A ready barrier
//! excludes setup, a start barrier releases all workers, and the last writer's
//! completion timestamp ends the drain metric. TLS/handle/TLS-control order is
//! counterbalanced across all six permutations. Every 64th writer update
//! retires an owned allocation and drives a bounded-callback reclamation pass.
//!
//! Per-update clock reads, start-barrier skew, scheduling, reader think loops,
//! and allocation all remain part of the workload. Reader work is not fixed:
//! report its count, and do not call process CPU a cost-per-identical-operation.
//! Final quiescent garbage draining is checked but outside the timed window.

use std::hint::black_box;
use std::sync::{Arc, Barrier, mpsc};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use ps_reclaim::{Domain, Handle};

const SYMBOLS: usize = 200;
const LEVELS: usize = 20;
const WRITERS: usize = 4;
const READERS: usize = 4;
const UPDATES: usize = SYMBOLS * LEVELS;
const PER_WRITER: usize = UPDATES / WRITERS;
const RETIRE_EVERY: usize = 64;
const WARMUP_ROUNDS: usize = 6;
const ROUNDS: usize = 24;
const ORDERS: [[usize; 3]; 6] = [
    [0, 1, 2], [0, 2, 1], [1, 0, 2], [1, 2, 0], [2, 0, 1], [2, 1, 0],
];
const _: () = assert!(UPDATES % WRITERS == 0);

#[cfg(unix)]
fn cpu() -> Option<Duration> {
    // SAFETY: zero is a valid initial representation, and getrusage writes
    // through a valid pointer. Only consume its result on success.
    let mut usage: libc::rusage = unsafe { core::mem::zeroed() };
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &raw mut usage) } != 0 {
        return None;
    }
    let part = |t: libc::timeval| {
        Duration::new(t.tv_sec as u64, (t.tv_usec as u32) * 1_000)
    };
    Some(part(usage.ru_utime) + part(usage.ru_stime))
}

#[cfg(not(unix))]
fn cpu() -> Option<Duration> { None }

struct WriterJob {
    explicit: bool,
    samples: Vec<Duration>,
}

struct WriterReport {
    finished: Instant,
    samples: Vec<Duration>,
}

#[derive(Default)]
struct Measurements {
    drains: Vec<Duration>,
    cpus: Vec<Duration>,
    reads: Vec<u64>,
    latencies: Vec<Duration>,
}

fn run(think: u32) -> [Measurements; 3] {
    let domain = Domain::new();
    let book: Vec<_> = (0..UPDATES as u64).map(AtomicU64::new).collect();
    let stop = AtomicBool::new(false);
    let ready = Barrier::new(READERS + WRITERS + 1);
    let go = Barrier::new(READERS + WRITERS + 1);
    let reclaimed = Arc::new(AtomicUsize::new(0));
    let mut results: [Measurements; 3] = std::array::from_fn(|_| Measurements::default());

    std::thread::scope(|scope| {
        let mut readers = Vec::new();
        for _ in 0..READERS {
            let (jobs, receive) = mpsc::channel::<bool>();
            let (report, reports) = mpsc::channel();
            let (domain, book, stop, ready, go) = (&domain, &book, &stop, &ready, &go);
            scope.spawn(move || {
                let handle = Handle::new();
                drop(domain.pin());
                drop(domain.pin_with(&handle));
                while let Ok(explicit) = receive.recv() {
                    let mut key = 0usize;
                    let mut acc = 0u64;
                    let mut count = 0u64;
                    ready.wait();
                    go.wait();
                    while !stop.load(Ordering::Relaxed) {
                        key = key.wrapping_add(2_654_435_761) % UPDATES;
                        if explicit {
                            let guard = domain.pin_with(&handle);
                            acc ^= book[key].load(Ordering::Relaxed);
                            black_box(&guard);
                        } else {
                            let guard = domain.pin();
                            acc ^= book[key].load(Ordering::Relaxed);
                            black_box(&guard);
                        }
                        count += 1;
                        for _ in 0..think { core::hint::spin_loop(); }
                    }
                    black_box(acc);
                    report.send(count).unwrap();
                }
            });
            readers.push((jobs, reports));
        }

        let mut writers = Vec::new();
        for writer in 0..WRITERS {
            let (jobs, receive) = mpsc::channel::<WriterJob>();
            let (report, reports) = mpsc::channel();
            let (domain, book, ready, go) = (&domain, &book, &ready, &go);
            let reclaimed = Arc::clone(&reclaimed);
            scope.spawn(move || {
                let handle = Handle::new();
                drop(domain.pin());
                drop(domain.pin_with(&handle));
                while let Ok(mut job) = receive.recv() {
                    job.samples.clear();
                    ready.wait();
                    go.wait();
                    for offset in 0..PER_WRITER {
                        let key = writer * PER_WRITER + offset;
                        let at = Instant::now();
                        if job.explicit {
                            let guard = domain.pin_with(&handle);
                            book[key].store((key as u64).wrapping_mul(31), Ordering::Relaxed);
                            black_box(&guard);
                        } else {
                            let guard = domain.pin();
                            book[key].store((key as u64).wrapping_mul(31), Ordering::Relaxed);
                            black_box(&guard);
                        }
                        if (offset + 1) % RETIRE_EVERY == 0 {
                            // This payload is never published to readers: it
                            // exercises ownership/destruction, not pointer safety.
                            let payload = Box::new([key as u64; 8]);
                            let reclaimed = Arc::clone(&reclaimed);
                            domain.retire(move || {
                                drop(black_box(payload));
                                reclaimed.fetch_add(1, Ordering::Relaxed);
                            });
                            domain.advance_up_to(8);
                        }
                        job.samples.push(at.elapsed());
                    }
                    let finished = Instant::now();
                    report.send(WriterReport { finished, samples: job.samples }).unwrap();
                }
            });
            writers.push((jobs, reports, Vec::with_capacity(PER_WRITER)));
        }

        let mut expected_reclaimed = 0;
        for round in 0..WARMUP_ROUNDS + ROUNDS {
            for arm in ORDERS[round % ORDERS.len()] {
                stop.store(false, Ordering::Relaxed);
                for (jobs, _) in &readers { jobs.send(arm == 1).unwrap(); }
                for (jobs, _, samples) in &mut writers {
                    jobs.send(WriterJob {
                        explicit: arm == 1, samples: core::mem::take(samples),
                    }).unwrap();
                }
                ready.wait();
                let before_cpu = cpu();
                let started = Instant::now();
                go.wait();
                let mut finished = started;
                for (_, reports, samples) in &mut writers {
                    let report = reports.recv().unwrap();
                    finished = finished.max(report.finished);
                    *samples = report.samples;
                }
                stop.store(true, Ordering::Relaxed);
                let reads: u64 = readers.iter().map(|(_, report)| report.recv().unwrap()).sum();
                // CPU covers release through reader shutdown, not just the
                // drain. Capture before aggregation and quiescent reclamation.
                let used_cpu = before_cpu.zip(cpu()).map(|(a, b)| b.saturating_sub(a));
                expected_reclaimed += WRITERS * (PER_WRITER / RETIRE_EVERY);
                while domain.pending() != 0 {
                    assert!(domain.advance() != 0, "quiescent reclamation stalled");
                }
                assert_eq!(reclaimed.load(Ordering::Relaxed), expected_reclaimed);
                if round >= WARMUP_ROUNDS {
                    let result = &mut results[arm];
                    result.drains.push(finished.duration_since(started));
                    if let Some(cpu) = used_cpu { result.cpus.push(cpu); }
                    result.reads.push(reads);
                    for (_, _, samples) in &writers {
                        result.latencies.extend_from_slice(samples);
                    }
                }
            }
        }
        // Dropping senders terminates workers before scoped joining; their
        // registration teardown is outside every measured burst.
        drop(writers);
        drop(readers);
    });
    assert_eq!(domain.pending(), 0);
    results
}

fn percentile(sorted: &[Duration], per_mille: usize) -> Duration {
    sorted[(sorted.len() * per_mille / 1_000).min(sorted.len() - 1)]
}

fn main() {
    println!("{UPDATES} updates/burst; {WRITERS} persistent writers, {READERS} readers");
    println!("{WARMUP_ROUNDS} warmup + {ROUNDS} measured rounds; counterbalanced TLS/handle/TLS control");
    println!("60 retirements/burst; advance_up_to(8) after each retirement; final drain excluded");
    println!("CPU: release through reader shutdown; drain: release through last writer timestamp");
    println!("No affinity control; sampled update times include clocks and periodic reclamation.");
    for think in [0u32, 100, 1_000, 10_000] {
        println!("\nreader think: {think} spin_loop iterations");
        println!("arm             Mupdates/s  drain us  CPU ms  reads/burst   p50 ns   p99 ns p99.9 ns   max ns");
        for (name, mut result) in ["TLS", "handle", "TLS control"].into_iter().zip(run(think)) {
            result.drains.sort_unstable();
            result.cpus.sort_unstable();
            result.reads.sort_unstable();
            result.latencies.sort_unstable();
            let drain = percentile(&result.drains, 500);
            let cpu = if result.cpus.is_empty() {
                "n/a".to_owned()
            } else {
                format!("{:.3}", percentile(&result.cpus, 500).as_secs_f64() * 1e3)
            };
            println!("{name:<15} {:>10.3} {:>9.3} {cpu:>7} {:>12} {:>8} {:>8} {:>8} {:>8}",
                UPDATES as f64 / drain.as_secs_f64() / 1e6,
                drain.as_secs_f64() * 1e6,
                result.reads[result.reads.len() / 2],
                percentile(&result.latencies, 500).as_nanos(),
                percentile(&result.latencies, 990).as_nanos(),
                percentile(&result.latencies, 999).as_nanos(),
                result.latencies.last().unwrap().as_nanos());
        }
    }
}

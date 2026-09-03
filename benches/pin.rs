//! What a pin costs, and whether it stays flat as readers are added.
//!
//! Flatness is the point. A scheme whose read path writes a shared line looks
//! fine on one thread and collapses on eight, which is exactly how the
//! regression that prompted this crate reached production.
//!
//! `crossbeam-epoch` and `seize` are dev-dependencies purely so this can
//! compare against them.

use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use ps_reclaim::Domain;

/// Run `f` on `threads` threads at once and report the slowest one's rate.
fn contended<F>(c: &mut Criterion, group: &str, threads: &[usize], f: F)
where
    F: Fn(usize) + Send + Sync + Copy + 'static,
{
    let mut g = c.benchmark_group(group);
    for &n in threads {
        g.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.iter_custom(|iters| {
                let go = Arc::new(AtomicBool::new(false));
                let workers: Vec<_> = (0..n)
                    .map(|_| {
                        let go = go.clone();
                        std::thread::spawn(move || {
                            while !go.load(Ordering::Relaxed) {
                                std::hint::spin_loop();
                            }
                            let start = std::time::Instant::now();
                            for _ in 0..iters {
                                f(0);
                            }
                            start.elapsed()
                        })
                    })
                    .collect();
                go.store(true, Ordering::Relaxed);
                workers
                    .into_iter()
                    .map(|w| w.join().unwrap())
                    .max()
                    .unwrap_or_default()
            })
        });
    }
    g.finish();
}

fn ps_reclaim_pin(c: &mut Criterion) {
    static DOMAIN: std::sync::OnceLock<Domain> = std::sync::OnceLock::new();
    let d = DOMAIN.get_or_init(Domain::new);
    contended(c, "pin/ps-reclaim", &[1, 2, 4, 8], |_| {
        let g = d.pin();
        black_box(&g);
    });
}

fn crossbeam_pin(c: &mut Criterion) {
    static COLLECTOR: std::sync::OnceLock<crossbeam_epoch::Collector> = std::sync::OnceLock::new();
    let col: &'static crossbeam_epoch::Collector =
        COLLECTOR.get_or_init(crossbeam_epoch::Collector::new);
    contended(c, "pin/crossbeam", &[1, 2, 4, 8], move |_| {
        // Registered per thread and cached, which is how a real caller uses
        // it: `LocalHandle` is not `Sync`.
        thread_local! {
            static H: std::cell::RefCell<Option<crossbeam_epoch::LocalHandle>> =
                const { std::cell::RefCell::new(None) };
        }
        H.with(|h| {
            let mut h = h.borrow_mut();
            let handle = h.get_or_insert_with(|| col.register());
            let g = handle.pin();
            black_box(&g);
        });
    });
}

fn seize_pin(c: &mut Criterion) {
    static COLLECTOR: std::sync::OnceLock<seize::Collector> = std::sync::OnceLock::new();
    let col: &'static seize::Collector =
        COLLECTOR.get_or_init(|| seize::Collector::new().batch_size(256));
    contended(c, "pin/seize", &[1, 2, 4, 8], move |_| {
        let g = col.enter();
        black_box(&g);
    });
}

criterion_group!(
    benches,
    ps_reclaim_pin,
    crossbeam_pin,
    seize_pin,
    registry_pressure
);
criterion_main!(benches);

/// Reports registry pressure after the pin benchmarks, because slot
/// exhaustion looks exactly like a loss of flatness.
fn registry_pressure(_c: &mut Criterion) {
    eprintln!(
        "registry slots handed out: {} (max {})",
        ps_reclaim::slots_handed_out(),
        ps_reclaim::MAX_THREADS
    );
}

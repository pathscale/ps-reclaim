//! What one `advance()` costs, which is what a caller that advances per
//! mutation actually pays.
//!
//! Two shapes, because they exercise different halves:
//!
//! - empty: advance with nothing retired. The common case for a caller that
//!   advances on every mutation, since the previous call already drained.
//! - retire: retire one item then advance, the write-path steady state.
use ps_reclaim::Domain;
use std::hint::black_box;
use std::time::Instant;

const N: u32 = 200_000;

fn main() {
    let d = Domain::new();
    for _ in 0..1000 {
        black_box(d.advance());
    }

    let mut best = f64::MAX;
    for _ in 0..5 {
        let t = Instant::now();
        for _ in 0..N {
            black_box(d.advance());
        }
        let ns = t.elapsed().as_nanos() as f64 / N as f64;
        if ns < best {
            best = ns;
        }
    }
    println!("advance, nothing retired : {best:>9.1} ns");

    let mut best = f64::MAX;
    for _ in 0..5 {
        let t = Instant::now();
        for _ in 0..N {
            d.retire(|| {});
            black_box(d.advance());
        }
        let ns = t.elapsed().as_nanos() as f64 / N as f64;
        if ns < best {
            best = ns;
        }
    }
    println!("retire + advance         : {best:>9.1} ns");

    let mut best = f64::MAX;
    for _ in 0..5 {
        let t = Instant::now();
        for _ in 0..N {
            let g = d.pin();
            d.retire(|| {});
            drop(g);
            black_box(d.advance());
        }
        let ns = t.elapsed().as_nanos() as f64 / N as f64;
        if ns < best {
            best = ns;
        }
    }
    println!("pin + retire + advance   : {best:>9.1} ns   <- the WorkTable page path");
}

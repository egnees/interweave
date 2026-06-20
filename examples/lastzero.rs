//! Reproduce the POPL'14 `lastzero` benchmark count.
//!
//! One atomic per cell `a0..=aN` (distinct objects — the object id is what makes
//! two cells independent). A reader scans top-down while it keeps reading nonzero
//! values; writer `j` stores `load(a[j-1]) + 1` into `a[j]`. The reader's
//! data-dependent control flow replays automatically. Optimal DPOR explores
//! `(N+3)·2^(N-2)` maximal traces — one per Mazurkiewicz class.
//!
//! ```sh
//! cargo run --release --example lastzero -- [N]   # default N = 5 => 64 traces
//! ```

#[path = "common/mod.rs"]
mod common;

use interweave::{Atomic, World};

fn lastzero(world: &mut World, n: usize) {
    let cells: Vec<Atomic<i32>> = (0..=n).map(|i| world.atomic(format!("a{i}"), 0)).collect();

    let scan = cells.clone();
    world.spawn("reader", async move {
        // Walk down from the top cell while it reads nonzero; `i > 0` also guards
        // the index against underflow.
        let mut i = n;
        while scan[i].load().await != 0 && i > 0 {
            i -= 1;
        }
        Ok(())
    });

    for j in 1..=n {
        let (prev, cur) = (cells[j - 1].clone(), cells[j].clone());
        world.spawn(format!("writer-{j}"), async move {
            let v = prev.load().await;
            cur.store(v + 1).await;
            Ok(())
        });
    }
}

fn main() {
    let n = common::size(5);
    assert!(n >= 2, "lastzero needs N >= 2");

    let counts = common::explore_counts(move |w| lastzero(w, n));
    let expected = (n + 3) * (1usize << (n - 2));
    println!(
        "lastzero({n}): traces={} (expected {expected}), states={}",
        counts.traces, counts.states,
    );
}

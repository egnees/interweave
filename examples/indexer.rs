//! Reproduce the POPL'14 `indexer` benchmark.
//!
//! A 128-cell hash table (one atomic per cell). Each of `N` threads inserts four
//! values with `w = m*11 + tid` (`m = 1..=4`), hashes to `h = (w*7) % 128`, and
//! probes the next slot on a failed compare-exchange. Our `compare_exchange` is
//! already an atomic RMW, so the paper's `cas_mutex[]` is not modeled. Threads
//! collide only once `N` is large enough that two `w`s hash to the same slot —
//! that single collision is the only source of dependent operations, so the
//! reduction is dramatic (e.g. `N = 12` yields just 8 maximal traces).
//!
//! ```sh
//! cargo run --release --example indexer -- [N]   # default N = 12 => 8 traces
//! ```

#[path = "common/mod.rs"]
mod common;

use interweave::{Atomic, World};

fn indexer(world: &mut World, num_threads: usize) {
    let table: Vec<Atomic<i32>> = (0..128).map(|i| world.atomic(format!("t{i}"), 0)).collect();
    for tid in 0..num_threads {
        let table = table.clone();
        world.spawn(format!("thread-{tid}"), async move {
            for m in 1..=4 {
                let w = m * 11 + tid as i32;
                let mut h = ((w * 7) % 128) as usize;
                // Linear-probe until an empty slot accepts the value.
                while table[h].compare_exchange(0, w).await.is_err() {
                    h = (h + 1) % 128;
                }
            }
            Ok(())
        });
    }
}

fn main() {
    let n = common::size(12);
    let counts = common::explore_counts(move |w| indexer(w, n));
    println!(
        "indexer({n}): traces={}, states={}",
        counts.traces, counts.states
    );
}

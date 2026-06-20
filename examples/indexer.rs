//! Reproduce the POPL'14 `indexer` benchmark.
//!
//! A 128-cell hash table (one atomic per cell). Each of `N` threads inserts four
//! values with `w = (++m)*11 + tid`, hashes to `h = (w*7) % 128`, and probes the
//! next slot on a failed compare-exchange. Our `compare_exchange` is already an
//! atomic RMW, so the paper's `cas_mutex[]` is not modeled. Threads collide only
//! once `N` is large enough that two `w`s hash to the same slot — that single
//! collision is the only source of dependent operations, so the reduction is
//! dramatic (e.g. `N = 12` yields just 8 maximal traces).
//!
//! ```sh
//! cargo run --release --example indexer -- [N]   # default N = 12 => 8 traces
//! ```

use std::env;

use interweave::{Atomic, Observer, State, Strategy, World, explore};

fn indexer(world: &mut World, num_threads: usize) {
    let table: Vec<Atomic<i32>> = (0..128)
        .map(|i| world.atomic(format!("t{i}"), 0i32))
        .collect();
    for tid in 0..num_threads {
        let tab = table.to_vec();
        world.spawn(format!("thread-{tid}"), async move {
            let mut m = 0i32;
            for _ in 0..10 {
                if m < 4 {
                    m += 1;
                    let w = m * 11 + tid as i32;
                    let mut h = ((w * 7) % 128) as usize;
                    loop {
                        match tab[h].compare_exchange(0, w).await {
                            Ok(_) => break,
                            Err(_) => h = (h + 1) % 128,
                        }
                    }
                } else {
                    break;
                }
            }
            Ok(())
        });
    }
}

// Counts maximal traces (leaves) and total visited states.
#[derive(Default)]
struct Counter {
    traces: usize,
    states: usize,
}

impl Observer for Counter {
    fn observe(&mut self, state: &State) {
        self.states += 1;
        if state.is_terminal() {
            self.traces += 1;
        }
    }
}

fn main() {
    let n = env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(12);

    let setup = move |w: &mut World| indexer(w, n);
    let mut counter = Counter::default();
    let result = explore(&setup, &mut counter, Strategy::Optimal);

    println!(
        "indexer({n}): traces={}, states={}",
        counter.traces, counter.states,
    );
    if let Err(failed) = result {
        eprintln!("unexpected failure: {failed}");
    }
}

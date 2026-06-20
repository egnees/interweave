//! Reproduce the POPL'14 `lastzero` benchmark count.
//!
//! One atomic per cell `a0..=aN` (distinct objects — the object id is what makes
//! two cells independent). A reader scans top-down while it keeps reading zero;
//! writer `j` stores `load(a[j-1]) + 1` into `a[j]`. The reader's data-dependent
//! control flow replays automatically. Optimal DPOR explores `(N+3)·2^(N-2)`
//! maximal traces — one per Mazurkiewicz class.
//!
//! ```sh
//! cargo run --release --example lastzero -- [N]   # default N = 5 => 64 traces
//! ```

use std::env;

use interweave::{Atomic, Observer, State, Strategy, World, explore};

fn lastzero(world: &mut World, n: usize) {
    let cells: Vec<Atomic<i32>> = (0..=n)
        .map(|i| world.atomic(format!("a{i}"), 0i32))
        .collect();
    let rc = cells.to_vec();
    world.spawn("reader", async move {
        let mut i = n;
        loop {
            if rc[i].load().await == 0 {
                break;
            }
            if i == 0 {
                break; // guard against underflow; keep the future total
            }
            i -= 1;
        }
        Ok(())
    });
    for j in 1..=n {
        let r = cells[j - 1].clone();
        let w = cells[j].clone();
        world.spawn(format!("writer-{j}"), async move {
            let v = r.load().await;
            w.store(v + 1).await;
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
    let n = env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(5);
    assert!(n >= 2, "lastzero needs N >= 2");

    let setup = move |w: &mut World| lastzero(w, n);
    let mut counter = Counter::default();
    let result = explore(&setup, &mut counter, Strategy::Optimal);

    let expected = (n + 3) * (1usize << (n - 2));
    println!(
        "lastzero({n}): traces={} (expected {expected}), states={}",
        counter.traces, counter.states,
    );
    if let Err(failed) = result {
        eprintln!("unexpected failure: {failed}");
    }
}

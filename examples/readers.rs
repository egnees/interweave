//! Reproduce the POPL'14 `readers` benchmark count.
//!
//! One writer stores to a shared atomic; `N` readers load it. A store and a load
//! are dependent, but two loads commute, so each reader sits independently on one
//! side of the single store — `2^N` Mazurkiewicz classes, which is exactly how
//! many maximal traces Optimal DPOR explores.
//!
//! ```sh
//! cargo run --release --example readers -- [N]   # default N = 8 => 256 traces
//! ```

use std::env;

use interweave::{Observer, State, Strategy, World, explore};

fn readers(world: &mut World, n: usize) {
    let x = world.atomic("x", 0u32);
    let w = x.clone();
    world.spawn("writer", async move {
        w.store(42).await;
        Ok(())
    });
    for i in 1..=n {
        let r = x.clone();
        world.spawn(format!("reader-{i}"), async move {
            r.load().await;
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
    let n = env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(8);

    let setup = move |w: &mut World| readers(w, n);
    let mut counter = Counter::default();
    let result = explore(&setup, &mut counter, Strategy::Optimal);

    println!(
        "readers({n}): traces={} (expected {}), states={}",
        counter.traces,
        1usize << n,
        counter.states,
    );
    if let Err(failed) = result {
        eprintln!("unexpected failure: {failed}");
    }
}

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

#[path = "common/mod.rs"]
mod common;

use interweave::World;

fn readers(world: &mut World, n: usize) {
    let x = world.atomic("x", 0u32);
    let writer = x.clone();
    world.spawn("writer", async move {
        writer.store(42).await;
        Ok(())
    });
    for i in 1..=n {
        let reader = x.clone();
        world.spawn(format!("reader-{i}"), async move {
            reader.load().await;
            Ok(())
        });
    }
}

fn main() {
    let n = common::size(8);
    let counts = common::explore_counts(move |w| readers(w, n));
    println!(
        "readers({n}): traces={} (expected {}), states={}",
        counts.traces,
        1usize << n,
        counts.states,
    );
}

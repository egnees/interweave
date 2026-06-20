//! Shared helpers for the benchmark examples (`readers`, `lastzero`, `indexer`).
//!
//! Pulled into each example with `#[path = "common/mod.rs"] mod common;`, so the
//! trace-counting boilerplate lives in one place instead of being copied per
//! example.

use interweave::{Observer, State, Strategy, World, explore};

/// Tally of an exploration: maximal traces (leaves) and total visited states.
#[derive(Default)]
pub struct Counts {
    pub traces: usize,
    pub states: usize,
}

impl Observer for Counts {
    fn observe(&mut self, state: &State) {
        self.states += 1;
        if state.is_terminal() {
            self.traces += 1;
        }
    }
}

/// The benchmark size from the first CLI argument, falling back to `default`.
pub fn size(default: usize) -> usize {
    std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

/// Explores `program` under Optimal DPOR and returns the trace/state counts.
/// These benchmarks never fail, so a failure is surfaced but not propagated.
pub fn explore_counts<F: Fn(&mut World)>(program: F) -> Counts {
    let mut counts = Counts::default();
    if let Err(failed) = explore(&program, &mut counts, Strategy::Optimal) {
        eprintln!("unexpected failure: {failed}");
    }
    counts
}

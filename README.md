# interweave

[![CI](https://github.com/egnees/interweave/actions/workflows/ci.yml/badge.svg)](https://github.com/egnees/interweave/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/interweave.svg)](https://crates.io/crates/interweave)
[![docs.rs](https://docs.rs/interweave/badge.svg)](https://docs.rs/interweave)
[![license: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![MSRV: 1.96](https://img.shields.io/badge/MSRV-1.96-blue.svg)](https://blog.rust-lang.org/2026/05/28/Rust-1.96.0/)

Stateless model-checking sandbox built around **Optimal DPOR**. Write a handful
of processes as async Rust; `explore` runs them under every meaningfully distinct
schedule and returns the first interleaving that fails — replayable exactly — or
proves that none can. The same run drives an `Observer`, so you can watch which
interleavings the search visits and which it prunes.

> **Status:** early-stage research sandbox (`0.x`); the API is still taking shape
> and may change between minor releases.

Processes run on a custom single-threaded, deterministic executor — no async
runtime, because controlling the schedule is the whole point. Each primitive's
observable operations are `.await` points that hand control to the checker; new
ones (a lock, a barrier, another channel) plug in through the `Object` trait and
`World::register`. The strategy is **Optimal DPOR** (Abdulla et al., POPL'14): it
visits exactly one interleaving per Mazurkiewicz equivalence class, so the search
stays exhaustive without enumerating every ordering.

## Install

```sh
cargo add interweave
```

## Usage

### Finding a bug

A `producer` hands a value to a `consumer` through a `ready` flag, but raises the
flag *before* writing the value. `explore` returns `Ok` when every interleaving
passes, or the first failing one — here the schedule where the consumer sees
`ready == 1` yet reads the stale `data`, the race behind broken double-checked
locking:

```rust
use interweave::{World, explore};

fn publish(world: &mut World) {
    let data = world.atomic("data", 0);
    let ready = world.atomic("ready", 0);

    let (data_w, ready_w) = (data.clone(), ready.clone());
    world.spawn("producer", async move {
        ready_w.store(1).await; // announce the value...
        data_w.store(42).await; // ...before it has been written
        Ok(())
    });

    world.spawn("consumer", async move {
        if ready.load().await == 1 {
            let v = data.load().await;
            if v != 42 {
                return Err(format!("read the value before it was published: {v}").into());
            }
        }
        Ok(())
    });
}

// `()` is the no-op observer.
explore(&publish, &mut ()).expect_err("publishes the flag before the value");
```

Writing the value *before* raising the flag fixes it, and the checker then clears
every interleaving.

### Watching the algorithm

Pass an `Observer` instead of `&mut ()` to see the search from the inside: its
`step` method fires at each decision, and a `Maximal` step marks one complete
interleaving. Here one `writer` races two `reader`s on a single atomic — the two
reads commute, so of the `3! = 6` orderings Optimal DPOR visits only the four
that differ observably:

```rust
use interweave::{World, explore, Observer, Step, StepCx};

fn writer_two_readers(world: &mut World) {
    let x = world.atomic("x", 0);
    let (r1, r2) = (x.clone(), x.clone());
    world.spawn("writer", async move { x.store(1).await; Ok(()) });
    world.spawn("reader-1", async move { let _ = r1.load().await; Ok(()) });
    world.spawn("reader-2", async move { let _ = r2.load().await; Ok(()) });
}

// Record every complete interleaving the search runs to the end.
#[derive(Default)]
struct Interleavings(Vec<String>);

impl Observer for Interleavings {
    fn step(&mut self, step: Step<'_>, cx: StepCx<'_, '_>) {
        if let Step::Maximal { trace, .. } = step {
            let names: Vec<&str> = trace.iter().map(|t| cx.process(t.pid())).collect();
            self.0.push(names.join(" -> "));
        }
    }
}

let mut seen = Interleavings::default();
explore(&writer_two_readers, &mut seen).expect("every interleaving terminates");

// Four, not six: the two that only swap the order of the independent reads are
// pruned as equivalent.
assert_eq!(seen.0.len(), 4);
```

`Step` also reports the driver's other decisions — descend, seed, race-reversal,
pop — each carrying the live wakeup tree and sleep sets.

More worked programs live in [`examples/`](examples): a `bank` ledger and the
`publish` unsafe-publication race (atomic bug hunts), an `rpc_mux` reply-misrouting
bug over an MPSC channel, a from-scratch custom `Object` (`custom_object`), and the
POPL'14 `readers` / `lastzero` / `indexer` benchmarks. The full API is on
[docs.rs](https://docs.rs/interweave).

## Contributing

Issues and pull requests are welcome. Before sending a change, run the checks CI does:

```sh
cargo +nightly fmt --all -- --check   # formatting (see note below)
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features
cargo rustc --lib --all-features -- -D missing_docs
```

CI also checks the build on the 1.96 MSRV. Formatting requires **nightly** `rustfmt`
because `rustfmt.toml` enables unstable options (`wrap_comments`, `comment_width`).

## References

- Abdulla et al., *Optimal Dynamic Partial Order Reduction* (POPL'14)
- Flanagan & Godefroid, *Dynamic Partial-Order Reduction for Model Checking
  Software* (POPL'05) — the classical DPOR this builds on
- Nidhugg, Concuerror — reference implementations

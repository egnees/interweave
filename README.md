# interweave

[![CI](https://github.com/egnees/interweave/actions/workflows/ci.yml/badge.svg)](https://github.com/egnees/interweave/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/interweave.svg)](https://crates.io/crates/interweave)
[![docs.rs](https://img.shields.io/docsrs/interweave)](https://docs.rs/interweave)
[![license: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Stateless model-checking sandbox — watch Optimal DPOR explore the interleavings
of small concurrent programs, modeled as `Future`s on a deterministic,
from-scratch executor.

## What it is

A research sandbox for **stateless model checking** of concurrent programs.
Processes are Rust `Future`s driven by a custom single-threaded, deterministic
executor. Synchronization primitives (`Atomic` now, channels later)
are implemented from scratch so that every operation that may interact with
another process becomes an explicit scheduling point — a `.await` that hands
control back to the executor. The executor stays deliberately dumb; all
interleaving control lives in the primitives and in the exploration strategy on
top, which is where Optimal DPOR lives.

## Usage

Write a concurrent program against `World` / `Atomic`, then `explore` every
interleaving. The result is `Ok` if all of them pass, or the first failing one.
Here a `producer` hands a value to a `consumer` through a `ready` flag, but
raises the flag *before* it has written the value — so Optimal DPOR finds the
schedule where the consumer sees the flag set yet reads the stale value, the
unsafe-publication race behind broken double-checked locking:

```rust
use interweave::{Strategy, World, explore};

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
        // No wait loop: the checker explores the schedule where the flag is
        // already set, so a single guarded read stays a finite safety check.
        if ready.load().await == 1 {
            let v = data.load().await;
            if v != 42 {
                return Err(format!("read the value before it was published: {v}").into());
            }
        }
        Ok(())
    });
}

// Optimal DPOR finds the schedule where the consumer sees `ready == 1` but
// still reads the stale `data`.
explore(&publish, &mut (), Strategy::Optimal).expect_err("publishes the flag before the value");
```

Writing the value *before* raising the flag fixes it, and the checker then
clears every interleaving — that is the other half of its job, a proof rather
than the absence of a failing test run.

Runnable examples live in [`examples/`](examples):

- `publish` — the program above, with the failing schedule printed out.
- `bank` — two accounts and a non-atomic transfer; the auditor catches the money
  mid-transfer.
- `readers` / `lastzero` / `indexer` — reproduce the POPL'14 Optimal-DPOR
  benchmark counts (one maximal trace per Mazurkiewicz class).

## Layout

Three module layers, dependencies pointing downward (`search → model`, with
`model ↔ sync` a mutual pair):

- `model/` — the modeled system and its execution: the deterministic `executor`,
  the `Object` trait + `Transition`, processes, and the `World` / `State` /
  `StateView` the search walks. Transparent to the layer above.
- `sync/` — synchronization primitives (`Atomic` now; channels later) whose every
  observable operation is a `.await` yield point.
- `search/` — the exploration algorithms (naive DFS and Optimal DPOR; `explore`
  takes the `Strategy`) and the `Observer` hook they call at every state.

## Status

- [x] Deterministic executor (futures + per-process wakers + FIFO driver)
- [x] `Atomic` primitive with load / store / compare-exchange as yield points
- [x] Naive exhaustive DFS over interleavings (`explore`)
- [x] Optimal DPOR (Abdulla et al., POPL'14) — wakeup trees + sleep sets and
      vector-clock happens-before; one trace per Mazurkiewicz class, no
      sleep-set blocking
- [ ] Channels (blocking, unbounded MPSC)
- [ ] Visualization of the interleaving tree / happens-before graphs

## References

- Abdulla et al., *Optimal Dynamic Partial Order Reduction* (POPL'14)
- Flanagan & Godefroid, *Dynamic Partial-Order Reduction for Model Checking
  Software* (POPL'05) — the classical DPOR this builds on
- Nidhugg, Concuerror — reference implementations

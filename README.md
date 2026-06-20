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
interleaving. The result is `Ok` if all of them pass, or the first failing one:

```rust
use interweave::{Strategy, World, explore};

fn program(world: &mut World) {
    let x = world.atomic("x", 0u32);
    let a = x.clone();
    world.spawn("writer-1", async move {
        a.store(1).await;
        Ok(())
    });
    world.spawn("writer-2", async move {
        x.store(2).await;
        Ok(())
    });
}

explore(&program, &mut (), Strategy::Optimal).expect("no interleaving fails");
```

Runnable examples live in [`examples/`](examples):

- `find_bug` — the checker pinning down a write-write race.
- `readers` / `lastzero` / `indexer` — reproduce the POPL'14 Optimal-DPOR
  benchmark counts (one maximal trace per Mazurkiewicz class).

```sh
cargo run --example find_bug
cargo run --release --example lastzero 6
```

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

## Commands

```sh
cargo test                  # run the tests
cargo clippy --all-targets  # lint
cargo +nightly fmt          # rustfmt.toml uses nightly-only options
```

## References

- Abdulla et al., *Optimal Dynamic Partial Order Reduction* (POPL'14)
- Flanagan & Godefroid, *Dynamic Partial-Order Reduction for Model Checking
  Software* (POPL'05) — the classical DPOR this builds on
- Nidhugg, Concuerror — reference implementations

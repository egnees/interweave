# interweave

Stateless model-checking sandbox — watch DPOR and Optimal DPOR explore the
interleavings of small concurrent programs, modeled as `Future`s on a
deterministic, from-scratch executor.

## What it is

A research sandbox for **stateless model checking** of concurrent programs.
Processes are Rust `Future`s driven by a custom single-threaded, deterministic
executor. Synchronization primitives (`Atomic`, and later `Mutex` / channels)
are implemented from scratch so that every operation that may interact with
another process becomes an explicit scheduling point — a `.await` that hands
control back to the executor. The executor stays deliberately dumb; all
interleaving control lives in the primitives and in the exploration strategy on
top, which is where DPOR / Optimal DPOR live.

## Status

- [x] Deterministic executor (futures + per-process wakers + FIFO driver)
- [x] `Atomic` primitive with load / store / compare-exchange as yield points
- [x] Naive exhaustive DFS over interleavings (`explore`)
- [ ] DPOR
- [ ] Optimal DPOR
- [ ] More primitives (`Mutex`, channels)
- [ ] Visualization of the interleaving tree / happens-before graphs

## Commands

```sh
cargo test                  # run the tests
cargo clippy --all-targets  # lint
cargo +nightly fmt          # rustfmt.toml uses nightly-only options
```

## References

- Flanagan & Godefroid, *Dynamic Partial-Order Reduction for Model Checking
  Software* (POPL'05)
- Abdulla et al., *Optimal Dynamic Partial Order Reduction* (POPL'14)
- Nidhugg — reference implementations

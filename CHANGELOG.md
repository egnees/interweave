# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Deterministic single-threaded executor driving processes written as `Future`s.
- `Atomic` synchronization primitive with `load` / `store` / `compare_exchange`,
  each an `.await` scheduling point.
- Unbounded MPSC channel primitive: a cloneable `Sender` (multi-producer) and a
  `!Clone` `Receiver` (single-consumer), with `send` / `recv` as `.await`
  scheduling points; a `recv` on an empty channel blocks. Created via
  `World::channel`. Optimal DPOR reverses only non-disabling races, so a consuming
  send→recv (a reads-from edge) is not redundantly explored.
- Public `Object` trait and `World::register` extension point for defining custom
  synchronization primitives (the built-in `Atomic` is one such primitive).
- `explore` entry point with two strategies: naive exhaustive `Strategy::Dfs` and
  `Strategy::Optimal` (Optimal DPOR, Abdulla et al., POPL'14).
- `Observer` hook called at every explored state (`&mut ()` to observe nothing).
- Examples: `publish` and `bank` (the checker finding an unsafe-publication and a
  non-atomic transfer bug), `custom_object` (a from-scratch primitive via the
  `Object` / `World::register` extension point), `rpc_mux` (an RPC connection
  multiplexer that routes replies by a shared in-flight slot instead of the id in
  each frame, so a reply can be delivered to the wrong call), and `readers` /
  `lastzero` / `indexer` reproducing the POPL'14 Optimal-DPOR benchmark counts.

[Unreleased]: https://github.com/egnees/interweave/commits/master

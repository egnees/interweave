# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Deterministic single-threaded executor driving processes written as `Future`s.
- `Atomic` synchronization primitive with `load` / `store` / `compare_exchange`,
  each an `.await` scheduling point.
- `explore` entry point with two strategies: naive exhaustive `Strategy::Dfs` and
  `Strategy::Optimal` (Optimal DPOR, Abdulla et al., POPL'14).
- `Observer` hook called at every explored state (`&mut ()` to observe nothing).
- Examples: `find_bug`, and `readers` / `lastzero` / `indexer` reproducing the
  POPL'14 Optimal-DPOR benchmark counts.

[Unreleased]: https://github.com/egnees/interweave/commits/master

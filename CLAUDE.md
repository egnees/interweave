# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

Research sandbox for **stateless model checking** algorithms — specifically **DPOR** (Dynamic Partial Order Reduction) and **Optimal DPOR**. The goal is to *see* which interleavings these algorithms explore on small concurrent programs.

The experiment models concurrent processes as Rust `Future`s driven by a custom single-threaded executor. Synchronization primitives (`Atomic*`, `Mutex`, channels, …) are implemented *from scratch* so that every memory operation that may interact with another process becomes an explicit **scheduling point** — a `.await` that hands control back to the executor. The executor itself stays deliberately *dumb*: a deterministic driver that just runs whichever process is enabled. Control over *which* interleaving happens lives in the synchronization primitives — they decide, via wakers, who is enabled and who stays blocked — and the DPOR algorithm drives those decisions to prune provably-equivalent interleavings.

Planned direction:
1. Finish the executor so it can run a fixed set of processes to completion deterministically. *(done — see Architecture)*
2. Build custom `Atomic` / `Mutex` / channel primitives whose every observable operation is a yield point with enough metadata (process id, location, operation kind, target) for the algorithm to compute happens-before / dependency relations. *(in progress — `atomic` has load/store/CAS)*
3. Implement scheduling strategies — naive exhaustive enumeration first, then DPOR, then Optimal DPOR — as drivers around the executor. *(naive DFS done — `explore`)*
4. Add visualization of the explored interleaving tree / happens-before graphs.

Useful background for anyone working here: Flanagan & Godefroid "Dynamic Partial-Order Reduction for Model Checking Software" (POPL'05); Abdulla et al. "Optimal Dynamic Partial Order Reduction" (POPL'14) and the *Source DPOR* / *Optimal DPOR* line of papers; Nidhugg and rcmc as reference implementations.

## Commands

- Build: `cargo build`
- Test all: `cargo test`
- Run a single test: `cargo test <test_name>` (e.g. `cargo test it_works`)
- Show stdout from passing tests: `cargo test -- --nocapture`
- Lint: `cargo clippy --all-targets`
- Format: `cargo fmt`

Rust **edition 2024**, so a recent toolchain is required. The only declared dependency is `thiserror` (derive-only, for the error boilerplate) — there is deliberately **no async runtime**: per-process wakers are built from `std::task::Wake`. Do not add `tokio` / `futures` / `async-std` or reach for any ready-made scheduler — the whole point is to control scheduling ourselves. A crate is fair game only if it touches neither scheduling nor the yield-point semantics (`thiserror` qualifies; a vector-clock crate for happens-before later would too).

## Architecture

Six modules across the three layers; only `Handle`, `World`, `explore`, `FailedState`, `ProcessID`, `ProcessResult` are re-exported from `lib.rs` — everything else is `pub(crate)` or private.

- **`process`** (shared vocabulary) — the `Pid` (process id) and `Result` (`Result<(), Box<dyn Error>>`, a process future's output) aliases, re-exported as `ProcessID` / `ProcessResult`.
- **`executor`** (mechanics) — the deterministic driver: `Executor`, the `ProcessError` failure, the current-process `pid()` hook.
- **`object`** (shared vocabulary) — `Oid`, the `Object` trait (`apply` / `enabled`), and `Transition { pid, oid, seq }`, the unit the strategy picks.
- **`atomic`** (sync primitive) — `Atomic` + the cloneable `Handle`; `load` / `store` / `compare_exchange` are `.await` yield points. Implements `Object`.
- **`state`** (strategy) — `World` (builder owning the objects + executor, with `spawn` / `atomic`), `State` (a search-tree node: world + the setup closure + a `trace`), and `StateView` (a cloneable prefix — setup + trace — replayed back into a full `State` because futures can't be cloned).
- **`explore`** (strategy) — `explore()` entry point (returns the interleaving count or the first failure), the recursive `dfs`, and `FailedState` (a reproducible failure: a `StateView` + a `FailureReason` of `Process` | `Deadlock`, with `play()` to replay it).

`executor` is single-threaded and **deterministic**. No `unsafe`, nothing from an async runtime (per-process wakers come from `std::task::Wake`).

- `Process` = `Pin<Box<dyn Future<Output = process::Result>>>` + a cached per-process `Waker`. Completion is dropping the process (the `Vec` slot becomes `None`); a process is *runnable* exactly when its id is in the `queue`.
- `Executor` state: `processes: Vec<Option<Process>>` (the vector index *is* the `Pid`), a FIFO `queue: VecDeque<Pid>`, and a shared `Inbox { woken: Mutex<VecDeque<Pid>> }` behind an `Arc`.
- `Executor::schedule(future) -> Pid` registers a process (id == push index), builds its `ProcessWaker`, and enqueues it.
- `Executor::execute(&mut self) -> Result<(), ProcessError>` runs the poll loop until the queue drains: a leading `flush_wakes()` re-queues woken processes, then pop / poll once. A process `Err` becomes `ProcessError { pid, source }`. It does **not** detect deadlock — the strategy does, via `pending()` (count of live, un-dropped slots) vs `enabled()`.
- Per-process waker (`ProcessWaker`, via `std::task::Wake`): `wake()` only pushes the id into the shared `Inbox`. `flush_wakes()` re-queues woken live processes in ascending-pid order (deduped, skipping any already queued) — the sort keeps the FIFO tie-break replay-stable however the wakes fired.
- `pid() -> Pid` reads a `thread_local!` that `execute` sets around each poll (panic-safe via an RAII `Guard`) and panics outside execution. This is the hook the sync primitives use to stamp `Transition`s.

The executor is deliberately **dumb**: *no* DPOR / checkpoint / strategy logic and *no* strategy hook. Its only ordering policy is the FIFO `queue`, a deterministic tie-break, not a scheduling decision. All real interleaving control lives in the sync primitives (and the strategy layer above), which decide who is enabled by controlling wakers.

The strategy reconstructs states by **replay**: a `StateView` (setup + trace) rebuilds a full `State` via `StateView::state` — re-running the setup closure and re-applying the trace from scratch (futures cannot be cloned); `State::fork` is just `view().state()`. This is deterministic only because object/transition ids are assigned in fixed insertion order — keep that invariant when adding primitives.

### Where this is heading

- Each step of a process between scheduling points is treated as *atomic* by the model checker. The custom synchronization primitives are responsible for defining what counts as a step — i.e. inserting `.await` points at exactly the operations DPOR needs to reason about (loads, stores, RMWs, lock acquire/release, send/recv).
- The executor must be **deterministic given a schedule**: same processes + same scheduling decisions ⇒ identical execution. This is what lets DPOR replay prefixes and explore alternative continuations.
- The scheduling-strategy layer sits above the executor and decides, at each scheduling point, which process is allowed to advance — enacted *through* the synchronization primitives' wakers, not by reaching into the executor. Naive DFS, DPOR, and Optimal DPOR are all instances of this layer.
- Recording an execution should produce a trace of `(process, operation)` events rich enough to compute happens-before and the dependency relations DPOR needs.

When adding new code, keep these concerns separated: **executor mechanics** (futures, wakers, queues) vs. **synchronization primitives** (the yield-point library) vs. **scheduling strategy / exploration algorithm** (the model checker proper).

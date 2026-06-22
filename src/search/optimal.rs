//! Optimal DPOR (Abdulla et al., POPL'14) as a replay-once driver: explores one
//! interleaving per Mazurkiewicz (happens-before) class with no sleep-set blocking.
//!
//! A per-prefix *wakeup tree* of reversing fragments plus a *sleep set* replace
//! classical DPOR's persistent set; races are analysed only on maximal traces via
//! vector-clock happens-before. One live `State` (`cur`) is stepped forward in
//! place during descent — ancestors survive only as metadata `Frame`s — and
//! backtracking rebuilds `cur` with a single root replay of the surviving prefix.

use std::collections::BTreeSet;

use super::explore::FailedState;
use super::observer::Observer;
use super::step::{RaceOutcome, Step, StepCx};
use crate::model::{State, StateView, Transition};

// One wakeup-tree node: children in ≺ (sibling) order, `children[0]` the ≺-minimal
// (explored first); `graft`/`insert` append new branches at the back. Each edge
// carries its planned transition for dependency tests but is matched by `pid` (the
// per-object `seq` is not stable across interleavings). For atomics `depends` is a
// pure function of the two ops' kinds, so a carried edge resolves identically in any
// state where both occur; for channels the send/recv case keys on the recv's
// consumed send-seq, which drifts across interleavings (a racy upstream op shifts a
// later send's seq), so `check_initial` re-stamps a carried edge to a live op of the
// same pid/object before testing it against the current trace.
//
// `pub(super)` (with a read accessor below) so the step-instrumentation module can
// expose the live frontier tree through `WakeupNode`; the field stays private to this
// module's writers and `Wut` itself never leaves `search`.
#[derive(Default)]
pub(super) struct Wut {
    children: Vec<(Transition, Wut)>,
}

impl Wut {
    // Read access for step instrumentation: each child's planned edge and subtree.
    // Wrapped by `WakeupNode` so a consumer never names `Wut`.
    pub(super) fn children(&self) -> &[(Transition, Wut)] {
        &self.children
    }

    // Appends `seq` as a fresh branch after the existing children. Navigation
    // matches by head pid, so siblings must have distinct pids — `insert` only
    // reaches `graft` for a new pid.
    fn graft(&mut self, seq: &[Transition]) {
        let Some((&head, rest)) = seq.split_first() else {
            return;
        };
        debug_assert!(
            self.children.iter().all(|(t, _)| t.pid != head.pid),
            "wakeup-tree siblings must have distinct pids"
        );
        let mut branch = Wut::default();
        branch.graft(rest);
        self.children.push((head, branch));
    }
}

// DFS-stack metadata for the prefix E it describes, captured while E was live so
// the search can reason about E after stepping past it. `sleep` is sleep(E)
// (filtered into children on descent, grown on backtrack); `pending` is each
// enabled process's next op. `frames[m]` describes `trace[..m]`, so
// `frames.len() == prefix.len() + 1` (the root frame is always present).
//
// `pub(super)` (with read accessors below) so the step-instrumentation module can
// expose each depth's sleep + pending through `StepCx`; the fields stay private.
pub(super) struct Frame {
    sleep: Vec<usize>,
    pending: Vec<Transition>,
}

impl Frame {
    pub(super) fn sleep(&self) -> &[usize] {
        &self.sleep
    }

    pub(super) fn pending(&self) -> &[Transition] {
        &self.pending
    }
}

pub(super) fn run<'a>(
    root: State<'a>,
    observer: &mut impl Observer,
) -> Result<(), FailedState<'a>> {
    observer.observe(&root);
    if root.is_failed() {
        let (reason, view) = root.into_failure();
        return Err(FailedState::new(reason, view));
    }

    // The view carries `setup`; replays rebuild `cur` from a transition prefix.
    let view = root.view();

    let mut tree = Wut::default();
    let mut frames: Vec<Frame> = vec![Frame {
        sleep: Vec::new(),
        pending: root.enabled(),
    }];
    // Seed the root with one enabled process (its sleep set is empty).
    if let Some(p) = seed(&root, &[]) {
        let seeded = resolve(&root, p).expect("a seeded process must be runnable");
        tree.graft(&[seeded]);
        observer.step(
            Step::RootSeed { seeded },
            StepCx::new(&tree, &frames, &[], &root),
        );
    } else {
        observer.step(Step::RootEmpty, StepCx::new(&tree, &frames, &[], &root));
        observer.step(Step::Done, StepCx::new(&tree, &frames, &[], &root));
    }

    let mut cur = root;
    // `cur`'s transition prefix: push on apply, pop on backtrack. Its length is the
    // depth of `cur`'s node (a path of ≺-minimal children), and it feeds the single
    // root replay on backtrack. `prefix.len() == frames.len() - 1`.
    let mut prefix: Vec<Transition> = Vec::new();
    let mut need_replay = false;

    loop {
        // An empty node means this frame is fully explored; popping past the root
        // ends the search.
        if node_at(&tree, prefix.len()).children.is_empty() {
            if pop_exhausted(&mut tree, &mut frames, &mut prefix, &cur, observer) {
                observer.step(Step::Done, StepCx::new(&tree, &frames, &prefix, &cur));
                return Ok(());
            }
            need_replay = true;
            continue;
        }

        if need_replay {
            cur = view.replay(prefix.clone());
            need_replay = false;
            observer.step(
                Step::Replay { prefix: &prefix },
                StepCx::new(&tree, &frames, &prefix, &cur),
            );
        }

        // Explore the ≺-minimal child, re-resolving its op against the live `cur`
        // so replay (not the stored seq) drives which concrete op runs.
        let p = node_at(&tree, prefix.len()).children[0].0.pid;
        let p_t = resolve(&cur, p).expect("a wakeup-tree branch must be runnable");
        debug_assert!(
            cur.enabled().contains(&p_t),
            "a committed op must be enabled"
        );

        // Sleep' for the child, computed on the LIVE parent BEFORE the in-place
        // apply below — the one ordering hazard of replay-once.
        let child_sleep = child_sleep_set(&cur, frames.last().unwrap(), p_t);

        cur.apply(p_t);
        observer.observe(&cur);
        if cur.is_failed() {
            // A failed leaf is a maximal trace too; emit it (label-able: it is now
            // committed) and abort the search at it. `cur.trace()` already includes
            // the just-applied op, so it is exactly the maximal trace.
            observer.step(
                Step::Maximal {
                    trace: cur.trace(),
                    failure: true,
                },
                StepCx::new(&tree, &frames, &prefix, &cur),
            );
            let (reason, view) = cur.into_failure();
            return Err(FailedState::new(reason, view));
        }

        // depth == prefix.len() BEFORE the push; `committed` is now applied, so it is
        // label-able. `parent_sleep` is still the parent frame (the push is below); a
        // consumer derives the dropped sleepers as `parent_sleep \ child_sleep`.
        observer.step(
            Step::Descend {
                depth: prefix.len(),
                committed: p_t,
                parent_sleep: frames.last().unwrap().sleep(),
                child_sleep: &child_sleep,
            },
            StepCx::new(&tree, &frames, &prefix, &cur),
        );

        frames.push(Frame {
            sleep: child_sleep,
            pending: cur.enabled(),
        });
        prefix.push(p_t);

        if cur.enabled().is_empty() {
            // A maximal trace: plan every reversible race's reversal into the
            // ancestor wakeup trees. `prefix` now includes the just-pushed op, so it
            // is the maximal trace.
            observer.step(
                Step::Maximal {
                    trace: &prefix,
                    failure: false,
                },
                StepCx::new(&tree, &frames, &prefix, &cur),
            );
            plan_reversals(&mut tree, &frames, &prefix, &cur, &view, observer);
            debug_assert!(
                node_at(&tree, prefix.len()).children.is_empty(),
                "a maximal trace's wakeup-tree node has no continuations"
            );
            need_replay = true;
        } else {
            seed_child(&mut tree, &cur, &frames, &prefix, observer);
        }
    }
}

// Pops the exhausted top frame: drops the finished (≺-minimal) branch from the
// parent and sleeps its head (Algorithm 2 line 17). Returns `true` when the popped
// frame was the root, i.e. the whole search is done.
fn pop_exhausted(
    tree: &mut Wut,
    frames: &mut Vec<Frame>,
    prefix: &mut Vec<Transition>,
    cur: &State,
    observer: &mut impl Observer,
) -> bool {
    let from_depth = prefix.len();
    frames.pop();
    let Some(finished_t) = prefix.pop() else {
        return true;
    };
    let finished = finished_t.pid;
    let parent = node_at_mut(tree, prefix.len());
    debug_assert_eq!(
        parent.children.first().map(|(t, _)| t.pid),
        Some(finished),
        "the finished branch must be the ≺-minimal child"
    );
    parent.children.remove(0);
    frames.last_mut().unwrap().sleep.push(finished);
    // Surface the line-17 sleep growth: `finished` was slept into the parent frame.
    observer.step(
        Step::Pop {
            finished_pid: finished,
            from_depth,
            into_depth: from_depth - 1,
        },
        StepCx::new(tree, frames, prefix, cur),
    );
    false
}

// Sleep' = { q ∈ sleep(E) | next(q) independent of p's step }. The parent's sleep
// is read live (line-17 pops may have grown it) and each q is resolved against
// `cur`.
fn child_sleep_set(cur: &State, parent: &Frame, p_t: Transition) -> Vec<usize> {
    parent
        .sleep
        .iter()
        .copied()
        .filter(|&q| match resolve(cur, q) {
            Some(q_t) => !cur.depends(p_t, q_t),
            None => false,
        })
        .collect()
}

// Seeds the child node with one enabled non-sleeping process, unless a race
// already planted a fragment there. `depth == prefix.len()`; `child_sleep` is the
// top frame's sleep. The emitted `seeded` is whatever process now heads the child
// node — the freshly grafted one, or the one a race already planted — and `None`
// only when the node is empty and nothing is runnable.
fn seed_child(
    tree: &mut Wut,
    cur: &State,
    frames: &[Frame],
    prefix: &[Transition],
    observer: &mut impl Observer,
) {
    let depth = prefix.len();
    let child_sleep = &frames.last().unwrap().sleep;
    if node_at(tree, depth).children.is_empty() {
        if let Some(q) = seed(cur, child_sleep) {
            debug_assert!(
                !child_sleep.contains(&q),
                "sleep-set-blocked state under Optimal DPOR"
            );
            let q_t = resolve(cur, q).expect("a seeded process must be runnable");
            node_at_mut(tree, depth).graft(&[q_t]);
        }
    }
    // The child node's head edge after seeding (matched by pid; its seq drifts,
    // so a consumer reads only the pid).
    let seeded = node_at(tree, depth).children.first().map(|(t, _)| *t);
    observer.step(
        Step::SeedChild { depth, seeded },
        StepCx::new(tree, frames, prefix, cur),
    );
}

// Plans the reversal of every reversible race in the maximal trace `state`: for a
// race (e, e') it inserts v = notdep(e, E).proc(e') into the wakeup tree of the
// prefix just before e, unless a process already asleep there covers it.
// `frames[m]` describes `trace[..m]`, so E' = pre(E, e) for event i (1-based) is
// `frames[i-1]` and its wut node sits at depth i-1.
fn plan_reversals(
    tree: &mut Wut,
    frames: &[Frame],
    trace: &[Transition],
    state: &State,
    view: &StateView,
    observer: &mut impl Observer,
) {
    let n = trace.len();
    let clocks = event_clocks(state, trace);

    for j in 1..=n {
        for i in 1..j {
            if !reversible_race(state, &clocks, trace, i, j) {
                continue;
            }
            let mut v = notdep(&clocks, trace, i);
            v.push(trace[j - 1]);
            // Non-disabling check (POPL'14 reversibility): the reversal is only legal if proc(e')
            // can actually run e' at the reordered prefix. For atomics this always holds (ops never
            // block); for a consuming send→recv it fails — e' (the recv) needs e (the send) to have
            // enqueued its message, so removing e disables e'. Done by replay so the strategy stays
            // primitive-agnostic, asking the model through `enabled` rather than knowing channels.
            //
            // This if/else IS the algorithm (the same short-circuit `runnable_after` →
            // `covered_by_sleeper` → `insert`); it merely also names the branch as a
            // `RaceOutcome` (a free stack enum).
            let outcome = if !runnable_after(view, trace, &v, i) {
                RaceOutcome::NonDisabling
            } else if let Some(covering_pid) = covered_by_sleeper(state, &frames[i - 1], &v) {
                RaceOutcome::CoveredBySleeper {
                    insert_depth: i - 1,
                    covering_pid,
                }
            } else {
                match insert(node_at_mut(tree, i - 1), state, &v) {
                    InsertResult::ExistingLeaf => RaceOutcome::ExistingLeaf {
                        insert_depth: i - 1,
                    },
                    InsertResult::Grafted => RaceOutcome::Grafted {
                        insert_depth: i - 1,
                    },
                }
            };
            // `v` is still alive (insert took `&v`); borrow it and its notdep prefix.
            observer.step(
                Step::Race {
                    i,
                    j,
                    e: trace[i - 1],
                    ep: trace[j - 1],
                    notdep: &v[..v.len() - 1],
                    v: &v,
                    outcome,
                },
                StepCx::new(tree, frames, trace, state),
            );
        }
    }
}

// Algorithm 2 line 6: which process already asleep at prefix `pre` is a weak-initial
// of `v` (covering the reversal), or `None` if none. `pending` is `cur.enabled()`
// captured at `pre`, so a blocked recv is already excluded (it is not enabled) —
// exactly the set this check needs.
fn covered_by_sleeper(state: &State, pre: &Frame, v: &[Transition]) -> Option<usize> {
    pre.sleep.iter().copied().find(|&q| {
        pre.pending
            .iter()
            .copied()
            .find(|t| t.pid == q)
            .is_some_and(|q_t| check_initial(state, q_t, v).is_some())
    })
}

// The terminal shape of an `insert`: a fresh branch was grafted, or an existing leaf
// already covered v. Returned so the step hook can tell the two apart without
// changing `insert`'s behavior.
enum InsertResult {
    Grafted,
    ExistingLeaf,
}

// insert[E'](v): descends the branch that is the longest weak-initial prefix of v
// (stripping each matched process), then grafts the residual as a new branch.
// Dependency tests run against the maximal `state` (the carried transitions resolve
// identically there), so no replay is needed while descending.
fn insert(node: &mut Wut, state: &State, v: &[Transition]) -> InsertResult {
    for idx in 0..node.children.len() {
        let q_t = node.children[idx].0;
        let Some(rest) = check_initial(state, q_t, v) else {
            continue; // q is not a weak-initial of v: try the next sibling.
        };
        if node.children[idx].1.children.is_empty() {
            return InsertResult::ExistingLeaf; // an existing leaf already covers v.
        }
        return insert(&mut node.children[idx].1, state, &rest);
    }
    node.graft(v);
    InsertResult::Grafted
}

// Re-stamp a carried wakeup-tree edge to an op valid in `state`. The edge's seq is
// the per-object registration index from the interleaving that planted it, which is
// NOT stable across interleavings (a racy upstream op shifts a later send's seq);
// only the op-kind, fixed by the node's prefix, is stable. q's relevant op is its
// first step from that prefix: `v`'s first event of q's pid when present, else (notdep
// filtered it out) q's matching op still occurs in the live maximal trace.
fn restamp(state: &State, q_t: Transition, v: &[Transition]) -> Transition {
    if let Some(&t) = v.iter().find(|t| t.pid == q_t.pid && t.oid == q_t.oid) {
        return t;
    }
    if let Some(&t) = state
        .trace()
        .iter()
        .find(|t| t.pid == q_t.pid && t.oid == q_t.oid)
    {
        return t;
    }
    q_t // no same-(pid,oid) op in state: no same-oid vk reaches kind_of, so safe.
}

// Whether `q_t` (q's next op) is a weak-initial of `v`, and if so the residual (v
// with q's first occurrence removed). The leading re-stamp swaps a carried edge for a
// live op of the same pid/object, since channel seqs drift across interleavings and a
// stale seq would panic in the channel's `kind_of`. Walking v from the front: q is
// blocked if a v-event it depends on comes first; otherwise q's own occurrence (or
// independence from all of v) makes it a weak-initial. q's op is stable while
// stripping events independent of it, and `depends` is evaluated on the maximal
// `state` where every event resolves — so no replay is needed.
fn check_initial(state: &State, q_t: Transition, v: &[Transition]) -> Option<Vec<Transition>> {
    let q_t = restamp(state, q_t, v);
    for (k, &vk) in v.iter().enumerate() {
        if vk.pid == q_t.pid {
            let mut rest = v.to_vec();
            rest.remove(k);
            return Some(rest);
        }
        if state.depends(vk, q_t) {
            return None;
        }
    }
    Some(v.to_vec())
}

// notdep(e, E): the events after e (index i, 1-based) that do not happen-after e,
// i.e. e's index is not in their vector clock (clocks[k][proc(e)] < i).
fn notdep(clocks: &[Vec<usize>], trace: &[Transition], i: usize) -> Vec<Transition> {
    let e = trace[i - 1];
    (i + 1..=trace.len())
        .filter(|&k| clocks[k][e.pid] < i)
        .map(|k| trace[k - 1])
        .collect()
}

// Whether (trace[i-1], trace[j-1]) is a reversible race (i < j, 1-based): different
// processes, dependent, and a *direct* happens-before edge (no causal
// intermediary). The non-disabling side of reversibility is checked separately in
// `plan_reversals` via `runnable_after` (it needs the candidate reordering `v`).
fn reversible_race(
    state: &State,
    clocks: &[Vec<usize>],
    trace: &[Transition],
    i: usize,
    j: usize,
) -> bool {
    let (e, ep) = (trace[i - 1], trace[j - 1]);
    if e.pid == ep.pid || !state.depends(e, ep) || !happens_before(clocks, trace, i, j) {
        return false;
    }
    // Direct: no m with e →_E m →_E e' (such m lies strictly between i and j).
    !(i + 1..j).any(|m| happens_before(clocks, trace, i, m) && happens_before(clocks, trace, m, j))
}

// Whether proc(e') can actually run e' at the reversed prefix, i.e. the reordering
// `pre(E,e)·notdep` leaves proc(e') enabled (e' = `v`'s last element). Replays
// `trace[..i-1]` (a valid prefix) then re-resolves the notdep events by pid against
// the live state — the same pid-driven replay the driver uses, so data-dependent
// control flow re-resolves correctly — and checks proc(e') is enabled at the end.
// A no-op for non-blocking primitives (a notdep step always re-resolves and proc(e')
// stays enabled); it only rejects a reversal whose later event was *enabled by* the
// event being moved past it (a channel rf edge: removing the send blocks the recv).
fn runnable_after(view: &StateView, trace: &[Transition], v: &[Transition], i: usize) -> bool {
    let (ep, notdep) = v.split_last().expect("v ends with e'");
    let mut state = view.replay(trace[..i - 1].to_vec());
    for nd in notdep {
        let Some(t) = resolve(&state, nd.pid) else {
            return false;
        };
        state.apply(t);
        if state.is_failed() {
            return false; // the reordering errs before e' — e' can't run.
        }
    }
    state.enabled().iter().any(|t| t.pid == ep.pid)
}

// i →_E j (event i happens-before event j), for i < j: i ≤ clocks[j][proc(event i)].
fn happens_before(clocks: &[Vec<usize>], trace: &[Transition], i: usize, j: usize) -> bool {
    i <= clocks[j][trace[i - 1].pid]
}

// Per-event vector clocks. clocks[k] (k in 1..=n) is the k-th event's clock;
// clocks[0] is ⊥. Each clock starts from its process's previous clock (program
// order) and merges every dependent predecessor, then sets its own component to k.
// The program-order seed keeps two same-process ops ordered even when the
// dependency relation calls them independent (e.g. two loads).
fn event_clocks(state: &State, trace: &[Transition]) -> Vec<Vec<usize>> {
    let procs = state.world().processes().len();
    let n = trace.len();
    let mut last = vec![vec![0usize; procs]; procs]; // last[p] = process p's current clock
    let mut clocks: Vec<Vec<usize>> = Vec::with_capacity(n + 1);
    clocks.push(vec![0; procs]);
    for k in 1..=n {
        let t = trace[k - 1];
        let mut clock = last[t.pid].clone();
        for j in 1..k {
            if state.depends(trace[j - 1], t) {
                for (c, &pred) in clock.iter_mut().zip(&clocks[j]) {
                    *c = (*c).max(pred);
                }
            }
        }
        clock[t.pid] = k;
        last[t.pid] = clock.clone();
        clocks.push(clock);
    }
    clocks
}

// One enabled process not in `sleep` (least pid, for determinism), or `None` at a
// maximal state. Optimal DPOR never sleep-set-blocks; the debug_assert guards that,
// and release falls back to the least enabled process for soundness.
fn seed(state: &State, sleep: &[usize]) -> Option<usize> {
    let enabled = enabled_pids(state);
    if enabled.is_empty() {
        return None;
    }
    debug_assert!(
        enabled.iter().any(|p| !sleep.contains(p)),
        "sleep-set-blocked state under Optimal DPOR"
    );
    enabled
        .iter()
        .find(|p| !sleep.contains(p))
        .or_else(|| enabled.iter().next())
        .copied()
}

fn enabled_pids(state: &State) -> BTreeSet<usize> {
    state.enabled().into_iter().map(|t| t.pid).collect()
}

// Process p's next op, or `None` if p has finished.
fn resolve(state: &State, p: usize) -> Option<Transition> {
    state.enabled().into_iter().find(|t| t.pid == p)
}

// The node `depth` ≺-minimal children below `root` (the all-front-child descent the
// driver always follows; `depth == prefix.len()`).
fn node_at(root: &Wut, depth: usize) -> &Wut {
    let mut n = root;
    for _ in 0..depth {
        n = &n.children[0].1;
    }
    n
}

fn node_at_mut(root: &mut Wut, depth: usize) -> &mut Wut {
    let mut n = root;
    for _ in 0..depth {
        n = &mut n.children[0].1;
    }
    n
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::fmt::Debug;

    use super::{State, StateView, event_clocks, happens_before};
    use crate::Atomic;
    use crate::model::World;
    use crate::search::{FailedState, Observer, explore};

    // Exhaustive DFS over every interleaving — the ground-truth oracle Optimal is
    // checked against. Mirrors the old public driver, kept test-only.
    fn dfs<'a>(state: State<'a>, observer: &mut impl Observer) -> Result<(), FailedState<'a>> {
        observer.observe(&state);
        if state.is_failed() {
            let (reason, view) = state.into_failure();
            return Err(FailedState::new(reason, view));
        }
        for t in state.enabled() {
            let mut next = state.fork();
            next.apply(t);
            dfs(next, observer)?;
        }
        Ok(())
    }

    fn dfs_explore<'a>(
        setup: &'a dyn Fn(&mut World<'a>),
        observer: &mut impl Observer,
    ) -> Result<(), FailedState<'a>> {
        dfs(StateView::new(setup).state(), observer)
    }

    // --- spawn helpers ----------------------------------------------------------
    // Each fixture is a fixed set of processes, each a short sequence of atomic ops
    // ending in `Ok(())`. These wrap the spawn-a-future-of-one-op boilerplate.

    fn spawn_store<'a, T>(world: &mut World<'a>, name: impl Into<String>, cell: Atomic<T>, value: T)
    where
        T: Copy + PartialEq + Debug + 'static,
    {
        world.spawn(name, async move {
            cell.store(value).await;
            Ok(())
        });
    }

    fn spawn_load<'a, T>(world: &mut World<'a>, name: impl Into<String>, cell: Atomic<T>)
    where
        T: Copy + PartialEq + Debug + 'static,
    {
        world.spawn(name, async move {
            cell.load().await;
            Ok(())
        });
    }

    // A writer publishing `load(src) + 1` into `dst` — the lastzero(N) writer shape.
    fn spawn_increment<'a>(
        world: &mut World<'a>,
        name: impl Into<String>,
        src: Atomic<i32>,
        dst: Atomic<i32>,
    ) {
        world.spawn(name, async move {
            let v = src.load().await;
            dst.store(v + 1).await;
            Ok(())
        });
    }

    // `count` distinct zero-initialized atomics `<prefix>0..<prefix>(count-1)`.
    // Distinct objects (distinct oids) are what make two cells independent.
    fn cells<'a>(world: &mut World<'a>, prefix: &str, count: usize) -> Vec<Atomic<i32>> {
        (0..count)
            .map(|i| world.atomic(format!("{prefix}{i}"), 0i32))
            .collect()
    }

    // --- observers / metrics ----------------------------------------------------

    // Counts the leaves (maximal interleavings) a search reaches.
    #[derive(Default)]
    struct Leaves(usize);

    impl Observer for Leaves {
        fn observe(&mut self, state: &State) {
            if state.is_terminal() {
                self.0 += 1;
            }
        }
    }

    fn leaves<'a>(setup: &'a dyn Fn(&mut World<'a>)) -> usize {
        let mut obs = Leaves::default();
        let _ = explore(setup, &mut obs);
        obs.0
    }

    fn dfs_leaves<'a>(setup: &'a dyn Fn(&mut World<'a>)) -> usize {
        let mut obs = Leaves::default();
        let _ = dfs_explore(setup, &mut obs);
        obs.0
    }

    // Ground-truth class count: run exhaustive DFS and canonicalize each maximal
    // trace by its happens-before relation over stable event labels. Equivalent
    // interleavings share one canonical form, so the number of distinct forms is the
    // exact number Optimal DPOR must explore.
    type Label = (usize, usize, usize); // (pid, the process's own op index, oid)
    type Canon = BTreeSet<(Label, Label)>;

    fn canon(state: &State) -> Canon {
        let trace = state.trace();
        let n = trace.len();
        let clocks = event_clocks(state, trace);
        let mut seen = BTreeMap::<usize, usize>::new();
        let mut label = vec![(0usize, 0usize, 0usize); n + 1];
        for k in 1..=n {
            let t = trace[k - 1];
            let idx = seen.entry(t.pid).or_default();
            label[k] = (t.pid, *idx, t.oid);
            *idx += 1;
        }
        let mut hb = Canon::new();
        for j in 1..=n {
            for i in 1..j {
                if happens_before(&clocks, trace, i, j) {
                    hb.insert((label[i], label[j]));
                }
            }
        }
        hb
    }

    #[derive(Default)]
    struct Classes(BTreeSet<Canon>);

    impl Observer for Classes {
        fn observe(&mut self, state: &State) {
            if state.is_terminal() {
                self.0.insert(canon(state));
            }
        }
    }

    fn classes<'a>(setup: &'a dyn Fn(&mut World<'a>)) -> usize {
        let mut obs = Classes::default();
        let _ = dfs_explore(setup, &mut obs);
        obs.0.len()
    }

    // Optimal explores exactly one trace per class, and never more than DFS.
    fn assert_optimal<'a>(setup: &'a dyn Fn(&mut World<'a>)) {
        let opt = leaves(setup);
        assert_eq!(
            opt,
            classes(setup),
            "Optimal must explore one trace per class"
        );
        assert!(
            opt <= dfs_leaves(setup),
            "Optimal must never explore more than DFS"
        );
    }

    // DFS sees `dfs` leaves, Optimal sees `optimal`, and Optimal is one-per-class.
    fn assert_leaves<'a>(setup: &'a dyn Fn(&mut World<'a>), dfs: usize, optimal: usize) {
        assert_eq!(dfs_leaves(setup), dfs, "DFS leaf count");
        assert_eq!(leaves(setup), optimal, "Optimal leaf count");
        assert_optimal(setup);
    }

    // --- fixtures ---------------------------------------------------------------

    // Two loads of one atomic: read/read independent.
    fn two_loaders(world: &mut World) {
        let x = world.atomic("x", 0u32);
        spawn_load(world, "reader-1", x.clone());
        spawn_load(world, "reader-2", x);
    }

    // Stores to two different atomics: independent (different objects).
    fn two_objects(world: &mut World) {
        let x = world.atomic("x", 0u32);
        let y = world.atomic("y", 0u32);
        spawn_store(world, "writer-x", x, 1);
        spawn_store(world, "writer-y", y, 1);
    }

    // Two stores to one atomic: dependent (the cell value differs by order).
    fn two_writers(world: &mut World) {
        let x = world.atomic("x", 0u32);
        spawn_store(world, "writer-1", x.clone(), 1);
        spawn_store(world, "writer-2", x, 2);
    }

    // The reader errors unless it observes the writer's store.
    fn racy(world: &mut World) {
        let x = world.atomic("x", 0u32);
        spawn_store(world, "writer", x.clone(), 1);
        world.spawn("reader", async move {
            if x.load().await == 1 {
                Ok(())
            } else {
                Err("unexpected value".into())
            }
        });
    }

    fn never_finishes(world: &mut World) {
        world.spawn("stuck", async {
            std::future::pending::<()>().await;
            Ok(())
        });
    }

    // Three stores to one cell: all 3! orders inequivalent, so Optimal == DFS.
    fn three_writers(world: &mut World) {
        let x = world.atomic("x", 0u32);
        for i in 1..=3u32 {
            spawn_store(world, format!("w{i}"), x.clone(), i);
        }
    }

    // A writer racing two readers on one cell: the readers commute, each races the
    // writer. DFS walks 3! = 6 orders; the classes are fewer.
    fn one_writer_two_readers(world: &mut World) {
        let x = world.atomic("x", 0u32);
        spawn_store(world, "writer", x.clone(), 1);
        spawn_load(world, "reader-1", x.clone());
        spawn_load(world, "reader-2", x);
    }

    // Flipping the x-race changes *which ops the reader issues*: on x == 0 it loads
    // y, on x == 1 it stores y (itself racing write-y). A reversing fragment from one
    // branch references an op that vanishes on the other — the hardest case for the
    // pid-based wakeup tree, which stores pids and re-resolves the concrete op by
    // replay.
    fn branch_changes_ops(world: &mut World) {
        let x = world.atomic("x", 0u32);
        let y = world.atomic("y", 0u32);
        spawn_store(world, "write-x", x.clone(), 1);
        spawn_store(world, "write-y", y.clone(), 1);
        world.spawn("reader", async move {
            if x.load().await == 0 {
                y.load().await;
            } else {
                y.store(2).await;
            }
            Ok(())
        });
    }

    // Two producers send one value each into one MPSC channel; a consumer recvs both.
    // The two sends race for queue position (send/send dependent); each recv reads
    // from whichever send won, so the only freedom is the enqueue order ⇒ 2 classes.
    fn producer_consumer(world: &mut World) {
        let (tx, rx) = world.channel::<i32>("ch");
        let tx2 = tx.clone();
        world.spawn("producer-1", async move {
            tx.send(1).await;
            Ok(())
        });
        world.spawn("producer-2", async move {
            tx2.send(2).await;
            Ok(())
        });
        world.spawn("consumer", async move {
            rx.recv().await;
            rx.recv().await;
            Ok(())
        });
    }

    // The reply-misrouting bug from examples/rpc_mux: callers share one connection and
    // the reader routes by a shared in_flight slot, so a reply can be delivered to the
    // wrong call when a second caller overwrites the slot first.
    fn rpc_mux(world: &mut World) {
        #[derive(Debug, Clone, Copy)]
        struct Reply {
            id: i32,
            result: i32,
        }
        let in_flight = world.atomic("in_flight", -1);
        let (conn, reader) = world.channel::<Reply>("conn");
        for id in 0..2 {
            let (in_flight, conn) = (in_flight.clone(), conn.clone());
            world.spawn(format!("caller-{id}"), async move {
                in_flight.store(id).await;
                conn.send(Reply {
                    id,
                    result: id * 10,
                })
                .await;
                Ok(())
            });
        }
        world.spawn("reader", async move {
            for _ in 0..2 {
                let frame = reader.recv().await;
                let routed_to = in_flight.load().await;
                if frame.result != routed_to * 10 {
                    return Err(format!(
                        "call {routed_to} received call {}'s result ({})",
                        frame.id, frame.result
                    )
                    .into());
                }
            }
            Ok(())
        });
    }

    // Three producers send one value each; the consumer recvs all three. The sends
    // race for queue position (send/send dependent), so the only freedom is the 3!
    // enqueue orders ⇒ 6 classes.
    fn three_producers(world: &mut World) {
        let (tx, rx) = world.channel::<i32>("ch");
        for i in 1..=3i32 {
            let tx = tx.clone();
            world.spawn(format!("producer-{i}"), async move {
                tx.send(i).await;
                Ok(())
            });
        }
        world.spawn("consumer", async move {
            rx.recv().await;
            rx.recv().await;
            rx.recv().await;
            Ok(())
        });
    }

    // Producer-a sends 1 then 2 (program order fixes 1 before 2); producer-b sends 3.
    // The consumer recvs three times. A Mazurkiewicz class fixes only the consumer's
    // FIFO value sequence = the linearizations of {1<2, 3 free} = {[1,2,3],[1,3,2],
    // [3,1,2]} = exactly 3, hand-counted independent of `depends`.
    fn interleaved(world: &mut World) {
        let (tx, rx) = world.channel::<i32>("ch");
        let tx_a = tx.clone();
        world.spawn("producer-a", async move {
            tx_a.send(1).await;
            tx_a.send(2).await;
            Ok(())
        });
        world.spawn("producer-b", async move {
            tx.send(3).await;
            Ok(())
        });
        world.spawn("consumer", async move {
            rx.recv().await;
            rx.recv().await;
            rx.recv().await;
            Ok(())
        });
    }

    // Two writers race an atomic `x` while two producers race a channel; the consumer
    // recvs both messages. The atomic and the channel are different objects, so their
    // orders are independent: 2 (store order) × 2 (send order) = 4 classes.
    fn mixed(world: &mut World) {
        let x = world.atomic("x", 0i32);
        spawn_store(world, "writer-1", x.clone(), 1);
        spawn_store(world, "writer-2", x, 2);
        let (tx, rx) = world.channel::<i32>("ch");
        let tx2 = tx.clone();
        world.spawn("producer-1", async move {
            tx.send(1).await;
            Ok(())
        });
        world.spawn("producer-2", async move {
            tx2.send(2).await;
            Ok(())
        });
        world.spawn("consumer", async move {
            rx.recv().await;
            rx.recv().await;
            Ok(())
        });
    }

    // The consumer's control flow depends on which message it recvs first: a sentinel
    // makes it load the atomic, otherwise it stores it — racing a writer on that
    // atomic. The channel analog of `branch_changes_ops`: a reversing fragment from
    // one branch references an atomic op that vanishes on the other.
    fn branch_consumer(world: &mut World) {
        let x = world.atomic("x", 0i32);
        let writer = x.clone();
        spawn_store(world, "writer", writer, 5);
        let (tx, rx) = world.channel::<i32>("ch");
        let tx2 = tx.clone();
        world.spawn("producer-1", async move {
            tx.send(0).await; // sentinel
            Ok(())
        });
        world.spawn("producer-2", async move {
            tx2.send(1).await;
            Ok(())
        });
        world.spawn("consumer", async move {
            if rx.recv().await == 0 {
                x.load().await;
            } else {
                x.store(9).await;
            }
            Ok(())
        });
    }

    // The exact program that panicked under Optimal before the `restamp` fix: a racy
    // atomic read sits between sends, so a later send's per-object seq drifts with the
    // g load/store order — the stale-seq case `restamp` repairs. 3! send orders × 2
    // for the g race.
    fn seq_drift(world: &mut World) {
        let g = world.atomic("g", 0i32);
        let (tx, rx) = world.channel::<i32>("ch");
        let tx1 = tx.clone();
        world.spawn("p1", async move {
            tx1.send(1).await;
            Ok(())
        });
        let gw = g.clone();
        spawn_store(world, "writer-g", gw, 5);
        let tx3 = tx.clone();
        world.spawn("p3", async move {
            g.load().await;
            tx3.send(3).await;
            Ok(())
        });
        world.spawn("p2", async move {
            tx.send(2).await;
            Ok(())
        });
        world.spawn("consumer", async move {
            rx.recv().await;
            rx.recv().await;
            rx.recv().await;
            Ok(())
        });
    }

    // --- POPL'14 benchmarks -----------------------------------------------------
    // Direct ports of the paper's `readers` / `lastzero` / `indexer`. Each `.await`
    // is one DPOR step, so these reproduce the paper's *optimal* column (the number
    // of Mazurkiewicz classes Optimal must explore).

    // readers(N): one writer to `x`, N readers of it. store/load dependent, load/load
    // independent, so each reader is independently on one side of the store ⇒ 2^N.
    fn readers(world: &mut World, n: usize) {
        let x = world.atomic("x", 0u32);
        spawn_store(world, "writer", x.clone(), 42);
        for i in 1..=n {
            spawn_load(world, format!("reader-{i}"), x.clone());
        }
    }

    // lastzero(N): cells `a0..=aN` (distinct objects). The reader scans top-down
    // while it reads zero; writer j stores `load(a[j-1]) + 1` into `a[j]`. The
    // reader's data-dependent control flow replays automatically ⇒ (N+3)·2^(N-2).
    fn lastzero(world: &mut World, n: usize) {
        let cells = cells(world, "a", n + 1);
        let rc = cells.to_vec();
        world.spawn("reader", async move {
            let mut i = n;
            loop {
                if rc[i].load().await == 0 {
                    break;
                }
                if i == 0 {
                    break; // guard against underflow; keep the future total
                }
                i -= 1;
            }
            Ok(())
        });
        for j in 1..=n {
            spawn_increment(
                world,
                format!("writer-{j}"),
                cells[j - 1].clone(),
                cells[j].clone(),
            );
        }
    }

    // indexer(NUM_THREADS): 128 hash-table cells, each thread inserts MAX=4 values
    // with `w = (++m)*11 + tid`, `h = (w*7) % 128`, probing h+1 on CAS failure. Our
    // compare_exchange is already an atomic RMW, so the C `cas_mutex[]` is not
    // modeled. Threads collide only once two `w`s hash to the same slot.
    fn indexer(world: &mut World, num_threads: usize) {
        let table = cells(world, "t", 128);
        for tid in 0..num_threads {
            let tab = table.to_vec();
            world.spawn(format!("thread-{tid}"), async move {
                let mut m = 0i32;
                for _ in 0..10 {
                    if m < 4 {
                        m += 1;
                        let w = m * 11 + tid as i32;
                        let mut h = ((w * 7) % 128) as usize;
                        loop {
                            match tab[h].compare_exchange(0, w).await {
                                Ok(_) => break,
                                Err(_) => h = (h + 1) % 128,
                            }
                        }
                    } else {
                        break;
                    }
                }
                Ok(())
            });
        }
    }

    // A hand-built indexer-shaped collision: two threads CAS the same cell with
    // distinct values, the loser probes the next cell. Same CAS-collision logic as
    // indexer but small enough to check against ground truth.
    fn indexer_collision(world: &mut World, n: usize) {
        let cells = cells(world, "c", n + 1);
        for tid in 0..2 {
            let tab = cells.to_vec();
            world.spawn(format!("t{tid}"), async move {
                let w = tid + 1;
                let mut h = 0usize;
                loop {
                    match tab[h].compare_exchange(0, w).await {
                        Ok(_) => break,
                        Err(_) => h += 1,
                    }
                }
                Ok(())
            });
        }
    }

    // --- reduction / optimality -------------------------------------------------

    #[test]
    fn reduces_read_read() {
        assert_leaves(&two_loaders, 2, 1);
    }

    #[test]
    fn reduces_disjoint_objects() {
        assert_leaves(&two_objects, 2, 1);
    }

    #[test]
    fn keeps_dependent_writes() {
        // Two stores to one atomic are inequivalent in either order: no reduction.
        assert_leaves(&two_writers, 2, 2);
    }

    #[test]
    fn keeps_three_writers() {
        // 3! inequivalent orders: Optimal matches DFS exactly (6), none pruned.
        assert_leaves(&three_writers, 6, 6);
    }

    #[test]
    fn reduces_writer_two_readers() {
        assert!(leaves(&one_writer_two_readers) < 6);
        assert_optimal(&one_writer_two_readers);
    }

    #[test]
    fn handles_branch_changing_ops() {
        // Reversing a race changes which ops exist downstream — the hardest case for
        // the pid-based wakeup tree + replay. Still exactly one trace per class.
        assert_optimal(&branch_changes_ops);
    }

    // --- completeness / soundness ----------------------------------------------

    #[test]
    fn finds_the_race() {
        // Optimal still reaches the stale-read failure DFS finds, identically.
        let dfs = dfs_explore(&racy, &mut ()).unwrap_err();
        let opt = explore(&racy, &mut ()).unwrap_err();
        assert_eq!(opt.to_string(), dfs.to_string());
    }

    #[test]
    fn detects_deadlock() {
        let failed = explore(&never_finishes, &mut ()).unwrap_err();
        assert_eq!(failed.to_string(), "deadlock");
    }

    // --- channels ---------------------------------------------------------------

    #[test]
    fn channel_one_per_class() {
        // Two producers + one consumer: only the enqueue order matters (2 classes),
        // and Optimal matches the exhaustive-DFS ground truth exactly.
        assert_optimal(&producer_consumer);
    }

    #[test]
    fn rpc_mux_bug_found_identically() {
        // Differential soundness: DFS and Optimal both find the reply-misrouting bug
        // and report the same failing state.
        let dfs = dfs_explore(&rpc_mux, &mut ()).unwrap_err();
        let opt = explore(&rpc_mux, &mut ()).unwrap_err();
        assert_eq!(opt.to_string(), dfs.to_string());
    }

    #[test]
    fn channel_three_producers() {
        // 3! enqueue orders ⇒ 6 classes.
        assert_leaves(&three_producers, 30, 6);
    }

    #[test]
    fn channel_interleaved_independent_count() {
        // Optimal == 3 is hand-derived from program-order + FIFO (the linearizations
        // of {1<2, 3 free}), independent of the crate's `depends`.
        assert_leaves(&interleaved, 15, 3);
    }

    #[test]
    fn channel_mixed_objects() {
        // Cross-object independence: 2 store orders × 2 send orders = 4 classes.
        assert_leaves(&mixed, 120, 4);
    }

    #[test]
    fn channel_branch_consumer() {
        // The recv'd value flips which atomic op the consumer issues; a reversing
        // fragment references an op that vanishes on the other branch. Just optimal.
        assert_optimal(&branch_consumer);
    }

    #[test]
    fn channel_seq_drift_no_panic() {
        // Pins the stale-seq fix (`restamp`): without it Optimal panics in the
        // channel's `kind_of` on a carried edge whose seq drifted. 3! send orders × 2
        // for the g load/store race.
        assert_optimal(&seq_drift);
        assert_leaves(&seq_drift, 608, 12);
    }

    // --- POPL'14 benchmark counts -----------------------------------------------
    // Small N: full ground-truth check (Optimal == #classes by exhaustive DFS).
    // Large N: assert the paper's exact *optimal* count directly (DFS unreachable).

    #[test]
    fn readers_small() {
        for n in 2..=4 {
            assert_optimal(&|w| readers(w, n));
        }
        assert_eq!(leaves(&|w| readers(w, 2)), 4);
    }

    #[test]
    fn readers_paper_counts() {
        // 2^N classes.
        assert_eq!(leaves(&|w| readers(w, 8)), 256);
        assert_eq!(leaves(&|w| readers(w, 13)), 8192);
    }

    #[test]
    fn lastzero_small() {
        // classes: lastzero(2) = 5, lastzero(3) = 12.
        assert_optimal(&|w| lastzero(w, 2));
        assert_optimal(&|w| lastzero(w, 3));
    }

    // lastzero(4) has ~44k DFS interleavings — borderline slow in debug.
    #[test]
    #[ignore]
    fn lastzero_4_ground_truth() {
        assert_optimal(&|w| lastzero(w, 4));
    }

    #[test]
    fn lastzero_paper_counts() {
        // (N+3)·2^(N-2) classes.
        assert_eq!(leaves(&|w| lastzero(w, 5)), 64);
        assert_eq!(leaves(&|w| lastzero(w, 10)), 3328);
    }

    // ~300M apply-ops: run with `cargo test -- --ignored --release`.
    #[test]
    #[ignore]
    fn lastzero_15_paper_count() {
        assert_eq!(leaves(&|w| lastzero(w, 15)), 147456);
    }

    // The CAS-collision logic indexer relies on, checked against ground truth.
    #[test]
    fn indexer_collision_ground_truth() {
        assert_optimal(&|w| indexer_collision(w, 2));
        assert_optimal(&|w| indexer_collision(w, 3));
    }

    #[test]
    fn indexer_paper_counts() {
        assert_eq!(leaves(&|w| indexer(w, 12)), 8);
    }

    // ~21M apply-ops: run with `cargo test -- --ignored --release`.
    #[test]
    #[ignore]
    fn indexer_15_paper_count() {
        assert_eq!(leaves(&|w| indexer(w, 15)), 4096);
    }
}

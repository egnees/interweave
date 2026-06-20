//! The Optimal-DPOR driver — a replay-once exploration of one interleaving per equivalence class.
//!
//! Implements Optimal Dynamic Partial Order Reduction (Abdulla et al., POPL'14) as a driver
//! around the executor: a per-prefix *wakeup tree* of reversing fragments plus a *sleep set*,
//! with vector-clock happens-before and race analysis performed only on maximal (complete)
//! traces.
//!
//! It is *replay-once*: a single live `State` is stepped forward in place during descent (no
//! per-node fork), ancestors are kept only as lightweight metadata frames, and backtracking
//! rebuilds the live state with one root replay of the surviving transition prefix rather than
//! reconstructing every node. The driver speaks only in `Transition`s and pids.

use std::collections::BTreeSet;

use super::explore::FailedState;
use super::observer::Observer;
use crate::model::{State, Transition};

// Optimal DPOR (Abdulla, Aronis, Jonsson, Sagonas, POPL'14): explores exactly one
// interleaving per Mazurkiewicz (happens-before) equivalence class and never hits
// a sleep-set-blocked state. It replaces classical DPOR's per-state persistent set
// with a per-prefix *wakeup tree* (the reversing fragments still to explore) plus a
// *sleep set*; races are analysed only on maximal (complete) traces, where the
// reversing fragment v = notdep(e,E).proc(e') is fully known.
//
// The wakeup tree is one global ordered tree (≺ = sibling order). A frame's node is
// reached by following its trace from the root, taking the ≺-minimal child at each
// step. Branches are matched by *pid* — a transition's per-object `seq` is not
// stable across interleavings — but each edge also carries its `Transition` so that
// `insert` / `check_initial` evaluate `depends` against the final maximal trace
// without replaying: on a completed trace `depends` is a pure function of the two
// transitions' op kinds, identical in any state where both are resolvable.
//
// Driver shape: ONE live `State` (`cur`) at the current deepest prefix, stepped
// forward in place during descent. Ancestors are kept only as metadata `Frame`s
// (their sleep set, the pending/enabled at that prefix). Backtracking cannot
// un-apply a future, so on each pop-to-a-live-branch the state is rebuilt with a
// single root replay of the surviving transition prefix.

// One node of the wakeup tree: children in ≺ order (Vec order). Each edge carries
// the planned transition (for dependency tests) but is matched by `transition.pid`;
// the front child is the ≺-minimal one (explored first), and `insert` appends new
// branches at the back.
#[derive(Default)]
struct Wut {
    children: Vec<(Transition, Wut)>,
}

impl Wut {
    // Appends `seq` as a fresh linear branch, ordered after the existing children.
    // Navigation matches by first pid, so siblings must have distinct pids —
    // `insert` upholds this (it descends into an existing child whenever `seq`'s
    // head pid matches one, reaching `graft` only for a new pid).
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

// A DFS stack frame: metadata for the prefix E it describes, captured while E's
// state was still live so the search can reason about E after stepping past it.
// `sleep` is sleep(E) (mutable: filtered into children at descent, grown on
// backtrack); `pending` / `enabled` are E's pending and enabled-pid snapshots,
// used by the weak-initial guard against a maximal trace. No live `State` is kept
// per frame — only the single `cur` is. `frames[m]` describes prefix `trace[..m]`,
// so `frames.len() == prefix.len() + 1` (the root frame is always present).
struct Frame {
    sleep: Vec<usize>,
    pending: Vec<Transition>,
    enabled: Vec<usize>,
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
    // Seed the root with one enabled process (its sleep set is empty).
    if let Some(p) = seed(&root, &[]) {
        tree.graft(&[resolve(&root, p).expect("a seeded process must be runnable")]);
    }

    // The root frame (prefix length 0). `frames[m]` describes `trace[..m]`.
    let mut frames: Vec<Frame> = vec![Frame {
        sleep: Vec::new(),
        pending: root.pending_transitions(),
        enabled: enabled_pids(&root).into_iter().collect(),
    }];
    let mut cur = root;
    // The transition prefix of `cur`: push on apply, truncate on pop. Feeds the
    // single root replay on backtrack. `prefix.len() == frames.len() - 1`.
    let mut prefix: Vec<Transition> = Vec::new();
    // Child-index path from the root to `cur`'s node (always the ≺-min child, so
    // all zeros today, but kept general); lockstep with `prefix`.
    let mut cursor: Vec<usize> = Vec::new();
    let mut need_replay = false;

    loop {
        // An empty wut node means this frame is fully explored. Popping it past the
        // root means the whole search is done.
        if node_at(&tree, &cursor).children.is_empty() {
            if pop_exhausted(&mut tree, &mut frames, &mut prefix, &mut cursor) {
                return Ok(());
            }
            need_replay = true;
            continue;
        }

        if need_replay {
            cur = view.replay(prefix.clone());
            need_replay = false;
        }

        // Explore the ≺-minimal (front) child. The edge carries the planned
        // transition, but it is re-resolved against the live `cur` so replay (not
        // the stored seq) drives which concrete op runs.
        let p = node_at(&tree, &cursor).children[0].0.pid;
        let p_t = resolve(&cur, p).expect("a wakeup-tree branch must be runnable");
        debug_assert!(
            cur.enabled().contains(&p_t),
            "a committed op must be enabled"
        );

        // Sleep' for the child, computed on the LIVE `cur` BEFORE the in-place apply
        // below — the single ordering hazard of replay-once: the parent must be
        // snapshot before stepping past it.
        let child_sleep = child_sleep_set(&cur, frames.last().unwrap(), p_t);

        cur.apply(p_t);
        observer.observe(&cur);
        if cur.is_failed() {
            // A failed leaf is a maximal trace too; abort the search at it.
            let (reason, view) = cur.into_failure();
            return Err(FailedState::new(reason, view));
        }
        // The child frame describes the new prefix: `sleep` was computed pre-apply
        // (the hazard), but `pending` / `enabled` are the new prefix's own and so
        // are read from the live `cur` after the apply that produced it.
        frames.push(Frame {
            sleep: child_sleep,
            pending: cur.pending_transitions(),
            enabled: enabled_pids(&cur).into_iter().collect(),
        });
        prefix.push(p_t);
        cursor.push(0);

        if cur.enabled().is_empty() {
            // A maximal trace: plan every reversible race's reversal into the
            // wakeup trees of the ancestor prefixes.
            plan_reversals(&mut tree, &frames, &prefix, &cur);
            debug_assert!(
                node_at(&tree, &cursor).children.is_empty(),
                "a maximal trace's wakeup-tree node has no continuations"
            );
            need_replay = true;
        } else {
            seed_child(&mut tree, &cursor, &cur, &frames.last().unwrap().sleep);
        }
    }
}

// Pops the exhausted top frame: drops the finished (≺-minimal) branch in the parent
// and sleeps its head (Algorithm 2 line 17). Returns `true` when the popped frame
// was the root, i.e. the whole search is done.
fn pop_exhausted(
    tree: &mut Wut,
    frames: &mut Vec<Frame>,
    prefix: &mut Vec<Transition>,
    cursor: &mut Vec<usize>,
) -> bool {
    frames.pop();
    let Some(finished_t) = prefix.pop() else {
        return true;
    };
    let finished = finished_t.pid;
    cursor.pop();
    let parent = node_at_mut(tree, cursor);
    debug_assert_eq!(
        parent.children.first().map(|(t, _)| t.pid),
        Some(finished),
        "the finished branch must be the ≺-minimal child"
    );
    parent.children.remove(0);
    frames.last_mut().unwrap().sleep.push(finished);
    false
}

// Sleep' = { q ∈ sleep(E) | next(q) independent of p's step }. The parent's sleep is
// read live (line-17 pops may have grown it) and each q is resolved against `cur`.
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

// Seeds the child's wut with one enabled non-sleeping process — but only if a race
// did not already plant a fragment there.
fn seed_child(tree: &mut Wut, cursor: &[usize], cur: &State, child_sleep: &[usize]) {
    if !node_at(tree, cursor).children.is_empty() {
        return;
    }
    let Some(q) = seed(cur, child_sleep) else {
        return;
    };
    debug_assert!(
        !child_sleep.contains(&q),
        "sleep-set-blocked state under Optimal DPOR"
    );
    let q_t = resolve(cur, q).expect("a seeded process must be runnable");
    node_at_mut(tree, cursor).graft(&[q_t]);
}

// Plans the reversal of every reversible race in the maximal trace `state`: for a
// race (e, e') it inserts v = notdep(e, E).proc(e') into the wakeup tree of the
// prefix just before e, unless a sleeping process already covers it.
//
// `trace` is the maximal transition sequence; `frames[m]` describes the prefix
// `trace[..m]` (root-inclusive: `frames[0]` is the root). So E' = pre(E, e) for
// event i (1-based) is `frames[i-1]`, and its wut node sits at cursor depth i-1.
fn plan_reversals(tree: &mut Wut, frames: &[Frame], trace: &[Transition], state: &State) {
    let n = trace.len();
    let clocks = event_clocks(state, trace);

    for j in 1..=n {
        for i in 1..j {
            if !reversible_race(state, &clocks, trace, i, j) {
                continue;
            }
            // The reversing fragment v = notdep(e, E).proc(e') and the prefix E' =
            // pre(E, e) it belongs to (frames[i-1]).
            let mut v = notdep(&clocks, trace, i);
            v.push(trace[j - 1]);
            let pre = &frames[i - 1];
            if covered_by_sleeper(state, pre, &v) {
                continue;
            }
            // The wut node for prefix trace[..i-1] is at cursor depth i-1 (≺-min
            // descent), so a path of (i-1) zeros.
            insert(node_at_mut(tree, &vec![0; i - 1]), state, &v);
        }
    }
}

// Guard sleep(E') ∩ WI[E'](v) ≠ ∅ (Algorithm 2 line 6): whether a process already
// asleep at the prefix `pre` is a weak-initial of `v`, in which case the reversal is
// already covered and must not be re-inserted.
fn covered_by_sleeper(state: &State, pre: &Frame, v: &[Transition]) -> bool {
    pre.sleep.iter().any(|&q| {
        let q_t = pre.pending.iter().copied().find(|t| t.pid == q);
        weak_initial(state, q_t, &pre.enabled, v)
    })
}

// insert[E'](v, wut): descends the branch that is the longest weak-initial prefix
// of v (stripping each matched process), then grafts the residual as a new branch.
// Dependency tests run against the final maximal `state` (the carried transitions
// resolve identically there), so no replay/fork is needed while descending.
fn insert(node: &mut Wut, state: &State, v: &[Transition]) {
    for idx in 0..node.children.len() {
        let q_t = node.children[idx].0;
        let Some(rest) = check_initial(state, q_t, v) else {
            continue; // q is not a weak-initial of v: try the next sibling.
        };
        if node.children[idx].1.children.is_empty() {
            return; // an existing leaf covers v.
        }
        insert(&mut node.children[idx].1, state, &rest);
        return;
    }
    node.graft(v);
}

// Whether `q_t` (process q's next op at this prefix) is a weak-initial of `v`, and
// if so the residual (v with q's first occurrence removed). Walks v from the front:
// if a v-event that q's op depends on comes first, q is blocked; if q's own
// occurrence comes first (or q is independent of all of v), it is a weak-initial.
//
// No replay: q's next op never changes while stripping events independent of it
// (that is the weak-initial definition), so the op carried in `q_t` is the same op
// the old fork-and-re-resolve would have found. `depends` is evaluated against the
// final maximal `state`, where every v-event and q_t are resolvable to the same op.
fn check_initial(state: &State, q_t: Transition, v: &[Transition]) -> Option<Vec<Transition>> {
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

// Whether process `q` (its next op `q_t`, or `None` if q already finished / has no
// pending op) is a weak-initial of `v` from the prefix with the given enabled pids.
// A finished process leads nothing. WI also requires q to be currently enabled (its
// op could actually run first); atomics never block, but the guard is explicit.
fn weak_initial(
    state: &State,
    q_t: Option<Transition>,
    enabled: &[usize],
    v: &[Transition],
) -> bool {
    let Some(q_t) = q_t else {
        return false;
    };
    if !enabled.contains(&q_t.pid) {
        return false;
    }
    check_initial(state, q_t, v).is_some()
}

// notdep(e, E): the events after e (index i, 1-based) that do not happen-after e,
// as transitions. Event k is in notdep iff e's index is not in k's vector clock,
// i.e. clocks[k][proc(e)] < i.
fn notdep(clocks: &[Vec<usize>], trace: &[Transition], i: usize) -> Vec<Transition> {
    let e = trace[i - 1];
    (i + 1..=trace.len())
        .filter(|&k| clocks[k][e.pid] < i)
        .map(|k| trace[k - 1])
        .collect()
}

// Whether (trace[i-1], trace[j-1]) is a reversible race (i < j, 1-based): different
// processes, dependent, a *direct* happens-before edge (no causal intermediary),
// and the reversed process could have run instead (co-enabled — always true for
// atomics, which never block).
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
    for m in (i + 1)..j {
        if happens_before(clocks, trace, i, m) && happens_before(clocks, trace, m, j) {
            return false;
        }
    }
    state.co_enabled(e, ep)
}

// i →_E j (event i happens-before event j), for i < j: i ≤ clocks[j][proc(event i)].
fn happens_before(clocks: &[Vec<usize>], trace: &[Transition], i: usize, j: usize) -> bool {
    i <= clocks[j][trace[i - 1].pid]
}

// Per-event vector clocks for a trace. clocks[k] (k in 1..=n) is the clock of the
// k-th event; clocks[0] is ⊥. Each event's clock starts from its process's previous
// clock (program order) and merges every dependent predecessor, then sets its own
// component to k. The program-order seed is what keeps two same-process ops ordered
// even when the dependency relation calls them independent (two loads).
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

// One enabled process not in `sleep` (least pid, for determinism). Returns `None`
// only when no process is enabled (a maximal state). If every enabled process is
// asleep — a sleep-set-blocked state Optimal DPOR must never reach — this asserts
// in debug and, for soundness in release, falls back to the least enabled process.
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

// Process p's next registered op (runnable or not), or `None` if p has finished.
fn resolve(state: &State, p: usize) -> Option<Transition> {
    state.pending_transitions().into_iter().find(|t| t.pid == p)
}

// The wakeup-tree node reached by a child-index path. Each step indexes into
// `children` directly (no pid `find`), so navigation is O(path) of plain indexing;
// the path is the ≺-min descent (all zeros) and is only walked at branch points.
fn node_at<'w>(root: &'w Wut, path: &[usize]) -> &'w Wut {
    let mut n = root;
    for &i in path {
        n = &n.children[i].1;
    }
    n
}

fn node_at_mut<'w>(root: &'w mut Wut, path: &[usize]) -> &'w mut Wut {
    let mut n = root;
    for &i in path {
        n = &mut n.children[i].1;
    }
    n
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{State, event_clocks, happens_before};
    use crate::Atomic;
    use crate::model::World;
    use crate::search::{Observer, Strategy, explore};

    // Counts the leaves (terminal or failed states) a search reaches — i.e. the
    // number of maximal interleavings explored.
    #[derive(Default)]
    struct Leaves(usize);

    impl Observer for Leaves {
        fn observe(&mut self, state: &State) {
            if state.failure_reason().is_some() || state.enabled().is_empty() {
                self.0 += 1;
            }
        }
    }

    fn leaves<'a>(setup: &'a dyn Fn(&mut World<'a>), strategy: Strategy) -> usize {
        let mut obs = Leaves::default();
        let _ = explore(setup, &mut obs, strategy);
        obs.0
    }

    // The ground-truth number of Mazurkiewicz classes: run exhaustive DFS and
    // canonicalize each maximal trace by its happens-before relation over stable
    // event labels (pid, per-process index, object). Equivalent interleavings share
    // one canonical form, so the count of distinct forms is the class count — the
    // exact number Optimal DPOR must explore.
    // A canonical happens-before form: ordered pairs of stable event labels
    // (pid, the process's own op index, object).
    type Label = (usize, usize, usize);
    type Canon = BTreeSet<(Label, Label)>;

    #[derive(Default)]
    struct Classes(BTreeSet<Canon>);

    impl Observer for Classes {
        fn observe(&mut self, state: &State) {
            if !(state.failure_reason().is_some() || state.enabled().is_empty()) {
                return;
            }
            let trace = state.trace();
            let n = trace.len();
            let clocks = event_clocks(state, trace);
            // Stable label per event: (pid, index among the process's own ops, oid).
            let mut seen = std::collections::BTreeMap::<usize, usize>::new();
            let mut label = vec![(0usize, 0usize, 0usize); n + 1];
            for k in 1..=n {
                let t = trace[k - 1];
                let idx = seen.entry(t.pid).or_default();
                label[k] = (t.pid, *idx, t.oid);
                *idx += 1;
            }
            let mut hb = BTreeSet::new();
            for j in 1..=n {
                for i in 1..j {
                    if happens_before(&clocks, trace, i, j) {
                        hb.insert((label[i], label[j]));
                    }
                }
            }
            self.0.insert(hb);
        }
    }

    fn classes<'a>(setup: &'a dyn Fn(&mut World<'a>)) -> usize {
        let mut obs = Classes::default();
        let _ = explore(setup, &mut obs, Strategy::Dfs);
        obs.0.len()
    }

    // Optimal explores exactly one trace per Mazurkiewicz class, and no more than
    // exhaustive DFS — the headline optimality property, checked against ground truth.
    fn assert_optimal<'a>(setup: &'a dyn Fn(&mut World<'a>)) {
        let opt = leaves(setup, Strategy::Optimal);
        let dfs = leaves(setup, Strategy::Dfs);
        assert_eq!(
            opt,
            classes(setup),
            "Optimal must explore one trace per class"
        );
        assert!(opt <= dfs, "Optimal must never explore more than DFS");
    }

    // --- fixtures ---------------------------------------------------------------

    // Two loads of one atomic: read/read independent.
    fn two_loaders(world: &mut World) {
        let x = world.atomic("x", 0u32);
        let (a, b) = (x.clone(), x.clone());
        world.spawn("reader-1", async move {
            a.load().await;
            Ok(())
        });
        world.spawn("reader-2", async move {
            b.load().await;
            Ok(())
        });
    }

    // Stores to two different atomics: independent (different objects).
    fn two_objects(world: &mut World) {
        let x = world.atomic("x", 0u32);
        let y = world.atomic("y", 0u32);
        world.spawn("writer-x", async move {
            x.store(1).await;
            Ok(())
        });
        world.spawn("writer-y", async move {
            y.store(1).await;
            Ok(())
        });
    }

    // Two stores to one atomic: dependent (the cell value differs by order).
    fn two_writers(world: &mut World) {
        let x = world.atomic("x", 0u32);
        let (a, b) = (x.clone(), x.clone());
        world.spawn("writer-1", async move {
            a.store(1).await;
            Ok(())
        });
        world.spawn("writer-2", async move {
            b.store(2).await;
            Ok(())
        });
    }

    // The reader errors unless it observes the writer's store.
    fn racy(world: &mut World) {
        let x = world.atomic("x", 0u32);
        let (w, r) = (x.clone(), x.clone());
        world.spawn("writer", async move {
            w.store(1).await;
            Ok(())
        });
        world.spawn("reader", async move {
            if r.load().await == 1 {
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

    // Three processes, one shared cell, each a store: every order is observable, so
    // all 3! = 6 interleavings are inequivalent — Optimal cannot reduce below DFS.
    fn three_writers(world: &mut World) {
        let x = world.atomic("x", 0u32);
        for i in 1..=3u32 {
            let h = x.clone();
            world.spawn(format!("w{i}"), async move {
                h.store(i).await;
                Ok(())
            });
        }
    }

    // A writer racing two readers on one cell: the readers commute with each other
    // but each races the writer. DFS explores 3! = 6 orders; the classes are fewer.
    fn one_writer_two_readers(world: &mut World) {
        let x = world.atomic("x", 0u32);
        let (w, r1, r2) = (x.clone(), x.clone(), x.clone());
        world.spawn("writer", async move {
            w.store(1).await;
            Ok(())
        });
        world.spawn("reader-1", async move {
            r1.load().await;
            Ok(())
        });
        world.spawn("reader-2", async move {
            r2.load().await;
            Ok(())
        });
    }

    // lastzero-style: a data-dependent reader whose control flow follows racy cells.
    // Two writers each set their own cell; the reader loads both and the second load
    // it issues depends on the first value — a program where classical Source-DPOR
    // sleep-set-blocks but Optimal does not. (A toy; the parametric `lastzero` below
    // is the POPL'14 benchmark.)
    fn lastzero_toy(world: &mut World) {
        let a = world.atomic("a", 0u32);
        let b = world.atomic("b", 0u32);
        let (wa, wb) = (a.clone(), b.clone());
        world.spawn("write-a", async move {
            wa.store(1).await;
            Ok(())
        });
        world.spawn("write-b", async move {
            wb.store(1).await;
            Ok(())
        });
        let (ra, rb) = (a.clone(), b.clone());
        world.spawn("reader", async move {
            if ra.load().await == 0 {
                rb.load().await;
            }
            Ok(())
        });
    }

    // Flipping the x-race changes *which operations the reader issues*: on x == 0 it
    // loads y, on x == 1 it stores y (itself racing write-y). A reversing fragment
    // computed on one branch references an op that vanishes on the other — the
    // hardest case for the pid-based wakeup tree (it stores process ids and
    // re-resolves the concrete op by replay, so the diverged branch resolves its own
    // ops). Stresses pid-vs-seq replay, the vanishing-fragment-event path, and a
    // control-flow-dependent dependency at once.
    fn branch_changes_ops(world: &mut World) {
        let x = world.atomic("x", 0u32);
        let y = world.atomic("y", 0u32);
        let wx = x.clone();
        world.spawn("write-x", async move {
            wx.store(1).await;
            Ok(())
        });
        let wy = y.clone();
        world.spawn("write-y", async move {
            wy.store(1).await;
            Ok(())
        });
        let (rx, ry) = (x.clone(), y.clone());
        world.spawn("reader", async move {
            if rx.load().await == 0 {
                ry.load().await;
            } else {
                ry.store(2).await;
            }
            Ok(())
        });
    }

    // --- POPL'14 benchmarks -----------------------------------------------------
    // Direct ports of the Optimal DPOR paper's `readers` / `lastzero` / `indexer`
    // (canonical C in refs/nidhugg/benchmarks). Each `.await` is one DPOR step, so
    // these reproduce the paper's *optimal* column: the number of Mazurkiewicz
    // classes Optimal must explore.

    // readers(N): one writer storing to `x`, N readers loading it. store/load are
    // dependent, load/load independent, so each reader is independently on one side
    // of the single store ⇒ 2^N classes. The C source's `idx[]` is per-thread arg
    // plumbing, not a race, so it is not modeled.
    fn readers(world: &mut World, n: usize) {
        let x = world.atomic("x", 0u32);
        let w = x.clone();
        world.spawn("writer", async move {
            w.store(42).await;
            Ok(())
        });
        for i in 1..=n {
            let r = x.clone();
            world.spawn(format!("reader-{i}"), async move {
                r.load().await;
                Ok(())
            });
        }
    }

    // lastzero(N): one atomic per cell `a0..=aN` (distinct objects — the oid is what
    // makes two cells independent). The reader scans top-down while it reads zero;
    // writer j stores `load(a[j-1]) + 1` into `a[j]`. The reader's data-dependent
    // control flow replays automatically. ⇒ (N+3)·2^(N-2) classes.
    fn lastzero(world: &mut World, n: usize) {
        let cells: Vec<Atomic<i32>> = (0..=n)
            .map(|i| world.atomic(format!("a{i}"), 0i32))
            .collect();
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
            let r = cells[j - 1].clone();
            let w = cells[j].clone();
            world.spawn(format!("writer-{j}"), async move {
                let v = r.load().await;
                w.store(v + 1).await;
                Ok(())
            });
        }
    }

    // indexer(NUM_THREADS): SIZE=128 hash-table cells (one atomic each), each thread
    // inserts MAX=4 values with `w = (++m)*11 + tid` (pre-increment: m runs 1..=4),
    // `h = (w*7) % 128`, probing h+1 on CAS failure. Our compare_exchange is already
    // an atomic RMW, so the C `cas_mutex[]` is not modeled. Threads collide only once
    // NUM_THREADS is large enough that two `w`s hash to the same slot — that is the
    // only source of dependent ops. Branch on the Ok/Err discriminant, not the value.
    fn indexer(world: &mut World, num_threads: usize) {
        let table: Vec<Atomic<i32>> = (0..128)
            .map(|i| world.atomic(format!("t{i}"), 0i32))
            .collect();
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
    // distinct values, the loser probes the next cell. This exercises the same
    // CAS-collision logic indexer relies on but is small enough to check against
    // ground truth (assert_optimal runs exhaustive DFS, which is astronomical for
    // the real indexer).
    fn indexer_collision(world: &mut World, n: usize) {
        let cells: Vec<Atomic<i32>> = (0..=n)
            .map(|i| world.atomic(format!("c{i}"), 0i32))
            .collect();
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
        assert_eq!(leaves(&two_loaders, Strategy::Dfs), 2);
        assert_eq!(leaves(&two_loaders, Strategy::Optimal), 1);
        assert_optimal(&two_loaders);
    }

    #[test]
    fn reduces_disjoint_objects() {
        assert_eq!(leaves(&two_objects, Strategy::Dfs), 2);
        assert_eq!(leaves(&two_objects, Strategy::Optimal), 1);
        assert_optimal(&two_objects);
    }

    #[test]
    fn keeps_dependent_writes() {
        // Two stores to one atomic are inequivalent in either order: no reduction.
        assert_eq!(leaves(&two_writers, Strategy::Dfs), 2);
        assert_eq!(leaves(&two_writers, Strategy::Optimal), 2);
        assert_optimal(&two_writers);
    }

    #[test]
    fn keeps_three_writers() {
        // 3! inequivalent orders: Optimal matches DFS exactly (6), none pruned.
        assert_eq!(leaves(&three_writers, Strategy::Dfs), 6);
        assert_eq!(leaves(&three_writers, Strategy::Optimal), 6);
        assert_optimal(&three_writers);
    }

    #[test]
    fn reduces_writer_two_readers() {
        // DFS walks all 6; the two readers commute, so there are fewer classes.
        assert!(leaves(&one_writer_two_readers, Strategy::Optimal) < 6);
        assert_optimal(&one_writer_two_readers);
    }

    #[test]
    fn handles_lastzero() {
        // The data-dependent benchmark: still exactly one trace per class.
        assert_optimal(&lastzero_toy);
        assert!(leaves(&lastzero_toy, Strategy::Optimal) <= leaves(&lastzero_toy, Strategy::Dfs));
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
        // Completeness: Optimal still reaches the stale-read failure DFS finds.
        let dfs = explore(&racy, &mut (), Strategy::Dfs).unwrap_err();
        let opt = explore(&racy, &mut (), Strategy::Optimal).unwrap_err();
        assert_eq!(opt.to_string(), dfs.to_string());
    }

    #[test]
    fn detects_deadlock() {
        let failed = explore(&never_finishes, &mut (), Strategy::Optimal).unwrap_err();
        assert_eq!(failed.to_string(), "deadlock");
    }

    // Optimal reaches the same set of leaf outcomes as exhaustive DFS on a clean
    // program — the differential soundness check, beyond bare counts.
    #[test]
    fn same_outcomes_as_dfs() {
        for setup in [
            &two_loaders as &dyn Fn(&mut World),
            &two_objects,
            &two_writers,
            &three_writers,
            &one_writer_two_readers,
            &lastzero_toy,
            &branch_changes_ops,
        ] {
            assert_optimal(setup);
        }
    }

    // --- POPL'14 benchmark counts -----------------------------------------------
    // Small N: full ground-truth check (Optimal == #classes, run by exhaustive DFS).
    // Large N: assert the paper's exact *optimal* count directly (DFS unreachable).

    #[test]
    fn readers_small() {
        assert_optimal(&|w| readers(w, 2));
        assert_optimal(&|w| readers(w, 3));
        assert_optimal(&|w| readers(w, 4));
        assert_eq!(leaves(&|w| readers(w, 2), Strategy::Optimal), 4);
    }

    #[test]
    fn readers_paper_counts() {
        // 2^N classes.
        assert_eq!(leaves(&|w| readers(w, 8), Strategy::Optimal), 256);
        assert_eq!(leaves(&|w| readers(w, 13), Strategy::Optimal), 8192);
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
        assert_eq!(leaves(&|w| lastzero(w, 5), Strategy::Optimal), 64);
        assert_eq!(leaves(&|w| lastzero(w, 10), Strategy::Optimal), 3328);
    }

    // ~300M apply-ops: run with `cargo test -- --ignored --release`.
    #[test]
    #[ignore]
    fn lastzero_15_paper_count() {
        assert_eq!(leaves(&|w| lastzero(w, 15), Strategy::Optimal), 147456);
    }

    // The CAS-collision logic indexer relies on, checked against ground truth on a
    // tiny synthetic fixture (the real indexer is astronomical under DFS).
    #[test]
    fn indexer_collision_ground_truth() {
        assert_optimal(&|w| indexer_collision(w, 2));
        assert_optimal(&|w| indexer_collision(w, 3));
    }

    #[test]
    fn indexer_paper_counts() {
        assert_eq!(leaves(&|w| indexer(w, 12), Strategy::Optimal), 8);
    }

    // ~21M apply-ops: run with `cargo test -- --ignored --release`.
    #[test]
    #[ignore]
    fn indexer_15_paper_count() {
        assert_eq!(leaves(&|w| indexer(w, 15), Strategy::Optimal), 4096);
    }
}

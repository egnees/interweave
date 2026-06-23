//! The [`Observer`] hook the strategy calls as it explores, plus the typed
//! step-instrumentation data it carries.
//!
//! [`Observer::step`] fires at each discrete decision of the Optimal driver (descend,
//! seed, race-reversal, pop, …) and carries the borrowed [`Step`] / [`StepCx`] views
//! below. [`Step::Visit`] is the per-state signal: it fires for every state the search
//! reaches (the root, then each freshly-applied prefix), so a consumer recovers
//! "every state / terminal" via `cx.state()` + `is_terminal()`. `step` is a no-op by
//! default, so plain `explore` pays nothing: every emit site passes a `Step<'_>`
//! referencing data the driver already holds, and an observer that ignores steps reads
//! none of it. These step types are public so a visualizer is built *on top of* the
//! crate — a pure consumer of [`Observer::step`] plus the public `model` surface, the
//! same way any external consumer would.

use super::optimal::{Frame, Wut};
use crate::model::{ObjectID, ProcessID, State, Transition};

/// A hook into the search, called as it explores.
///
/// [`step`](Observer::step) fires at each discrete decision of the Optimal DPOR driver
/// (descend, seed, race-reversal, pop, …), carrying the typed [`Step`] / [`StepCx`]
/// views. One of those decisions, [`Step::Visit`], fires for every state the search
/// reaches — runnable, terminal, or failed — so an implementor can inspect failures,
/// count maximal traces, or record the tree. `step` is a no-op by default, so an
/// observer that watches only a few decisions ignores the rest at zero cost.
///
/// An observer only *reads* the search: it cannot steer it and cannot fail it, so it is
/// not the place to check program properties — express those as checks inside the
/// processes that return `Err`, which [`explore`](crate::explore) reports as a failing
/// interleaving.
pub trait Observer {
    /// Called at each discrete decision of the Optimal DPOR driver (descend, seed,
    /// race-reversal, pop, …), including [`Step::Visit`] for every visited state.
    /// Default no-op: an observer that only watches some steps ignores the rest at
    /// zero cost.
    fn step(&mut self, step: Step<'_>, cx: StepCx<'_, '_>) {
        let _ = (step, cx);
    }
}

/// The default observer: observe nothing.
impl Observer for () {}

// Two lifetimes: `'a` is the (short) borrow at the emit site; `'w` is the `State`'s
// own setup lifetime. They are decoupled because `State<'w>` is invariant over `'w`
// (it holds a `&'w dyn Fn`), so tying them would force the borrow to live as long as
// the whole search.
/// The read-only context accompanying a [`Step`]: the live [`State`] (for resolving
/// names and labels), the committed `prefix`, the per-depth sleep sets and pending
/// operations, and the frontier wakeup tree. Everything is borrowed for the
/// [`step`](Observer::step) call — clone out what you need to keep.
pub struct StepCx<'a, 'w> {
    tree: &'a Wut,
    frames: &'a [Frame],
    prefix: &'a [Transition],
    state: &'a State<'w>,
}

impl<'a, 'w> StepCx<'a, 'w> {
    // Built only by the driver (inside `search`), so it shares the `Wut`/`Frame`
    // visibility ceiling; consumers receive an already-built `StepCx` and read it
    // through the public, neutral accessors below.
    pub(in crate::search) fn new(
        tree: &'a Wut,
        frames: &'a [Frame],
        prefix: &'a [Transition],
        state: &'a State<'w>,
    ) -> Self {
        Self {
            tree,
            frames,
            prefix,
            state,
        }
    }

    /// The live `State` at this step, for resolving names and labels.
    pub fn state(&self) -> &State<'w> {
        self.state
    }

    /// The committed prefix down to the current frontier (the ≺-minimal spine). At a
    /// freshly-advanced [`Visit`](Step::Visit) it is one shorter than `state().trace()`,
    /// which already includes the just-committed transition.
    pub fn prefix(&self) -> &[Transition] {
        self.prefix
    }

    /// A human-readable label for a *committed* transition (e.g. `"load -> 123"`).
    /// Panics on a transition that has not committed.
    pub fn label(&self, t: Transition) -> String {
        self.state.world().label(t)
    }

    /// The name of the process with the given id.
    pub fn process(&self, pid: ProcessID) -> &str {
        &self.state.world().processes()[pid]
    }

    /// The name of the object with the given id.
    pub fn object(&self, oid: ObjectID) -> &str {
        &self.state.world().objects()[oid]
    }

    /// The number of depths carrying sleep/pending data: `prefix().len() + 1` (the
    /// starting state is always present). Valid `depth` arguments to
    /// [`sleep`](StepCx::sleep) and [`pending`](StepCx::pending) are `0..self.depth()`.
    pub fn depth(&self) -> usize {
        self.frames.len()
    }

    /// The sleep set at `depth` (the pids asleep at that prefix).
    pub fn sleep(&self, depth: usize) -> &[ProcessID] {
        self.frames[depth].sleep()
    }

    /// The pending ops at `depth` (each enabled process's next op there).
    pub fn pending(&self, depth: usize) -> &[Transition] {
        self.frames[depth].pending()
    }

    /// The frontier wakeup tree's root, for walking the reversing fragments.
    pub fn wakeup(&self) -> WakeupNode<'a> {
        WakeupNode { node: self.tree }
    }
}

/// A read-only view of one wakeup-tree node. Children are in ≺ (sibling) order;
/// `children()[0]` is the ≺-minimal branch, explored first — walk it to inspect the
/// reversing fragments the search still plans to run from the current frontier.
pub struct WakeupNode<'a> {
    node: &'a Wut,
}

impl<'a> WakeupNode<'a> {
    /// Each child as its edge transition (matched by pid; the per-object `seq`
    /// drifts, so read only the pid) plus the subtree below it.
    pub fn children(&self) -> Vec<(Transition, WakeupNode<'a>)> {
        self.node
            .children()
            .iter()
            .map(|(edge, sub)| (*edge, WakeupNode { node: sub }))
            .collect()
    }
}

/// How the reordering of a reversible race ([`Step::Race`]'s `v`) resolved against the
/// wakeup tree. `insert_depth`, where present, is the depth the fragment targets — the
/// point just before the earlier racing event.
#[derive(Debug)]
pub enum RaceOutcome {
    /// Rejected: running `ep`'s process first would leave it unable to perform `ep`
    /// (for example a `recv` whose only matching `send` is `e`), so the reordered
    /// schedule is unrealizable and is skipped.
    Disabling,
    /// Already covered: a process asleep at the target prefix is a weak-initial of the
    /// fragment, so the reversed order is reached without adding anything.
    CoveredBySleeper {
        insert_depth: usize,
        covering_pid: ProcessID,
    },
    /// Already covered by an existing branch of the wakeup tree.
    ExistingLeaf { insert_depth: usize },
    /// A fresh branch was grafted into the wakeup tree to explore the reversed order.
    Grafted { insert_depth: usize },
}

// Borrows the driver's live data, so emitting a step allocates nothing and the
// no-op `step` reads none of it.
/// A discrete decision of the Optimal DPOR driver, delivered to [`Observer::step`]
/// together with a [`StepCx`]. The stream of steps narrates the whole search: every
/// interleaving it runs to the end, every race it reverses, every branch it
/// backtracks out of.
///
/// A transition a step reports as *committed* — a [`Maximal`](Step::Maximal) trace or
/// a [`Descend`](Step::Descend)'s `committed` — can be named and labelled through the
/// [`StepCx`]. Transitions in planned or sleeping data identify their process
/// ([`Transition::pid`]) and object ([`Transition::oid`]) but not a stable operation,
/// so do not [`label`](StepCx::label) them.
pub enum Step<'a> {
    /// The search reached a state — the starting state, or one freshly advanced by a
    /// committed transition (so [`state()`](StepCx::state)'s [`trace`](State::trace)
    /// already includes that transition). Fires once for every state the search reaches —
    /// runnable, terminal, or failed; a state rebuilt by replay is not re-reported.
    /// Inspect the state for its [`trace`](State::trace),
    /// [`is_terminal`](State::is_terminal), and any
    /// [`failure_reason`](State::failure_reason).
    Visit,

    /// Exploration began: the search seeded its first interleaving with one enabled
    /// process, `seeded`.
    RootSeed { seeded: Transition },

    /// Nothing to explore: no process is enabled at the start.
    RootEmpty,

    /// A complete interleaving was run to a leaf — one representative of a
    /// Mazurkiewicz equivalence class. `trace` is its full sequence of committed
    /// transitions (each labellable via [`StepCx::label`]); `failure` is `true` when
    /// the leaf ends in a process error or a deadlock.
    Maximal {
        trace: &'a [Transition],
        failure: bool,
    },

    /// The search advanced one step, committing `committed` as the current
    /// interleaving's next transition. `depth` is the prefix length before the step.
    /// `parent_sleep` and `child_sleep` are the sleep sets just before and just after
    /// it: a process in `parent_sleep` but not `child_sleep` was woken because it
    /// conflicts with `committed`.
    Descend {
        depth: usize,
        committed: Transition,
        parent_sleep: &'a [ProcessID],
        child_sleep: &'a [ProcessID],
    },

    /// Having descended to `depth`, the search chose the next process to explore from
    /// the new state. `Some(t)` is whichever process now heads the child — a freshly
    /// seeded one, or a continuation an earlier race reversal already planted there;
    /// `None` only when nothing is runnable (a maximal or fully-blocked state).
    SeedChild {
        depth: usize,
        seeded: Option<Transition>,
    },

    /// A reversible race in the interleaving just completed: the dependent events `e`
    /// (at 1-based trace position `i`) and `ep` (at position `j`, with `i < j`) have a
    /// direct happens-before edge the search tries to flip. To explore the order where
    /// `ep` precedes `e`, the driver schedules the fragment `v` from the state just
    /// before `e`; `v` is `notdep` followed by `ep`, where `notdep` is the events after
    /// `e` that do not happen-after `e` (the part of the future causally independent of
    /// `e`). `outcome` records how `v` resolved against the wakeup tree.
    Race {
        i: usize,
        j: usize,
        e: Transition,
        ep: Transition,
        notdep: &'a [Transition],
        v: &'a [Transition],
        outcome: RaceOutcome,
    },

    /// A frontier node was fully explored, so the search backtracked one level: every
    /// continuation at depth `from_depth` is done, and the search returns to
    /// `into_depth` (`= from_depth - 1`), putting `finished_pid` to sleep there.
    Pop {
        finished_pid: ProcessID,
        from_depth: usize,
        into_depth: usize,
    },

    /// The current state was rebuilt by replaying `prefix` from the start before the
    /// search continues down a different branch.
    Replay { prefix: &'a [Transition] },

    /// Exploration is complete: one interleaving per Mazurkiewicz class has been
    /// visited.
    Done,
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use super::{Observer, RaceOutcome, Step, StepCx};
    use crate::Atomic;
    use crate::model::World;
    use crate::search::explore;

    fn spawn_store<'a>(world: &mut World<'a>, name: &str, cell: Atomic<u32>, value: u32) {
        world.spawn(name.to_string(), async move {
            cell.store(value).await;
            Ok(())
        });
    }

    fn spawn_load<'a>(world: &mut World<'a>, name: &str, cell: Atomic<u32>) {
        world.spawn(name.to_string(), async move {
            cell.load().await;
            Ok(())
        });
    }

    // The same fixture as optimal.rs: a writer racing two readers on one atomic.
    fn one_writer_two_readers(world: &mut World) {
        let x = world.atomic("x", 0u32);
        spawn_store(world, "writer", x.clone(), 1);
        spawn_load(world, "reader-1", x.clone());
        spawn_load(world, "reader-2", x);
    }

    // Records each step as one compact line, resolving pids to process names. Only
    // committed transitions are labelled; planned/sleep/pending data is name-only.
    #[derive(Default)]
    struct Recorder {
        lines: Vec<String>,
    }

    fn names(cx: &StepCx<'_, '_>, pids: &[usize]) -> String {
        let mut s = String::new();
        s.push('[');
        for (k, &p) in pids.iter().enumerate() {
            if k > 0 {
                s.push(',');
            }
            s.push_str(cx.process(p));
        }
        s.push(']');
        s
    }

    fn proc_names(cx: &StepCx<'_, '_>, ts: &[crate::model::Transition]) -> String {
        names(cx, &ts.iter().map(|t| t.pid()).collect::<Vec<_>>())
    }

    impl Observer for Recorder {
        fn step(&mut self, step: Step<'_>, cx: StepCx<'_, '_>) {
            let mut line = String::new();
            match step {
                Step::Visit => {
                    // The visited state's own depth is its trace length (the prefix
                    // describes the parent at a post-apply Visit), per `Step::Visit`'s
                    // contract: read `cx.state()`, not `cx.prefix()`.
                    write!(
                        line,
                        "Visit d{} terminal={}",
                        cx.state().trace().len(),
                        cx.state().is_terminal()
                    )
                    .unwrap();
                }
                Step::RootSeed { seeded } => {
                    write!(line, "RootSeed {}", cx.process(seeded.pid())).unwrap();
                }
                Step::RootEmpty => line.push_str("RootEmpty"),
                Step::Descend {
                    depth,
                    committed,
                    parent_sleep,
                    child_sleep,
                } => {
                    // `dropped` is no longer carried by the event: a consumer derives
                    // it as parent_sleep \ child_sleep.
                    let dropped: Vec<usize> = parent_sleep
                        .iter()
                        .copied()
                        .filter(|p| !child_sleep.contains(p))
                        .collect();
                    write!(
                        line,
                        "Descend d{depth} {} parent_sleep={} child_sleep={} dropped={}",
                        cx.process(committed.pid()),
                        names(&cx, parent_sleep),
                        names(&cx, child_sleep),
                        names(&cx, &dropped),
                    )
                    .unwrap();
                }
                Step::SeedChild { depth, seeded } => match seeded {
                    Some(q_t) => {
                        write!(line, "SeedChild d{depth} {}", cx.process(q_t.pid())).unwrap()
                    }
                    None => write!(line, "SeedChild d{depth} -").unwrap(),
                },
                Step::Maximal { trace, failure } => {
                    write!(line, "Maximal {} failure={failure}", proc_names(&cx, trace)).unwrap();
                }
                Step::Race {
                    i,
                    j,
                    e,
                    ep,
                    notdep: _,
                    v,
                    outcome,
                } => {
                    write!(
                        line,
                        "Race i{i} j{j} ({},{}) v={} -> ",
                        cx.process(e.pid()),
                        cx.process(ep.pid()),
                        proc_names(&cx, v),
                    )
                    .unwrap();
                    match outcome {
                        RaceOutcome::Disabling => line.push_str("disabling"),
                        RaceOutcome::CoveredBySleeper {
                            insert_depth,
                            covering_pid,
                        } => write!(
                            line,
                            "covered@{insert_depth} by {}",
                            cx.process(covering_pid)
                        )
                        .unwrap(),
                        RaceOutcome::ExistingLeaf { insert_depth } => {
                            write!(line, "existingleaf@{insert_depth}").unwrap()
                        }
                        RaceOutcome::Grafted { insert_depth } => {
                            write!(line, "grafted@{insert_depth}").unwrap()
                        }
                    }
                }
                Step::Pop {
                    finished_pid,
                    from_depth,
                    into_depth,
                } => {
                    write!(
                        line,
                        "Pop {} {from_depth}->{into_depth}",
                        cx.process(finished_pid)
                    )
                    .unwrap();
                }
                Step::Replay { prefix } => {
                    write!(line, "Replay {}", proc_names(&cx, prefix)).unwrap();
                }
                Step::Done => line.push_str("Done"),
            }
            self.lines.push(line);
        }
    }

    #[test]
    fn one_writer_two_readers_step_stream() {
        let mut rec = Recorder::default();
        let res = explore(&one_writer_two_readers, &mut rec);
        assert!(res.is_ok(), "fixture explores cleanly");

        let expected = [
            "Visit d0 terminal=false",
            "RootSeed writer",
            "Visit d1 terminal=false",
            "Descend d0 writer parent_sleep=[] child_sleep=[] dropped=[]",
            "SeedChild d1 reader-1",
            "Visit d2 terminal=false",
            "Descend d1 reader-1 parent_sleep=[] child_sleep=[] dropped=[]",
            "SeedChild d2 reader-2",
            "Visit d3 terminal=true",
            "Descend d2 reader-2 parent_sleep=[] child_sleep=[] dropped=[]",
            "Maximal [writer,reader-1,reader-2] failure=false",
            "Race i1 j2 (writer,reader-1) v=[reader-1] -> grafted@0",
            "Race i1 j3 (writer,reader-2) v=[reader-2] -> existingleaf@0",
            "Pop reader-2 3->2",
            "Pop reader-1 2->1",
            "Pop writer 1->0",
            "Replay []",
            "Visit d1 terminal=false",
            "Descend d0 reader-1 parent_sleep=[writer] child_sleep=[] dropped=[writer]",
            "SeedChild d1 writer",
            "Visit d2 terminal=false",
            "Descend d1 writer parent_sleep=[] child_sleep=[] dropped=[]",
            "SeedChild d2 reader-2",
            "Visit d3 terminal=true",
            "Descend d2 reader-2 parent_sleep=[] child_sleep=[] dropped=[]",
            "Maximal [reader-1,writer,reader-2] failure=false",
            "Race i1 j2 (reader-1,writer) v=[writer] -> covered@0 by writer",
            "Race i2 j3 (writer,reader-2) v=[reader-2] -> grafted@1",
            "Pop reader-2 3->2",
            "Pop writer 2->1",
            "Replay [reader-1]",
            "Visit d2 terminal=false",
            "Descend d1 reader-2 parent_sleep=[writer] child_sleep=[] dropped=[writer]",
            "SeedChild d2 writer",
            "Visit d3 terminal=true",
            "Descend d2 writer parent_sleep=[] child_sleep=[] dropped=[]",
            "Maximal [reader-1,reader-2,writer] failure=false",
            "Race i1 j3 (reader-1,writer) v=[reader-2,writer] -> grafted@0",
            "Race i2 j3 (reader-2,writer) v=[writer] -> covered@1 by writer",
            "Pop writer 3->2",
            "Pop reader-2 2->1",
            "Pop reader-1 1->0",
            "Replay []",
            "Visit d1 terminal=false",
            "Descend d0 reader-2 parent_sleep=[writer,reader-1] child_sleep=[reader-1] dropped=[writer]",
            "SeedChild d1 writer",
            "Visit d2 terminal=false",
            "Descend d1 writer parent_sleep=[reader-1] child_sleep=[] dropped=[reader-1]",
            "SeedChild d2 reader-1",
            "Visit d3 terminal=true",
            "Descend d2 reader-1 parent_sleep=[] child_sleep=[] dropped=[]",
            "Maximal [reader-2,writer,reader-1] failure=false",
            "Race i1 j2 (reader-2,writer) v=[writer] -> covered@0 by writer",
            "Race i2 j3 (writer,reader-1) v=[reader-1] -> covered@1 by reader-1",
            "Pop reader-1 3->2",
            "Pop writer 2->1",
            "Pop reader-2 1->0",
            "Done",
        ];

        assert_eq!(
            rec.lines, expected,
            "step stream must match the expert-verified ground truth"
        );
    }
}

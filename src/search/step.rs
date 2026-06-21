//! Step instrumentation for the Optimal DPOR driver: a typed callback that emits the
//! algorithm's discrete decisions (descend, seed, race-reversal, pop, …) as they
//! happen, mirroring the `Observer` pattern. The `()` impl is a no-op, so normal
//! `explore` pays nothing — every emit site passes a borrowed `Step<'_>` referencing
//! data the driver already holds, so the no-op path allocates nothing.
//!
//! Public, feature-gated (`viz`): a visualizer is built *on top of* the crate, the
//! same way an external consumer would. The renderer itself lives in the
//! `optimal_viz` bin, a pure consumer of this hook plus the public `model` surface.

// Without `viz` these items are `pub` but not re-exported, so they are unreachable
// from outside the crate and the no-op `()::on` reads none of `Step`'s fields. They
// are the public hook the `viz` consumer (and the golden test) exercises in full.
#![allow(dead_code)]

use super::optimal::{Frame, Wut};
use crate::model::{ObjectID, ProcessID, State, Transition};

// The step callback: invoked at every discrete decision of the Optimal driver. The
// consumer reaches the model only through `cx` (names/labels via the `State`), never
// into primitive internals. `Step` borrows the driver's live data, so the no-op `()`
// impl pays nothing.
pub trait StepObserver {
    fn on(&mut self, step: Step<'_>, cx: StepCx<'_, '_>);
}

// The no-op observer the default `explore` uses: an empty `on`, so every emit site
// monomorphizes away and `()` allocates nothing.
impl StepObserver for () {
    fn on(&mut self, _step: Step<'_>, _cx: StepCx<'_, '_>) {}
}

// The live state a consumer needs at any step: the frontier wakeup tree, the
// per-depth sleep + pending, the surviving prefix, and the live `State` for name and
// label resolution. All borrowed — clone in the consumer if needed.
//
// Two lifetimes: `'a` is the (short) borrow at the emit site; `'w` is the `State`'s
// own setup lifetime. They are decoupled because `State<'w>` is invariant over `'w`
// (it holds a `&'w dyn Fn`), so tying them would force the borrow to live as long as
// the whole search.
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

    /// The surviving committed prefix (the ≺-minimal spine down to the frontier).
    pub fn prefix(&self) -> &[Transition] {
        self.prefix
    }

    /// A human-readable label for a *committed* transition (e.g. `"load -> 123"`).
    pub fn label(&self, t: &Transition) -> String {
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

    /// Number of captured sleep/pending frames; `depth() == prefix().len() + 1` (the
    /// root frame is always present).
    pub fn depth(&self) -> usize {
        self.frames.len()
    }

    /// The sleep set at frame `depth` (the pids asleep at that prefix).
    pub fn sleep(&self, depth: usize) -> &[ProcessID] {
        self.frames[depth].sleep()
    }

    /// The pending ops at frame `depth` (each enabled process's next op there).
    pub fn pending(&self, depth: usize) -> &[Transition] {
        self.frames[depth].pending()
    }

    /// The frontier wakeup tree's root, for walking the reversing fragments.
    pub fn wakeup(&self) -> WakeupNode<'a> {
        WakeupNode { node: self.tree }
    }
}

/// A read-only view of one wakeup-tree node. Children are in ≺ (sibling) order;
/// `children()[0]` is the ≺-minimal branch (explored first). Wraps the private `Wut`
/// so a consumer can walk the frontier tree without touching driver internals.
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

// How a planned reversal resolved into the ancestor wakeup tree. `insert_depth` is
// the wut node depth the fragment targets (`i - 1` for race event i, 1-based).
#[derive(Debug)]
pub enum RaceOutcome {
    // proc(e') cannot run e' at the reversed prefix: not a legal reversal.
    NonDisabling,
    // A process already asleep at the prefix is a weak-initial of v.
    CoveredBySleeper {
        insert_depth: usize,
        covering_pid: ProcessID,
    },
    // An existing wut leaf already covers v.
    ExistingLeaf {
        insert_depth: usize,
    },
    // A fresh fragment was grafted into the wut node.
    Grafted {
        insert_depth: usize,
    },
}

// The discrete steps of the Optimal driver, borrowing the driver's live data so the
// no-op path allocates nothing. Committed transitions (`committed`, `seeded` after a
// real apply, a maximal `trace`) are label-able via `cx`'s live `State`;
// planned/pending/sleep data is pid+oid only (a planned edge's concrete op drifts, so
// never label it).
pub enum Step<'a> {
    // The root's wut was seeded with one enabled process.
    RootSeed {
        seeded: Transition,
    },
    // No process is enabled at the root: nothing to explore.
    RootEmpty,
    // A maximal trace was reached (a leaf); `failure` marks a failed leaf. `trace`
    // includes the just-applied op (label-able).
    Maximal {
        trace: &'a [Transition],
        failure: bool,
    },
    // The ≺-minimal child was committed in place. `depth == prefix.len()` BEFORE the
    // push; `committed` is the just-applied op (label-able via `state`). A consumer
    // derives the dropped sleepers as `parent_sleep \ child_sleep`.
    Descend {
        depth: usize,
        committed: Transition,
        parent_sleep: &'a [ProcessID],
        child_sleep: &'a [ProcessID],
    },
    // The child node was seeded: `Some(q_t)` when a fresh process was grafted,
    // `None` when a race already planted it or nothing was runnable.
    SeedChild {
        depth: usize,
        seeded: Option<Transition>,
    },
    // One reversible race analysed in `plan_reversals`. `notdep` is `v` without its
    // trailing e'; `v` is the reversing fragment `notdep · e'`.
    Race {
        i: usize,
        j: usize,
        e: Transition,
        ep: Transition,
        notdep: &'a [Transition],
        v: &'a [Transition],
        outcome: RaceOutcome,
    },
    // The exhausted top frame was popped: `finished_pid` was slept into the parent
    // frame. `from_depth == prefix.len()` before the pop; `into_depth == from - 1`.
    Pop {
        finished_pid: ProcessID,
        from_depth: usize,
        into_depth: usize,
    },
    // `cur` was rebuilt by a single root replay of `prefix`.
    Replay {
        prefix: &'a [Transition],
    },
    // The search is complete (the root frame was popped, or the root was empty).
    Done,
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use super::super::explore::explore_stepped;
    use super::{RaceOutcome, Step, StepCx, StepObserver};
    use crate::Atomic;
    use crate::model::World;

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

    impl StepObserver for Recorder {
        fn on(&mut self, step: Step<'_>, cx: StepCx<'_, '_>) {
            let mut line = String::new();
            match step {
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
                        RaceOutcome::NonDisabling => line.push_str("nondisabling"),
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
        let res = explore_stepped(&one_writer_two_readers, &mut rec);
        assert!(res.is_ok(), "fixture explores cleanly");

        let expected = [
            "RootSeed writer",
            "Descend d0 writer parent_sleep=[] child_sleep=[] dropped=[]",
            "SeedChild d1 reader-1",
            "Descend d1 reader-1 parent_sleep=[] child_sleep=[] dropped=[]",
            "SeedChild d2 reader-2",
            "Descend d2 reader-2 parent_sleep=[] child_sleep=[] dropped=[]",
            "Maximal [writer,reader-1,reader-2] failure=false",
            "Race i1 j2 (writer,reader-1) v=[reader-1] -> grafted@0",
            "Race i1 j3 (writer,reader-2) v=[reader-2] -> existingleaf@0",
            "Pop reader-2 3->2",
            "Pop reader-1 2->1",
            "Pop writer 1->0",
            "Replay []",
            "Descend d0 reader-1 parent_sleep=[writer] child_sleep=[] dropped=[writer]",
            "SeedChild d1 writer",
            "Descend d1 writer parent_sleep=[] child_sleep=[] dropped=[]",
            "SeedChild d2 reader-2",
            "Descend d2 reader-2 parent_sleep=[] child_sleep=[] dropped=[]",
            "Maximal [reader-1,writer,reader-2] failure=false",
            "Race i1 j2 (reader-1,writer) v=[writer] -> covered@0 by writer",
            "Race i2 j3 (writer,reader-2) v=[reader-2] -> grafted@1",
            "Pop reader-2 3->2",
            "Pop writer 2->1",
            "Replay [reader-1]",
            "Descend d1 reader-2 parent_sleep=[writer] child_sleep=[] dropped=[writer]",
            "SeedChild d2 writer",
            "Descend d2 writer parent_sleep=[] child_sleep=[] dropped=[]",
            "Maximal [reader-1,reader-2,writer] failure=false",
            "Race i1 j3 (reader-1,writer) v=[reader-2,writer] -> grafted@0",
            "Race i2 j3 (reader-2,writer) v=[writer] -> covered@1 by writer",
            "Pop writer 3->2",
            "Pop reader-2 2->1",
            "Pop reader-1 1->0",
            "Replay []",
            "Descend d0 reader-2 parent_sleep=[writer,reader-1] child_sleep=[reader-1] dropped=[writer]",
            "SeedChild d1 writer",
            "Descend d1 writer parent_sleep=[reader-1] child_sleep=[] dropped=[reader-1]",
            "SeedChild d2 reader-1",
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

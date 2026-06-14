use crate::{
    atomic::Handle,
    executor::{Executor, ProcessError},
    object::{Object, Transition},
    process,
};

pub struct World<'a> {
    objects: Vec<Box<dyn Object>>,
    exec: Executor<'a>,
}

impl<'a> World<'a> {
    pub(crate) fn new() -> Self {
        Self {
            objects: Vec::new(),
            exec: Executor::default(),
        }
    }

    pub fn spawn(
        &mut self,
        code: impl Future<Output = process::ProcessResult> + 'a,
    ) -> process::ProcessID {
        self.exec.schedule(code)
    }

    pub fn atomic<T: Copy + PartialEq + 'static>(&mut self, value: T) -> Handle<T> {
        let id = self.objects.len();
        let handle = Handle::new(id, value);
        self.objects.push(Box::new(handle.clone()));
        handle
    }
}

pub(crate) struct State<'a> {
    world: World<'a>,
    setup: &'a dyn Fn(&mut World<'a>),
    trace: Vec<Transition>,
}

// A lightweight, cloneable view of a `State`: the setup (the processes) plus the
// scheduling decisions taken (the trace). `State`s cannot be cloned (futures
// can't), so the search carries views around instead and rebuilds the full
// state on demand via `state` — deterministic because object/transition ids are
// assigned in fixed order.
#[derive(Clone)]
pub(crate) struct StateView<'a> {
    setup: &'a dyn Fn(&mut World<'a>),
    trace: Vec<Transition>,
}

impl<'a> StateView<'a> {
    pub(crate) fn new(setup: &'a dyn Fn(&mut World<'a>)) -> Self {
        Self {
            setup,
            trace: Vec::new(),
        }
    }

    pub(crate) fn trace(&self) -> &[Transition] {
        &self.trace
    }

    // Rebuilds the full state by replaying the recorded schedule. A view is a
    // valid prefix by construction, so only the final step can fail; an earlier
    // failure means the model is non-deterministic.
    pub(crate) fn state(&self) -> Result<State<'a>, ProcessError> {
        let Some((&last, rest)) = self.trace.split_last() else {
            // No transitions: the setup itself is the only step that may fail.
            return State::new(self.setup);
        };
        let mut state = State::new(self.setup).expect("setup must replay");
        for &t in rest {
            debug_assert!(
                state.enabled().contains(&t),
                "replay diverged at {t:?}: model is non-deterministic"
            );
            state.apply(t).expect("non-final transition must replay");
        }
        state.apply(last)?;
        Ok(state)
    }
}

impl<'a> State<'a> {
    fn new(setup: &'a dyn Fn(&mut World<'a>)) -> Result<Self, ProcessError> {
        let mut world = World::new();
        setup(&mut world);
        world.exec.execute()?;
        Ok(Self {
            world,
            setup,
            trace: Vec::new(),
        })
    }

    pub(crate) fn pending(&self) -> usize {
        self.world.exec.pending()
    }

    pub(crate) fn enabled(&self) -> Vec<Transition> {
        // Fixed iteration order (objects, then each object's requests, both
        // insertion-ordered) is what lets replay rebuild identical states.
        self.world
            .objects
            .iter()
            .flat_map(|o| o.enabled())
            .collect()
    }

    pub(crate) fn apply(&mut self, t: Transition) -> Result<(), ProcessError> {
        // Record before executing so a failing transition is part of the trace:
        // the view of a failed state must replay the failure (see FailedState).
        self.trace.push(t);
        self.world.objects[t.oid].apply(t);
        self.world.exec.execute()
    }

    // A view of this state, ready to be replayed back into a full `State`.
    pub(crate) fn view(&self) -> StateView<'a> {
        StateView {
            setup: self.setup,
            trace: self.trace.clone(),
        }
    }

    // An independent copy, rebuilt by replaying its view from scratch.
    pub(crate) fn fork(&self) -> Self {
        self.view()
            .state()
            .expect("fork replay failed: model is non-deterministic")
    }
}

#[cfg(test)]
mod tests {
    use super::{State, World};

    fn program(world: &mut World) {
        let atomic = world.atomic(123);
        let r1 = atomic.clone();
        let r2 = atomic.clone();
        world.spawn(async move {
            r1.store(222).await;
            Ok(())
        });
        world.spawn(async move {
            if r2.load().await == 222 {
                Ok(())
            } else {
                Err("unexpected value".into())
            }
        });
    }

    #[test]
    fn store_then_load() {
        let mut state = State::new(&program).unwrap();
        let enabled = state.enabled();
        let p1 = *enabled.iter().find(|t| t.pid == 0).unwrap();
        let p2 = *enabled.iter().find(|t| t.pid == 1).unwrap();
        state.apply(p1).unwrap();
        state.apply(p2).unwrap();
        assert!(state.enabled().is_empty());
    }

    #[test]
    fn load_then_store_fails() {
        let mut state = State::new(&program).unwrap();
        let enabled = state.enabled();
        let p2 = *enabled.iter().find(|t| t.pid == 1).unwrap();
        assert!(state.apply(p2).is_err());
    }
}

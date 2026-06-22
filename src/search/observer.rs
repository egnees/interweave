use super::step::{Step, StepCx};
use crate::model::State;

/// A hook the strategy calls as it explores.
///
/// [`observe`](Observer::observe) fires for every state the strategy reaches — runnable, terminal,
/// or failed — so an implementor sees failed states too (and can read their failure reason and
/// resolve the failing transition through [`State`]). [`step`](Observer::step) fires at each
/// discrete decision of the Optimal driver (descend, seed, race-reversal, pop, …); it is a no-op by
/// default, so an observer that only watches states ignores it at zero cost. Implementors learn
/// about the search only through typed views ([`State`], [`Step`], [`StepCx`]), never by reaching
/// into the synchronization primitives or the executor.
///
/// Observation is intentionally infallible: an observer is not replayed, so it cannot serve as a
/// property check; verifying invariants belongs in a separate, replay-aware facility. The no-op
/// `()` implementation observes nothing, making `&mut ()` a zero-cost default.
pub trait Observer {
    /// Called once per explored state, before the search branches out of it.
    fn observe(&mut self, state: &State);

    /// Called at each discrete decision of the Optimal driver (descend, seed,
    /// race-reversal, pop, …). Default no-op: an observer that only watches
    /// states ignores it at zero cost.
    fn step(&mut self, step: Step<'_>, cx: StepCx<'_, '_>) {
        let _ = (step, cx);
    }
}

/// The default observer: observe nothing.
impl Observer for () {
    fn observe(&mut self, _state: &State) {}
}

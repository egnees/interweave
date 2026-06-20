use crate::model::State;

/// A hook the strategy calls at every explored state.
///
/// Fires for every state the strategy reaches — runnable, terminal, or failed — so an implementor
/// sees failed states too (and can read their failure reason and resolve the failing transition
/// through [`State`]). Implementors learn about a state only through the typed [`State`], never by
/// reaching into the synchronization primitives or the executor.
///
/// Observation is intentionally infallible: an observer is not replayed, so it cannot serve as a
/// property check; verifying invariants belongs in a separate, replay-aware facility. The no-op
/// `()` implementation observes nothing, making `&mut ()` a zero-cost default.
pub trait Observer {
    /// Called once per explored state, before the search branches out of it.
    fn observe(&mut self, state: &State);
}

/// The default observer: observe nothing.
impl Observer for () {
    fn observe(&mut self, _state: &State) {}
}

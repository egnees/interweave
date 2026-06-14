use crate::process;

pub(crate) type ObjectID = usize;

#[derive(Eq, PartialEq, Clone, Copy, Debug)]
pub(crate) struct Transition {
    pub(crate) pid: process::ProcessID,
    pub(crate) oid: ObjectID,
    // Index of this op in its object's registration order. Reproducible across
    // replays, which is what makes a transition's identity stable.
    pub(crate) seq: usize,
}

pub(crate) trait Object {
    fn apply(&mut self, t: Transition);
    fn enabled(&self) -> Vec<Transition>;
}

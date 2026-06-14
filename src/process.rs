use std::error::Error;

pub type ProcessID = usize;
pub type ProcessResult = Result<(), Box<dyn Error>>;

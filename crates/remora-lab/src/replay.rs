//! Placeholder for replaying recorded observations through sans-IO state machines.

use crate::error::LabError;

pub fn run_placeholder() -> Result<(), LabError> {
    Err(LabError::Unimplemented("replay"))
}

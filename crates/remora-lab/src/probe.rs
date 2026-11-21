//! Placeholder for probing remote endpoints/targets.

use crate::error::LabError;

pub fn run_placeholder() -> Result<(), LabError> {
    Err(LabError::Unimplemented("probe"))
}

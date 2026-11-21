//! Placeholder for live trace recording against official or Remora targets.

use crate::error::LabError;

pub fn run_placeholder() -> Result<(), LabError> {
    Err(LabError::Unimplemented("record"))
}

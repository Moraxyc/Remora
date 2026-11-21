//! Input source and sink traits operating on [`InputMessage`].

use remora_proto::session::input::InputMessage;

use super::PlatformError;

/// Produces local input events (real devices or scripted test sequences).
pub trait InputSource: Send {
    /// Returns the next input event, or `None` when no more events remain.
    fn next_event(&mut self) -> Result<Option<InputMessage>, PlatformError>;
}

/// Injects remote input events into the host platform (or records them in tests).
pub trait InputSink: Send {
    fn inject(&mut self, event: &InputMessage) -> Result<(), PlatformError>;
}

//! Sans-IO client-side state machines driven by external I/O adapters.

pub mod auth;
pub mod peer;
pub mod signaling;

/// A pure, sans-IO state machine.
///
/// External drivers feed inputs (decoded network frames, HTTP responses, elapsed time, or
/// local commands) into [`Machine::handle`] and drain outgoing effects via
/// [`Machine::poll_output`] until it returns `None`.
pub trait Machine {
    type Input;
    type Output;
    type Error;

    /// Feeds one input event into the state machine.
    fn handle(&mut self, input: Self::Input) -> Result<(), Self::Error>;

    /// Returns the next pending output action, if any.
    fn poll_output(&mut self) -> Option<Self::Output>;
}

//! Sans-IO client-side state machines driven by external I/O adapters.

use std::ops::{Add, Sub};
use std::time::Duration;

pub mod auth;
pub mod peer;
pub mod signaling;

/// Monotonic timestamp passed into state machines by their I/O driver.
///
/// `remora-proto` never reads the system clock directly; drivers anchor `Timestamp::ZERO`
/// at startup (or pass elapsed monotonic duration) so state machines remain deterministic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Timestamp(Duration);

impl Timestamp {
    pub const ZERO: Self = Self(Duration::ZERO);

    pub const fn from_duration(elapsed: Duration) -> Self {
        Self(elapsed)
    }

    pub const fn from_millis(millis: u64) -> Self {
        Self(Duration::from_millis(millis))
    }

    pub const fn from_secs(secs: u64) -> Self {
        Self(Duration::from_secs(secs))
    }

    pub const fn as_duration(self) -> Duration {
        self.0
    }

    pub fn as_millis(self) -> u64 {
        u64::try_from(self.0.as_millis()).unwrap_or(u64::MAX)
    }

    pub const fn as_secs(self) -> u64 {
        self.0.as_secs()
    }

    pub fn saturating_duration_since(self, earlier: Self) -> Duration {
        self.0.saturating_sub(earlier.0)
    }
}

impl Add<Duration> for Timestamp {
    type Output = Self;

    fn add(self, rhs: Duration) -> Self::Output {
        Self(self.0.saturating_add(rhs))
    }
}

impl Sub<Timestamp> for Timestamp {
    type Output = Duration;

    fn sub(self, rhs: Timestamp) -> Self::Output {
        self.0.saturating_sub(rhs.0)
    }
}

/// A pure, sans-IO state machine.
///
/// Drivers feed inbound wire bytes via [`Machine::handle_input`], advance timers via
/// [`Machine::handle_timeout`] when [`Machine::poll_timeout`] is reached, and drain
/// [`Machine::poll_transmit`] and [`Machine::poll_event`] until both return `None`.
pub trait Machine {
    type Transmit;
    type Event;
    type Error;

    /// Feeds one inbound byte slice (HTTP response, WebSocket frame, or data-channel frame)
    /// into the state machine at monotonic time `now`.
    fn handle_input(&mut self, now: Timestamp, input: &[u8]) -> Result<(), Self::Error>;

    /// Advances time-driven transitions when `now >= poll_timeout()`.
    fn handle_timeout(&mut self, now: Timestamp);

    /// Returns the next outbound transmission to send over I/O, if any.
    fn poll_transmit(&mut self) -> Option<Self::Transmit>;

    /// Returns the next domain event for the application, if any.
    fn poll_event(&mut self) -> Option<Self::Event>;

    /// Returns the next deadline at which [`Machine::handle_timeout`] should be called.
    fn poll_timeout(&self) -> Option<Timestamp>;
}

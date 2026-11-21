//! Clipboard abstraction trait.

use super::PlatformError;

/// Reads and writes plain-text clipboard contents.
pub trait Clipboard: Send {
    fn get_text(&mut self) -> Result<Option<String>, PlatformError>;
    fn set_text(&mut self, text: &str) -> Result<(), PlatformError>;
}

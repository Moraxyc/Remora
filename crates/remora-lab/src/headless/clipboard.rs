//! In-memory headless clipboard.

use remora_endpoint::platform::{Clipboard, PlatformError};

#[derive(Debug, Clone, Default)]
pub struct MemoryClipboard {
    text: Option<String>,
}

impl Clipboard for MemoryClipboard {
    fn get_text(&mut self) -> Result<Option<String>, PlatformError> {
        Ok(self.text.clone())
    }

    fn set_text(&mut self, text: &str) -> Result<(), PlatformError> {
        self.text = Some(text.to_owned());
        Ok(())
    }
}

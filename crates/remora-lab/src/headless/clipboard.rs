//! In-memory headless clipboard.

use std::sync::{Arc, Mutex};

use remora_endpoint::platform::{Clipboard, PlatformError};

#[derive(Debug, Clone, Default)]
pub struct MemoryClipboard {
    text: Arc<Mutex<Option<String>>>,
}

impl MemoryClipboard {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn text(&self) -> Option<String> {
        self.text.lock().expect("clipboard lock poisoned").clone()
    }
}

impl Clipboard for MemoryClipboard {
    fn get_text(&mut self) -> Result<Option<String>, PlatformError> {
        Ok(self.text())
    }

    fn set_text(&mut self, text: &str) -> Result<(), PlatformError> {
        *self.text.lock().expect("clipboard lock poisoned") = Some(text.to_owned());
        Ok(())
    }
}

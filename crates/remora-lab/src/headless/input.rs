//! Scripted input source and recording input sink.

use std::sync::{Arc, Mutex};

use remora_endpoint::platform::{InputSink, InputSource, PlatformError};
use remora_proto::session::input::InputMessage;

/// Emits a pre-configured sequence of [`InputMessage`]s.
#[derive(Debug, Clone, Default)]
pub struct ScriptedInputSource {
    events: Vec<InputMessage>,
    cursor: usize,
}

impl ScriptedInputSource {
    pub fn new(events: Vec<InputMessage>) -> Self {
        Self { events, cursor: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.events.len().saturating_sub(self.cursor)
    }
}

impl InputSource for ScriptedInputSource {
    fn next_event(&mut self) -> Result<Option<InputMessage>, PlatformError> {
        if self.cursor >= self.events.len() {
            return Ok(None);
        }
        let msg = self.events[self.cursor].clone();
        self.cursor += 1;
        Ok(Some(msg))
    }
}

/// Records every [`InputMessage`] injected into the host platform for later verification.
#[derive(Debug, Clone, Default)]
pub struct RecordingInputSink {
    recorded: Arc<Mutex<Vec<InputMessage>>>,
}

impl RecordingInputSink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn recorded(&self) -> Vec<InputMessage> {
        self.recorded
            .lock()
            .expect("input sink lock poisoned")
            .clone()
    }

    pub fn len(&self) -> usize {
        self.recorded
            .lock()
            .expect("input sink lock poisoned")
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn clear(&self) {
        self.recorded
            .lock()
            .expect("input sink lock poisoned")
            .clear();
    }
}

impl InputSink for RecordingInputSink {
    fn inject(&mut self, event: &InputMessage) -> Result<(), PlatformError> {
        self.recorded
            .lock()
            .expect("input sink lock poisoned")
            .push(event.clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripted_source_feeds_recording_sink() {
        let script = vec![
            InputMessage::Keyboard {
                key: 65,
                pressed: true,
            },
            InputMessage::MouseAbsolute { x: 0.5, y: 0.25 },
            InputMessage::TextInput {
                text: "remora".into(),
            },
        ];
        let mut source = ScriptedInputSource::new(script.clone());
        let mut sink = RecordingInputSink::new();

        while let Some(ev) = source.next_event().unwrap() {
            sink.inject(&ev).unwrap();
        }
        assert_eq!(sink.recorded(), script);
    }
}

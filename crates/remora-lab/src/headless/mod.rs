//! Headless [`Platform`] implementation for deterministic lab testing.

pub mod audio;
pub mod clipboard;
pub mod input;
pub mod tools;
pub mod video;

use remora_endpoint::platform::{
    AudioSink, AudioSource, Clipboard, InputSink, InputSource, Platform, VideoSink, VideoSource,
};

pub use audio::{CountingAudioSink, SilenceAudioSource};
pub use clipboard::MemoryClipboard;
pub use input::{RecordingInputSink, ScriptedInputSource};
pub use tools::spawn_headless_tool_responder;
pub use video::{AnnexBFileVideoSource, CountingVideoSink, synthetic_annex_b_sample};

/// Complete headless endpoint platform combining scripted/file sources and counting/recording sinks.
pub struct HeadlessPlatform {
    pub video_source: Option<AnnexBFileVideoSource>,
    pub video_sink: CountingVideoSink,
    pub input_source: ScriptedInputSource,
    pub input_sink: RecordingInputSink,
    pub audio_source: SilenceAudioSource,
    pub audio_sink: CountingAudioSink,
    pub clipboard: MemoryClipboard,
}

impl Default for HeadlessPlatform {
    fn default() -> Self {
        Self::new()
    }
}

impl HeadlessPlatform {
    pub fn new() -> Self {
        Self {
            video_source: None,
            video_sink: CountingVideoSink::new(),
            input_source: ScriptedInputSource::default(),
            input_sink: RecordingInputSink::new(),
            audio_source: SilenceAudioSource::default(),
            audio_sink: CountingAudioSink::default(),
            clipboard: MemoryClipboard::default(),
        }
    }

    pub fn with_video_source(mut self, source: AnnexBFileVideoSource) -> Self {
        self.video_source = Some(source);
        self
    }

    pub fn with_input_source(mut self, source: ScriptedInputSource) -> Self {
        self.input_source = source;
        self
    }
}

impl Platform for HeadlessPlatform {
    fn video_source(&mut self) -> Option<&mut dyn VideoSource> {
        self.video_source
            .as_mut()
            .map(|src| src as &mut dyn VideoSource)
    }

    fn video_sink(&mut self) -> Option<&mut dyn VideoSink> {
        Some(&mut self.video_sink)
    }

    fn input_source(&mut self) -> Option<&mut dyn InputSource> {
        Some(&mut self.input_source)
    }

    fn input_sink(&mut self) -> Option<&mut dyn InputSink> {
        Some(&mut self.input_sink)
    }

    fn audio_source(&mut self) -> Option<&mut dyn AudioSource> {
        Some(&mut self.audio_source)
    }

    fn audio_sink(&mut self) -> Option<&mut dyn AudioSink> {
        Some(&mut self.audio_sink)
    }

    fn clipboard(&mut self) -> Option<&mut dyn Clipboard> {
        Some(&mut self.clipboard)
    }
}

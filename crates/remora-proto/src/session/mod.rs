//! Session plane: WebRTC data-channel payloads, control envelopes, input, and media codecs.

pub mod clipboard;
pub mod control;
pub mod file;
pub mod input;
pub mod media;
pub mod system;
pub mod terminal;
pub mod tunnel;

/// Official WebRTC SCTP data channel labels.
pub mod channels {
    pub const CONTROL: &str = "CONTROL_DATA_CHANNEL";
    pub const TEXT: &str = "TEXT_DATA_CHANNEL";
    pub const FILE: &str = "FILE_DATA_CHANNEL";
    pub const BINARY: &str = "BINARY_DATA_CHANNEL";
    pub const STREAMER: &str = "STREAMER_DATA_CHANNEL";
}

/// WebRTC data channel identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DataChannel {
    Control,
    Text,
    File,
    Binary,
    Streamer,
    Unknown(String),
}

impl DataChannel {
    pub fn from_label(label: &str) -> Self {
        match label {
            channels::CONTROL => Self::Control,
            channels::TEXT => Self::Text,
            channels::FILE => Self::File,
            channels::BINARY => Self::Binary,
            channels::STREAMER => Self::Streamer,
            other => Self::Unknown(other.to_owned()),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Control => channels::CONTROL,
            Self::Text => channels::TEXT,
            Self::File => channels::FILE,
            Self::Binary => channels::BINARY,
            Self::Streamer => channels::STREAMER,
            Self::Unknown(label) => label.as_str(),
        }
    }
}

use std::fmt;
use std::io;

use remora_endpoint::EndpointError;
use remora_endpoint::client::http::HttpError;
use remora_endpoint::client::socket::SocketError;
use remora_endpoint::platform::PlatformError;

#[derive(Debug)]
pub enum LabError {
    Io(io::Error),
    Yaml(serde_yaml_ng::Error),
    Json(serde_json::Error),
    Platform(PlatformError),
    Endpoint(EndpointError),
    Http(HttpError),
    Socket(SocketError),
    Scenario(String),
    Target(String),
    Diff(String),
    Unimplemented(&'static str),
}

impl fmt::Display for LabError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "I/O error: {err}"),
            Self::Yaml(err) => write!(f, "YAML error: {err}"),
            Self::Json(err) => write!(f, "JSON error: {err}"),
            Self::Platform(err) => write!(f, "platform error: {err}"),
            Self::Endpoint(err) => write!(f, "endpoint error: {err}"),
            Self::Http(err) => write!(f, "HTTP error: {err}"),
            Self::Socket(err) => write!(f, "socket error: {err}"),
            Self::Scenario(msg) => write!(f, "scenario failure: {msg}"),
            Self::Target(msg) => write!(f, "target error: {msg}"),
            Self::Diff(msg) => write!(f, "diff mismatch: {msg}"),
            Self::Unimplemented(cmd) => write!(f, "subcommand `{cmd}` is a placeholder"),
        }
    }
}

impl std::error::Error for LabError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::Yaml(err) => Some(err),
            Self::Json(err) => Some(err),
            Self::Platform(err) => Some(err),
            Self::Endpoint(err) => Some(err),
            Self::Http(err) => Some(err),
            Self::Socket(err) => Some(err),
            Self::Scenario(_) | Self::Target(_) | Self::Diff(_) | Self::Unimplemented(_) => None,
        }
    }
}

impl From<io::Error> for LabError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<serde_yaml_ng::Error> for LabError {
    fn from(err: serde_yaml_ng::Error) -> Self {
        Self::Yaml(err)
    }
}

impl From<serde_json::Error> for LabError {
    fn from(err: serde_json::Error) -> Self {
        Self::Json(err)
    }
}

impl From<PlatformError> for LabError {
    fn from(err: PlatformError) -> Self {
        Self::Platform(err)
    }
}

impl From<EndpointError> for LabError {
    fn from(err: EndpointError) -> Self {
        Self::Endpoint(err)
    }
}

impl From<HttpError> for LabError {
    fn from(err: HttpError) -> Self {
        Self::Http(err)
    }
}

impl From<SocketError> for LabError {
    fn from(err: SocketError) -> Self {
        Self::Socket(err)
    }
}

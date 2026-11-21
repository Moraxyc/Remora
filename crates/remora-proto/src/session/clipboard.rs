//! Clipboard session codecs (`ClipboardChange` and `ClipboardTextChangeRequest` RPCs).

use std::fmt;

use crate::pb::{self, rpc_request, rpc_response};
use crate::session::control::{ControlMessage, encode_message};

/// Maximum assembled plain-text clipboard payload size (1 MiB).
pub const MAX_CLIPBOARD_BYTES: usize = 1024 * 1024;
/// Maximum byte length of one UTF-8 chunk in a `ClipboardChange` message.
pub const CLIPBOARD_CHUNK_BYTES: usize = 8192;
/// Plain-text format identifier carried in `ClipboardTextChangeRequest`.
pub const TEXT_FORMAT_ID: u32 = 1;

/// Clipboard error returned by [`ClipboardAssembler`] or invalid RPC payloads.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ClipboardError {
    ExceedsLimit,
    UnsupportedFormat(u32),
}

impl fmt::Display for ClipboardError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExceedsLimit => f.write_str("clipboard payload exceeds maximum size"),
            Self::UnsupportedFormat(id) => write!(f, "unsupported clipboard format id: {id}"),
        }
    }
}

impl std::error::Error for ClipboardError {}

/// Decoded clipboard-related message extracted from a [`ControlMessage`].
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ClipboardMessage {
    Change {
        seq: i64,
        timestamp: i64,
        content_id: u32,
        content: String,
        to_be_continued: bool,
    },
    TextRpcRequest {
        seq: i64,
        timestamp: i64,
        request_id: i64,
        format_id: u32,
        text: String,
    },
    TextRpcResponse {
        seq: i64,
        timestamp: i64,
        request_id: i64,
        err: i32,
    },
}

impl fmt::Debug for ClipboardMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Change {
                seq,
                timestamp,
                content_id,
                content,
                to_be_continued,
            } => f
                .debug_struct("Change")
                .field("seq", seq)
                .field("timestamp", timestamp)
                .field("content_id", content_id)
                .field("len", &content.len())
                .field("to_be_continued", to_be_continued)
                .finish(),
            Self::TextRpcRequest {
                seq,
                timestamp,
                request_id,
                format_id,
                text,
            } => f
                .debug_struct("TextRpcRequest")
                .field("seq", seq)
                .field("timestamp", timestamp)
                .field("request_id", request_id)
                .field("format_id", format_id)
                .field("len", &text.len())
                .finish(),
            Self::TextRpcResponse {
                seq,
                timestamp,
                request_id,
                err,
            } => f
                .debug_struct("TextRpcResponse")
                .field("seq", seq)
                .field("timestamp", timestamp)
                .field("request_id", request_id)
                .field("err", err)
                .finish(),
        }
    }
}

/// Sans-IO reassembler for chunked `ClipboardChange` payloads.
#[derive(Default)]
pub struct ClipboardAssembler {
    pending: Option<(u32, String)>,
}

impl fmt::Debug for ClipboardAssembler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClipboardAssembler")
            .field("pending_id", &self.pending.as_ref().map(|(id, _)| *id))
            .field(
                "pending_len",
                &self.pending.as_ref().map_or(0, |(_, s)| s.len()),
            )
            .finish()
    }
}

impl ClipboardAssembler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset(&mut self) {
        self.pending = None;
    }

    /// Feeds one `ClipboardChange` chunk. Returns `Ok(Some(text))` on the final chunk.
    pub fn accept(
        &mut self,
        content_id: u32,
        chunk: &str,
        to_be_continued: bool,
    ) -> Result<Option<String>, ClipboardError> {
        let mut buf = match self.pending.take() {
            Some((prev_id, prev_buf)) if prev_id == content_id => prev_buf,
            _ => String::new(),
        };
        if buf.len().saturating_add(chunk.len()) > MAX_CLIPBOARD_BYTES {
            return Err(ClipboardError::ExceedsLimit);
        }
        buf.push_str(chunk);
        if to_be_continued {
            self.pending = Some((content_id, buf));
            Ok(None)
        } else {
            Ok(Some(buf))
        }
    }
}

/// Splits UTF-8 text into chunks of at most `max_bytes` aligned to char boundaries.
pub fn chunk_text(text: &str, max_bytes: usize) -> Vec<&str> {
    let limit = max_bytes.max(4);
    if text.is_empty() {
        return vec![""];
    }
    let mut chunks = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let mut end = rest.len().min(limit);
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        chunks.push(&rest[..end]);
        rest = &rest[end..];
    }
    chunks
}

/// Encodes one `Message.clipboard_change` frame.
pub fn encode_clipboard_change(
    seq: i64,
    timestamp: i64,
    content_id: u32,
    content: &str,
    to_be_continued: bool,
) -> Vec<u8> {
    encode_message(&ControlMessage::ClipboardChange {
        seq,
        timestamp,
        change: pb::ClipboardChange {
            content_id,
            content: content.to_owned(),
            to_be_continued,
        },
    })
}

/// Encodes a `Message.rpc_request` carrying `ClipboardTextChangeRequest`.
pub fn encode_text_rpc_request(seq: i64, timestamp: i64, request_id: i64, text: &str) -> Vec<u8> {
    encode_message(&ControlMessage::RpcRequest {
        seq,
        timestamp,
        request: pb::RpcRequest {
            request_header: Some(pb::RequestHeader { request_id }),
            which: Some(rpc_request::Which::TextChangeRequest(
                pb::ClipboardTextChangeRequest {
                    format_id: TEXT_FORMAT_ID,
                    data: text.to_owned(),
                },
            )),
        },
    })
}

/// Encodes a `Message.rpc_response` carrying `ClipboardTextChangeResponse`.
pub fn encode_text_rpc_response(seq: i64, timestamp: i64, request_id: i64, err: i32) -> Vec<u8> {
    encode_message(&ControlMessage::RpcResponse {
        seq,
        timestamp,
        response: pb::RpcResponse {
            response_header: Some(pb::ResponseHeader { request_id }),
            which: Some(rpc_response::Which::TextChangeResponse(
                pb::ClipboardTextChangeResponse { err },
            )),
        },
    })
}

/// Extracts a [`ClipboardMessage`] if `msg` is a clipboard change or text-change RPC.
pub fn extract_clipboard_message(msg: &ControlMessage) -> Option<ClipboardMessage> {
    match msg {
        ControlMessage::ClipboardChange {
            seq,
            timestamp,
            change,
        } => Some(ClipboardMessage::Change {
            seq: *seq,
            timestamp: *timestamp,
            content_id: change.content_id,
            content: change.content.clone(),
            to_be_continued: change.to_be_continued,
        }),
        ControlMessage::RpcRequest {
            seq,
            timestamp,
            request,
        } => match &request.which {
            Some(rpc_request::Which::TextChangeRequest(req)) => {
                let request_id = request.request_header.as_ref().map_or(0, |h| h.request_id);
                Some(ClipboardMessage::TextRpcRequest {
                    seq: *seq,
                    timestamp: *timestamp,
                    request_id,
                    format_id: req.format_id,
                    text: req.data.clone(),
                })
            }
            _ => None,
        },
        ControlMessage::RpcResponse {
            seq,
            timestamp,
            response,
        } => match &response.which {
            Some(rpc_response::Which::TextChangeResponse(resp)) => {
                let request_id = response
                    .response_header
                    .as_ref()
                    .map_or(0, |h| h.request_id);
                Some(ClipboardMessage::TextRpcResponse {
                    seq: *seq,
                    timestamp: *timestamp,
                    request_id,
                    err: resp.err,
                })
            }
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::control::decode_message;

    #[test]
    fn roundtrips_chunked_clipboard_change_and_redacts_debug() {
        let text = "hello-中文-clipboard-🙂-".repeat(500);
        let chunks = chunk_text(&text, CLIPBOARD_CHUNK_BYTES);
        assert!(chunks.len() > 1);

        let mut asm = ClipboardAssembler::new();
        let mut assembled = None;
        for (idx, chunk) in chunks.iter().enumerate() {
            let more = idx + 1 < chunks.len();
            let wire = encode_clipboard_change(idx as i64 + 1, 100, 7, chunk, more);
            let ctrl = decode_message(&wire).unwrap();
            assert!(!format!("{ctrl:?}").contains("clipboard"));
            let Some(ClipboardMessage::Change {
                content_id,
                content,
                to_be_continued,
                ..
            }) = extract_clipboard_message(&ctrl)
            else {
                panic!("expected clipboard change");
            };
            assembled = asm.accept(content_id, &content, to_be_continued).unwrap();
        }
        assert_eq!(assembled.as_deref(), Some(text.as_str()));
    }

    #[test]
    fn roundtrips_text_rpc_request_and_response() {
        let req_wire = encode_text_rpc_request(1, 200, 99, "secret-clip");
        let req_ctrl = decode_message(&req_wire).unwrap();
        let Some(msg) = extract_clipboard_message(&req_ctrl) else {
            panic!("expected text rpc request");
        };
        assert!(!format!("{msg:?}").contains("secret-clip"));
        assert_eq!(
            msg,
            ClipboardMessage::TextRpcRequest {
                seq: 1,
                timestamp: 200,
                request_id: 99,
                format_id: TEXT_FORMAT_ID,
                text: "secret-clip".into(),
            }
        );

        let resp_wire =
            encode_text_rpc_response(2, 201, 99, pb::ClipboardError::CbSucceeded as i32);
        let resp_ctrl = decode_message(&resp_wire).unwrap();
        assert_eq!(
            extract_clipboard_message(&resp_ctrl),
            Some(ClipboardMessage::TextRpcResponse {
                seq: 2,
                timestamp: 201,
                request_id: 99,
                err: 1,
            })
        );
    }
}

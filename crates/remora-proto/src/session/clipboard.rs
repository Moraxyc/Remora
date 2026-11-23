//! Clipboard session codecs for plain-text changes and format negotiation RPCs.

use std::fmt;

use sha2::{Digest, Sha256};

use crate::pb::{self, clipboard_request, clipboard_response, rpc_request, rpc_response};
use crate::session::control::{ControlMessage, encode_message};

/// Maximum assembled plain-text clipboard payload size (1 MiB).
pub const MAX_CLIPBOARD_BYTES: usize = 1024 * 1024;
/// Maximum byte length of one UTF-8 chunk in a `ClipboardChange` message.
pub const CLIPBOARD_CHUNK_BYTES: usize = 8192;
/// Plain-text format identifier carried in `ClipboardTextChangeRequest`.
pub const TEXT_FORMAT_ID: u32 = 1;
/// Windows predefined Unicode text clipboard format (`CF_UNICODETEXT`).
pub const CF_UNICODETEXT_ID: u32 = 13;

pub fn windows_unicode_text_format() -> pb::ClipboardFormat {
    pb::ClipboardFormat {
        id: CF_UNICODETEXT_ID,
        name: String::new(),
    }
}

/// Whether the signaling platform is Android (2) or iOS (3).
pub const fn is_mobile_platform(platform: u8) -> bool {
    matches!(platform, 2 | 3)
}

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
    FormatListRequest {
        seq: i64,
        timestamp: i64,
        request_id: i64,
        formats: Vec<pb::ClipboardFormat>,
    },
    FormatListResponse {
        seq: i64,
        timestamp: i64,
        request_id: i64,
    },
    FormatDataAsk {
        seq: i64,
        timestamp: i64,
        request_id: i64,
        format_id: u32,
        block_key: String,
        format_name: String,
    },
    FormatDataConfirm {
        seq: i64,
        timestamp: i64,
        request_id: i64,
        err: i32,
        block_key: String,
        block_count: i32,
    },
    DataBlock {
        seq: i64,
        timestamp: i64,
        request_id: i64,
        block_key: String,
        block_id: i32,
        data: Vec<u8>,
    },
    DataBlockConfirm {
        seq: i64,
        timestamp: i64,
        request_id: i64,
        block_key: String,
        block_id: i32,
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
            Self::FormatListRequest {
                seq,
                timestamp,
                request_id,
                formats,
            } => f
                .debug_struct("FormatListRequest")
                .field("seq", seq)
                .field("timestamp", timestamp)
                .field("request_id", request_id)
                .field("format_count", &formats.len())
                .finish(),
            Self::FormatListResponse {
                seq,
                timestamp,
                request_id,
            } => f
                .debug_struct("FormatListResponse")
                .field("seq", seq)
                .field("timestamp", timestamp)
                .field("request_id", request_id)
                .finish(),
            Self::FormatDataAsk {
                seq,
                timestamp,
                request_id,
                format_id,
                ..
            } => f
                .debug_struct("FormatDataAsk")
                .field("seq", seq)
                .field("timestamp", timestamp)
                .field("request_id", request_id)
                .field("format_id", format_id)
                .finish_non_exhaustive(),
            Self::FormatDataConfirm {
                seq,
                timestamp,
                request_id,
                err,
                block_count,
                ..
            } => f
                .debug_struct("FormatDataConfirm")
                .field("seq", seq)
                .field("timestamp", timestamp)
                .field("request_id", request_id)
                .field("err", err)
                .field("block_count", block_count)
                .finish_non_exhaustive(),
            Self::DataBlock {
                seq,
                timestamp,
                request_id,
                block_id,
                data,
                ..
            } => f
                .debug_struct("DataBlock")
                .field("seq", seq)
                .field("timestamp", timestamp)
                .field("request_id", request_id)
                .field("block_id", block_id)
                .field("len", &data.len())
                .finish_non_exhaustive(),
            Self::DataBlockConfirm {
                seq,
                timestamp,
                request_id,
                block_id,
                err,
                ..
            } => f
                .debug_struct("DataBlockConfirm")
                .field("seq", seq)
                .field("timestamp", timestamp)
                .field("request_id", request_id)
                .field("block_id", block_id)
                .field("err", err)
                .finish_non_exhaustive(),
        }
    }
}

/// Deduplicates consecutive text applications without retaining the text.
#[derive(Default)]
pub struct ClipboardDeduper {
    last_digest: Option<[u8; 32]>,
}

impl fmt::Debug for ClipboardDeduper {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClipboardDeduper")
            .field("has_last_text", &self.last_digest.is_some())
            .finish()
    }
}

impl ClipboardDeduper {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns true and remembers the digest if `text` is nonempty and differs from the last accepted text.
    pub fn should_apply(&mut self, text: &str) -> bool {
        if text.is_empty() {
            return false;
        }
        let digest = Sha256::digest(text.as_bytes()).into();
        if self.last_digest == Some(digest) {
            return false;
        }
        self.last_digest = Some(digest);
        true
    }

    pub fn reset(&mut self) {
        self.last_digest = None;
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
    encode_rpc_request(
        seq,
        timestamp,
        request_id,
        rpc_request::Which::TextChangeRequest(pb::ClipboardTextChangeRequest {
            format_id: TEXT_FORMAT_ID,
            data: text.to_owned(),
        }),
    )
}

/// Encodes a `Message.rpc_response` carrying `ClipboardTextChangeResponse`.
pub fn encode_text_rpc_response(seq: i64, timestamp: i64, request_id: i64, err: i32) -> Vec<u8> {
    encode_rpc_response(
        seq,
        timestamp,
        request_id,
        rpc_response::Which::TextChangeResponse(pb::ClipboardTextChangeResponse { err }),
    )
}

pub fn encode_format_list_request(
    seq: i64,
    timestamp: i64,
    request_id: i64,
    formats: Vec<pb::ClipboardFormat>,
) -> Vec<u8> {
    encode_clipboard_request(
        seq,
        timestamp,
        request_id,
        clipboard_request::Which::FormatList(pb::ClipboardFormatListRequest {
            formats,
            ..Default::default()
        }),
    )
}

pub fn encode_format_list_response(seq: i64, timestamp: i64, request_id: i64) -> Vec<u8> {
    encode_clipboard_response(
        seq,
        timestamp,
        request_id,
        clipboard_response::Which::FormatListResponse(pb::ClipboardFormatListResponse {}),
    )
}

pub fn encode_format_data_ask(
    seq: i64,
    timestamp: i64,
    request_id: i64,
    format_id: u32,
    block_key: &str,
    format_name: &str,
) -> Vec<u8> {
    encode_clipboard_request(
        seq,
        timestamp,
        request_id,
        clipboard_request::Which::FormatDataAsk(pb::ClipboardFormatDataAsk {
            format_id,
            block_key: block_key.to_owned(),
            format_name: format_name.to_owned(),
        }),
    )
}

pub fn encode_format_data_confirm(
    seq: i64,
    timestamp: i64,
    request_id: i64,
    err: i32,
    block_key: &str,
    block_count: i32,
) -> Vec<u8> {
    encode_clipboard_response(
        seq,
        timestamp,
        request_id,
        clipboard_response::Which::FormatDataConfirm(pb::ClipboardFormatDataConfirm {
            err,
            block_key: block_key.to_owned(),
            block_count,
        }),
    )
}

pub fn encode_data_block(
    seq: i64,
    timestamp: i64,
    request_id: i64,
    block_key: &str,
    block_id: i32,
    data: &[u8],
) -> Vec<u8> {
    encode_clipboard_request(
        seq,
        timestamp,
        request_id,
        clipboard_request::Which::DataBlock(pb::ClipboardDataBlock {
            block_key: block_key.to_owned(),
            block_id,
            data: data.to_vec(),
        }),
    )
}

pub fn encode_data_block_confirm(
    seq: i64,
    timestamp: i64,
    request_id: i64,
    block_key: &str,
    block_id: i32,
    err: i32,
) -> Vec<u8> {
    encode_clipboard_response(
        seq,
        timestamp,
        request_id,
        clipboard_response::Which::DataBlockConfirm(pb::ClipboardDataBlockConfirm {
            block_key: block_key.to_owned(),
            block_id,
            err,
        }),
    )
}

fn encode_clipboard_request(
    seq: i64,
    timestamp: i64,
    request_id: i64,
    which: clipboard_request::Which,
) -> Vec<u8> {
    encode_rpc_request(
        seq,
        timestamp,
        request_id,
        rpc_request::Which::ClipRequest(pb::ClipboardRequest { which: Some(which) }),
    )
}

fn encode_clipboard_response(
    seq: i64,
    timestamp: i64,
    request_id: i64,
    which: clipboard_response::Which,
) -> Vec<u8> {
    encode_rpc_response(
        seq,
        timestamp,
        request_id,
        rpc_response::Which::ClipResponse(pb::ClipboardResponse { which: Some(which) }),
    )
}

fn encode_rpc_request(
    seq: i64,
    timestamp: i64,
    request_id: i64,
    which: rpc_request::Which,
) -> Vec<u8> {
    encode_message(&ControlMessage::RpcRequest {
        seq,
        timestamp,
        request: pb::RpcRequest {
            request_header: Some(pb::RequestHeader { request_id }),
            which: Some(which),
        },
    })
}

fn encode_rpc_response(
    seq: i64,
    timestamp: i64,
    request_id: i64,
    which: rpc_response::Which,
) -> Vec<u8> {
    encode_message(&ControlMessage::RpcResponse {
        seq,
        timestamp,
        response: pb::RpcResponse {
            response_header: Some(pb::ResponseHeader { request_id }),
            which: Some(which),
        },
    })
}

/// Extracts supported clipboard changes and RPCs. File transfer and drag-and-drop
/// actions are not interpreted.
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
        } => extract_request(*seq, *timestamp, request),
        ControlMessage::RpcResponse {
            seq,
            timestamp,
            response,
        } => extract_response(*seq, *timestamp, response),
        _ => None,
    }
}

fn extract_request(seq: i64, timestamp: i64, request: &pb::RpcRequest) -> Option<ClipboardMessage> {
    use clipboard_request::Which;

    let request_id = request.request_header.as_ref().map_or(0, |h| h.request_id);
    Some(match request.which.as_ref()? {
        rpc_request::Which::TextChangeRequest(req) => ClipboardMessage::TextRpcRequest {
            seq,
            timestamp,
            request_id,
            format_id: req.format_id,
            text: req.data.clone(),
        },
        rpc_request::Which::ClipRequest(clip) => match clip.which.as_ref()? {
            Which::FormatList(req) => ClipboardMessage::FormatListRequest {
                seq,
                timestamp,
                request_id,
                formats: req.formats.clone(),
            },
            Which::FormatDataAsk(req) => ClipboardMessage::FormatDataAsk {
                seq,
                timestamp,
                request_id,
                format_id: req.format_id,
                block_key: req.block_key.clone(),
                format_name: req.format_name.clone(),
            },
            Which::DataBlock(req) => ClipboardMessage::DataBlock {
                seq,
                timestamp,
                request_id,
                block_key: req.block_key.clone(),
                block_id: req.block_id,
                data: req.data.clone(),
            },
            _ => return None,
        },
        _ => return None,
    })
}

fn extract_response(
    seq: i64,
    timestamp: i64,
    response: &pb::RpcResponse,
) -> Option<ClipboardMessage> {
    use clipboard_response::Which;

    let request_id = response
        .response_header
        .as_ref()
        .map_or(0, |h| h.request_id);
    Some(match response.which.as_ref()? {
        rpc_response::Which::TextChangeResponse(resp) => ClipboardMessage::TextRpcResponse {
            seq,
            timestamp,
            request_id,
            err: resp.err,
        },
        rpc_response::Which::ClipResponse(clip) => match clip.which.as_ref()? {
            Which::FormatListResponse(_) => ClipboardMessage::FormatListResponse {
                seq,
                timestamp,
                request_id,
            },
            Which::FormatDataConfirm(resp) => ClipboardMessage::FormatDataConfirm {
                seq,
                timestamp,
                request_id,
                err: resp.err,
                block_key: resp.block_key.clone(),
                block_count: resp.block_count,
            },
            Which::DataBlockConfirm(resp) => ClipboardMessage::DataBlockConfirm {
                seq,
                timestamp,
                request_id,
                block_key: resp.block_key.clone(),
                block_id: resp.block_id,
                err: resp.err,
            },
            _ => return None,
        },
        _ => return None,
    })
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
    fn empty_text_has_no_chunks() {
        assert!(chunk_text("", CLIPBOARD_CHUNK_BYTES).is_empty());
    }

    #[test]
    fn windows_unicode_text_uses_predefined_format_without_a_name() {
        let format = windows_unicode_text_format();
        assert_eq!(format.id, 13);
        assert!(format.name.is_empty());
        assert_eq!(
            encode_format_list_request(1, 2, 3, vec![format]),
            [
                0x08, 1, 0x10, 2, 0xaa, 1, 12, 0x0a, 2, 0x08, 3, 0x4a, 6, 0x0a, 4, 0x0a, 2, 0x08,
                13
            ]
        );
        assert_eq!(
            encode_format_list_response(1, 2, 3),
            [
                0x08, 1, 0x10, 2, 0xb2, 1, 8, 0x0a, 2, 0x08, 3, 0x2a, 2, 0x0a, 0
            ]
        );
    }

    #[test]
    fn roundtrips_rich_clipboard_rpcs() {
        let formats = vec![
            windows_unicode_text_format(),
            pb::ClipboardFormat {
                id: 42,
                name: "other format".into(),
            },
        ];
        let cases = [
            (
                encode_format_list_request(1, 200, 12, formats.clone()),
                ClipboardMessage::FormatListRequest {
                    seq: 1,
                    timestamp: 200,
                    request_id: 12,
                    formats,
                },
            ),
            (
                encode_format_list_response(2, 201, 12),
                ClipboardMessage::FormatListResponse {
                    seq: 2,
                    timestamp: 201,
                    request_id: 12,
                },
            ),
            (
                encode_format_data_ask(3, 202, 13, 42, "block-key", "other format"),
                ClipboardMessage::FormatDataAsk {
                    seq: 3,
                    timestamp: 202,
                    request_id: 13,
                    format_id: 42,
                    block_key: "block-key".into(),
                    format_name: "other format".into(),
                },
            ),
            (
                encode_format_data_confirm(4, 203, 13, 1, "block-key", 2),
                ClipboardMessage::FormatDataConfirm {
                    seq: 4,
                    timestamp: 203,
                    request_id: 13,
                    err: 1,
                    block_key: "block-key".into(),
                    block_count: 2,
                },
            ),
            (
                encode_data_block(5, 204, 14, "block-key", 1, &[0, 128, 255]),
                ClipboardMessage::DataBlock {
                    seq: 5,
                    timestamp: 204,
                    request_id: 14,
                    block_key: "block-key".into(),
                    block_id: 1,
                    data: vec![0, 128, 255],
                },
            ),
            (
                encode_data_block_confirm(6, 205, 14, "block-key", 1, 2),
                ClipboardMessage::DataBlockConfirm {
                    seq: 6,
                    timestamp: 205,
                    request_id: 14,
                    block_key: "block-key".into(),
                    block_id: 1,
                    err: 2,
                },
            ),
        ];
        for (wire, expected) in cases {
            let ctrl = decode_message(&wire).unwrap();
            assert_eq!(extract_clipboard_message(&ctrl), Some(expected));
        }
    }

    #[test]
    fn data_block_debug_redacts_payload() {
        use prost::Message;

        let wire = encode_data_block(5, 204, 14, "block-key", 1, &[128, 129, 130]);
        let ctrl = decode_message(&wire).unwrap();
        let msg = extract_clipboard_message(&ctrl).unwrap();
        let raw = pb::Message::decode(wire.as_slice()).unwrap();
        for debug in [format!("{msg:?}"), format!("{raw:?}")] {
            assert!(debug.contains("len: 3"));
            assert!(!debug.contains("block-key"));
            assert!(!debug.contains("128, 129, 130"));
        }
    }

    #[test]
    fn deduper_ignores_empty_text_and_remembers_only_the_last_text() {
        let mut deduper = ClipboardDeduper::new();
        assert!(!deduper.should_apply(""));
        assert!(deduper.should_apply("first-secret"));
        assert!(!deduper.should_apply(""));
        assert!(!deduper.should_apply("first-secret"));
        assert!(deduper.should_apply("second-secret"));
        assert!(deduper.should_apply("first-secret"));
        assert_eq!(
            format!("{deduper:?}"),
            "ClipboardDeduper { has_last_text: true }"
        );
        deduper.reset();
        assert!(deduper.should_apply("first-secret"));
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

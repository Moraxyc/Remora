//! File transfer RPCs and sans-IO block accounting.

use std::collections::BTreeSet;
use std::fmt;

use prost::Message;

use super::{DataChannel, control::ControlMessage};
use crate::pb::{
    self, file_transfer_ftp_request, file_transfer_ftp_response, rpc_request, rpc_response,
};

/// File RPC replies use this channel even when the requests arrive on control.
pub const FILE_REPLY_CHANNEL: DataChannel = DataChannel::File;
pub const TASK_END_FILE_INDEX: i32 = -1;
pub const IOS_DEFAULT_UPLOAD_ALIAS: &str = ":/Default";
pub const HOST_ROOT_ALIAS: &str = ":/";
pub const HOST_HOME_ALIAS: &str = "~/";

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum FilePathError {
    EmptyPath,
}

impl fmt::Display for FilePathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyPath => f.write_str("empty file path"),
        }
    }
}

impl std::error::Error for FilePathError {}

/// Expands wire aliases without normalizing or authorizing the resulting path.
pub fn resolve_remote_path(raw: &str, default_download_dir: &str) -> Result<String, FilePathError> {
    validate_path(raw)?;
    let relative = if matches!(raw, IOS_DEFAULT_UPLOAD_ALIAS | "~") {
        Some("")
    } else {
        raw.strip_prefix(IOS_DEFAULT_UPLOAD_ALIAS)
            .and_then(|suffix| suffix.strip_prefix('/'))
            .or_else(|| raw.strip_prefix(HOST_HOME_ALIAS))
    };
    if let Some(relative) = relative {
        let base = default_download_dir.trim_end_matches('/');
        let relative = relative.trim_start_matches('/');
        return Ok(if relative.is_empty() && !base.is_empty() {
            base.to_owned()
        } else {
            format!("{base}/{relative}")
        });
    }
    Ok(raw
        .strip_prefix(HOST_ROOT_ALIAS)
        .map_or_else(|| raw.to_owned(), |relative| format!("/{relative}")))
}

pub fn validate_send_request(req: &pb::FileTransferSendRequest) -> Result<(), FilePathError> {
    validate_path(&req.path)
}

fn validate_path(path: &str) -> Result<(), FilePathError> {
    if path.trim().is_empty() {
        Err(FilePathError::EmptyPath)
    } else {
        Ok(())
    }
}

/// Identifies an outstanding operation without relying on either peer's file-index base.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FilePendingKey {
    Task { task_id: i32 },
    Block { task_id: i32, block_id: i32 },
}

pub fn pending_key_for_request(req: &file_transfer_ftp_request::Which) -> Option<FilePendingKey> {
    request_metadata(req).pending_key()
}

pub fn pending_key_for_response(
    resp: &file_transfer_ftp_response::Which,
) -> Option<FilePendingKey> {
    response_metadata(resp).pending_key()
}

/// Bounds the number of outstanding blocks. Any in-flight block can free its own slot.
#[derive(Debug)]
pub struct FileBlockWindow {
    window_size: usize,
    total_blocks: i32,
    next_to_send: i32,
    in_flight: BTreeSet<i32>,
    acked: usize,
}

impl FileBlockWindow {
    pub fn new(window_size: usize, total_blocks: i32) -> Self {
        Self {
            window_size: window_size.max(1),
            total_blocks: total_blocks.max(0),
            next_to_send: 0,
            in_flight: BTreeSet::new(),
            acked: 0,
        }
    }

    pub fn next_block_to_send(&mut self) -> Option<i32> {
        if self.next_to_send >= self.total_blocks || self.in_flight.len() >= self.window_size {
            return None;
        }
        let block_id = self.next_to_send;
        self.next_to_send += 1;
        self.in_flight.insert(block_id);
        Some(block_id)
    }

    pub fn ack_block(&mut self, block_id: i32) -> bool {
        if !self.in_flight.remove(&block_id) {
            return false;
        }
        self.acked += 1;
        true
    }

    pub fn in_flight_count(&self) -> usize {
        self.in_flight.len()
    }

    pub fn acked_count(&self) -> usize {
        self.acked
    }

    pub fn is_complete(&self) -> bool {
        self.next_to_send == self.total_blocks && self.in_flight.is_empty()
    }

    pub fn reset(&mut self, total_blocks: i32) {
        self.total_blocks = total_blocks.max(0);
        self.next_to_send = 0;
        self.in_flight.clear();
        self.acked = 0;
    }
}

#[derive(Clone, PartialEq)]
#[non_exhaustive]
pub enum FileMessage {
    Request {
        seq: i64,
        timestamp: i64,
        request_id: i64,
        request: pb::FileTransferFtpRequest,
    },
    Response {
        seq: i64,
        timestamp: i64,
        request_id: i64,
        response: pb::FileTransferFtpResponse,
    },
}

impl fmt::Debug for FileMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (name, seq, timestamp, request_id, metadata) = match self {
            Self::Request {
                seq,
                timestamp,
                request_id,
                request,
            } => (
                "Request",
                seq,
                timestamp,
                request_id,
                request.which.as_ref().map(request_metadata),
            ),
            Self::Response {
                seq,
                timestamp,
                request_id,
                response,
            } => (
                "Response",
                seq,
                timestamp,
                request_id,
                response.which.as_ref().map(response_metadata),
            ),
        };
        let mut debug = f.debug_struct(name);
        debug
            .field("seq", seq)
            .field("timestamp", timestamp)
            .field("request_id", request_id);
        if let Some(metadata) = metadata {
            debug.field("variant", &metadata.variant);
            if let Some(task_id) = metadata.task_id {
                debug.field("task_id", &task_id);
            }
            if let Some(block_id) = metadata.block_id {
                debug.field("block_id", &block_id);
            }
            if let Some(byte_len) = metadata.byte_len {
                debug.field("byte_len", &byte_len);
            }
            if let Some(error_code) = metadata.error_code {
                debug.field("error_code", &error_code);
            }
        }
        debug.finish()
    }
}

struct FileRpcMetadata {
    variant: &'static str,
    task_id: Option<i32>,
    block_id: Option<i32>,
    byte_len: Option<usize>,
    error_code: Option<i32>,
}

impl FileRpcMetadata {
    fn new(variant: &'static str, id: Option<&pb::TaskId>) -> Self {
        Self {
            variant,
            task_id: id.map(|id| id.task_id),
            block_id: None,
            byte_len: None,
            error_code: None,
        }
    }

    fn pending_key(&self) -> Option<FilePendingKey> {
        let task_id = self.task_id?;
        Some(match self.block_id {
            Some(block_id) => FilePendingKey::Block { task_id, block_id },
            None => FilePendingKey::Task { task_id },
        })
    }
}

fn request_metadata(req: &file_transfer_ftp_request::Which) -> FileRpcMetadata {
    use file_transfer_ftp_request::Which;

    match req {
        Which::ReceiveRequest(req) => FileRpcMetadata {
            byte_len: Some(req.list_data.len()),
            ..FileRpcMetadata::new("ReceiveRequest", req.id.as_ref())
        },
        Which::SendRequest(req) => FileRpcMetadata {
            byte_len: Some(req.list_data.len()),
            ..FileRpcMetadata::new("SendRequest", req.id.as_ref())
        },
        Which::FileAsk(req) => FileRpcMetadata::new("FileAsk", req.id.as_ref()),
        Which::FileBlock(req) => FileRpcMetadata {
            block_id: Some(req.block_id),
            byte_len: Some(req.data.len()),
            ..FileRpcMetadata::new("FileBlock", req.id.as_ref())
        },
        Which::Rename(_) => FileRpcMetadata::new("Rename", None),
        Which::DirCreate(_) => FileRpcMetadata::new("DirCreate", None),
        Which::RemoveFile(_) => FileRpcMetadata::new("RemoveFile", None),
        Which::RemoveDir(req) => FileRpcMetadata::new("RemoveDir", req.id.as_ref()),
        Which::Complete(req) => FileRpcMetadata {
            error_code: Some(req.error),
            ..FileRpcMetadata::new("Complete", req.id.as_ref())
        },
        Which::ReadDir(req) => FileRpcMetadata::new("ReadDir", req.id.as_ref()),
        Which::FileExist(_) => FileRpcMetadata::new("FileExist", None),
        Which::ClearSendTemp(_) => FileRpcMetadata::new("ClearSendTemp", None),
        Which::DirSizeRead(req) => FileRpcMetadata::new("DirSizeRead", req.id.as_ref()),
    }
}

fn response_metadata(resp: &file_transfer_ftp_response::Which) -> FileRpcMetadata {
    use file_transfer_ftp_response::Which;

    match resp {
        Which::ReceiveResponse(resp) => FileRpcMetadata {
            error_code: Some(resp.err),
            ..FileRpcMetadata::new("ReceiveResponse", resp.id.as_ref())
        },
        Which::SendResponse(resp) => FileRpcMetadata {
            byte_len: Some(resp.list_data.len()),
            error_code: Some(resp.err),
            ..FileRpcMetadata::new("SendResponse", resp.id.as_ref())
        },
        Which::FileConfirm(resp) => FileRpcMetadata {
            error_code: Some(resp.err),
            ..FileRpcMetadata::new("FileConfirm", resp.id.as_ref())
        },
        Which::BlockConfirm(resp) => FileRpcMetadata {
            block_id: Some(resp.block_id),
            byte_len: usize::try_from(resp.block_len).ok(),
            error_code: Some(resp.err),
            ..FileRpcMetadata::new("BlockConfirm", resp.id.as_ref())
        },
        Which::OperationResult(resp) => FileRpcMetadata {
            error_code: Some(resp.file_error),
            ..FileRpcMetadata::new("OperationResult", None)
        },
        Which::Result(resp) => FileRpcMetadata {
            error_code: Some(resp.file_error),
            ..FileRpcMetadata::new("Result", resp.id.as_ref())
        },
        Which::FileDirectory(resp) => FileRpcMetadata {
            byte_len: Some(resp.dir_data.len()),
            error_code: Some(resp.file_error),
            ..FileRpcMetadata::new("FileDirectory", resp.id.as_ref())
        },
        Which::FileExistResponse(_) => FileRpcMetadata::new("FileExistResponse", None),
    }
}

pub fn extract_file_message(msg: &ControlMessage) -> Option<FileMessage> {
    match msg {
        ControlMessage::RpcRequest {
            seq,
            timestamp,
            request,
        } => {
            let Some(rpc_request::Which::FileTransferFtpRequest(file)) = &request.which else {
                return None;
            };
            Some(FileMessage::Request {
                seq: *seq,
                timestamp: *timestamp,
                request_id: request.request_header.as_ref().map_or(0, |h| h.request_id),
                request: file.clone(),
            })
        }
        ControlMessage::RpcResponse {
            seq,
            timestamp,
            response,
        } => {
            let Some(rpc_response::Which::FileTransferFtpResponse(file)) = &response.which else {
                return None;
            };
            Some(FileMessage::Response {
                seq: *seq,
                timestamp: *timestamp,
                request_id: response
                    .response_header
                    .as_ref()
                    .map_or(0, |h| h.request_id),
                response: file.clone(),
            })
        }
        _ => None,
    }
}

pub fn encode_file_request(
    seq: i64,
    timestamp: i64,
    request_id: i64,
    which: file_transfer_ftp_request::Which,
) -> Vec<u8> {
    pb::Message {
        seq,
        timestamp,
        which: Some(pb::message::Which::RpcRequest(pb::RpcRequest {
            request_header: Some(pb::RequestHeader { request_id }),
            which: Some(rpc_request::Which::FileTransferFtpRequest(
                pb::FileTransferFtpRequest { which: Some(which) },
            )),
        })),
    }
    .encode_to_vec()
}

pub fn encode_file_response(
    seq: i64,
    timestamp: i64,
    request_id: i64,
    which: file_transfer_ftp_response::Which,
) -> Vec<u8> {
    pb::Message {
        seq,
        timestamp,
        which: Some(pb::message::Which::RpcResponse(pb::RpcResponse {
            response_header: Some(pb::ResponseHeader { request_id }),
            which: Some(rpc_response::Which::FileTransferFtpResponse(
                pb::FileTransferFtpResponse { which: Some(which) },
            )),
        })),
    }
    .encode_to_vec()
}

/// `RemoveDir` replies carry a task ID, unlike `OperationResult`.
pub fn encode_remove_dir_result(
    seq: i64,
    timestamp: i64,
    request_id: i64,
    id: pb::TaskId,
    error: pb::FileTransferError,
    err_msg: &str,
) -> Vec<u8> {
    encode_file_response(
        seq,
        timestamp,
        request_id,
        file_transfer_ftp_response::Which::Result(pb::FileTransferResult {
            id: Some(id),
            file_error: error as i32,
            err_msg: err_msg.to_owned(),
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::control::decode_message;
    use file_transfer_ftp_request::Which as Request;
    use file_transfer_ftp_response::Which as Response;

    #[test]
    fn resolves_aliases_without_rewriting_explicit_paths() {
        for (raw, expected) in [
            (":/Default", "/downloads"),
            (":/Default/", "/downloads"),
            (":/Default/sub", "/downloads/sub"),
            ("/Default", "/Default"),
            ("C:/Default", "C:/Default"),
            (":/DefaultExtra", "/DefaultExtra"),
            (":/", "/"),
            (":/tmp/a", "/tmp/a"),
            ("~", "/downloads"),
            ("~/", "/downloads"),
            ("~/docs", "/downloads/docs"),
            (" ~ ", " ~ "),
        ] {
            assert_eq!(resolve_remote_path(raw, "/downloads///").unwrap(), expected);
        }
        assert_eq!(resolve_remote_path(":/Default", "/").unwrap(), "/");
        assert_eq!(resolve_remote_path("~/docs", "/").unwrap(), "/docs");
        assert_eq!(resolve_remote_path("~/docs", "C:/").unwrap(), "C:/docs");
    }

    #[test]
    fn rejects_blank_paths_in_resolution_and_send_requests() {
        for raw in ["", " \t\n", "\u{2003}"] {
            assert_eq!(resolve_remote_path(raw, "/"), Err(FilePathError::EmptyPath));
            assert_eq!(
                validate_send_request(&pb::FileTransferSendRequest {
                    path: raw.into(),
                    ..Default::default()
                }),
                Err(FilePathError::EmptyPath)
            );
        }
        assert!(
            validate_send_request(&pb::FileTransferSendRequest {
                path: HOST_ROOT_ALIAS.into(),
                ..Default::default()
            })
            .is_ok()
        );
    }

    #[test]
    fn pending_keys_ignore_file_index_but_distinguish_blocks() {
        let request_id = pb::TaskId {
            task_id: 17,
            file_index: 1,
        };
        let response_id = pb::TaskId {
            task_id: 17,
            file_index: 0,
        };
        let task = Some(FilePendingKey::Task { task_id: 17 });
        let block = Some(FilePendingKey::Block {
            task_id: 17,
            block_id: 9,
        });

        let ask = Request::FileAsk(pb::FileTransferAsk {
            id: Some(request_id),
            ..Default::default()
        });
        let confirm = Response::FileConfirm(pb::FileTransferConfirm {
            id: Some(response_id),
            ..Default::default()
        });
        assert_eq!(pending_key_for_request(&ask), task);
        assert_eq!(pending_key_for_response(&confirm), task);

        let data = Request::FileBlock(pb::FileTransferBlock {
            id: Some(request_id),
            block_id: 9,
            ..Default::default()
        });
        let mut ack = pb::FileTransferBlockConfirm {
            id: Some(response_id),
            block_id: 9,
            ..Default::default()
        };
        assert_eq!(pending_key_for_request(&data), block);
        assert_eq!(
            pending_key_for_response(&Response::BlockConfirm(ack)),
            block
        );
        ack.block_id += 1;
        assert_ne!(
            pending_key_for_response(&Response::BlockConfirm(ack)),
            block
        );
        assert_eq!(
            pending_key_for_request(&Request::Rename(pb::FileRename::default())),
            None
        );
        assert_eq!(
            pending_key_for_response(&Response::Result(pb::FileTransferResult::default())),
            None
        );
    }

    #[test]
    fn out_of_order_acks_open_slots_before_earlier_blocks_finish() {
        let mut window = FileBlockWindow::new(2, 4);
        assert_eq!(window.next_block_to_send(), Some(0));
        assert_eq!(window.next_block_to_send(), Some(1));
        assert_eq!(window.next_block_to_send(), None);
        assert!(!window.ack_block(2));
        assert!(window.ack_block(1));
        assert!(!window.ack_block(1));
        assert_eq!(window.acked_count(), 1);
        assert_eq!(window.in_flight_count(), 1);
        assert_eq!(window.next_block_to_send(), Some(2));
        assert!(window.ack_block(2));
        assert_eq!(window.next_block_to_send(), Some(3));
        assert!(window.ack_block(3));
        assert_eq!(window.next_block_to_send(), None);
        assert!(!window.is_complete());
        assert!(window.ack_block(0));
        assert_eq!(window.acked_count(), 4);
        assert_eq!(window.in_flight_count(), 0);
        assert!(window.is_complete());
    }

    #[test]
    fn clamps_zero_window_and_resets_accounting() {
        let mut window = FileBlockWindow::new(0, -1);
        assert!(window.is_complete());
        assert_eq!(window.next_block_to_send(), None);
        window.reset(3);
        assert_eq!(window.next_block_to_send(), Some(0));
        assert_eq!(window.next_block_to_send(), None);
        assert!(window.ack_block(0));
        assert_eq!(window.next_block_to_send(), Some(1));
        window.reset(1);
        assert_eq!(window.acked_count(), 0);
        assert_eq!(window.in_flight_count(), 0);
        assert!(!window.ack_block(1));
        assert_eq!(window.next_block_to_send(), Some(0));
        assert!(window.ack_block(0));
        assert!(window.is_complete());
    }

    #[test]
    fn roundtrips_file_rpcs_and_redacts_paths_names_and_bytes() {
        let task = pb::TaskId {
            task_id: 27,
            file_index: 1,
        };
        let path = "/private/secret-path";
        let bytes = vec![128, 129, 130];
        let request_cases = [
            Request::SendRequest(pb::FileTransferSendRequest {
                id: Some(task),
                path: path.into(),
                list_data: bytes.clone(),
            }),
            Request::FileBlock(pb::FileTransferBlock {
                id: Some(task),
                block_id: 7,
                data: bytes.clone(),
            }),
        ];
        for which in request_cases {
            let wire = encode_file_request(1, 200, 99, which.clone());
            let ctrl = decode_message(&wire).unwrap();
            let file = extract_file_message(&ctrl).unwrap();
            assert_eq!(
                file,
                FileMessage::Request {
                    seq: 1,
                    timestamp: 200,
                    request_id: 99,
                    request: pb::FileTransferFtpRequest { which: Some(which) },
                }
            );
            let debug = format!("{file:?}");
            assert!(debug.contains("byte_len: 3"));
            assert!(debug.contains("task_id: 27"));
            if debug.contains("FileBlock") {
                assert!(debug.contains("block_id: 7"));
            }
            for debug in [debug, format!("{ctrl:?}")] {
                assert!(!debug.contains(path));
                assert!(!debug.contains("128, 129, 130"));
            }
        }

        let which = Response::SendResponse(pb::FileTransferSendResponse {
            id: Some(task),
            err: pb::FileTransferError::FtSucceeded as i32,
            files: vec![pb::FileInfo {
                rel_path: path.into(),
                ..Default::default()
            }],
            folder_name: "secret-name".into(),
            list_data: bytes,
        });
        let ctrl = decode_message(&encode_file_response(2, 201, 99, which.clone())).unwrap();
        let file = extract_file_message(&ctrl).unwrap();
        assert_eq!(
            file,
            FileMessage::Response {
                seq: 2,
                timestamp: 201,
                request_id: 99,
                response: pb::FileTransferFtpResponse { which: Some(which) },
            }
        );
        assert!(format!("{file:?}").contains("error_code: 1"));
        for debug in [format!("{ctrl:?}"), format!("{file:?}")] {
            assert!(!debug.contains(path));
            assert!(!debug.contains("secret-name"));
            assert!(!debug.contains("128, 129, 130"));
        }
    }

    #[test]
    fn remove_dir_reply_uses_result_with_task_id_and_error() {
        let id = pb::TaskId {
            task_id: 7,
            file_index: TASK_END_FILE_INDEX,
        };
        let error = pb::FileTransferError::FtPermissionDenied;
        let err_msg = "cannot remove /private/secret-path";
        let wire = encode_remove_dir_result(2, 3, 5, id, error, err_msg);
        let ctrl = decode_message(&wire).unwrap();
        let file = extract_file_message(&ctrl).unwrap();
        assert_eq!(
            file,
            FileMessage::Response {
                seq: 2,
                timestamp: 3,
                request_id: 5,
                response: pb::FileTransferFtpResponse {
                    which: Some(Response::Result(pb::FileTransferResult {
                        id: Some(id),
                        file_error: error as i32,
                        err_msg: err_msg.into(),
                    })),
                },
            }
        );
        let debug = format!("{file:?}");
        assert!(debug.contains("error_code: 4"));
        assert!(!debug.contains("secret-path"));
        assert_eq!(FILE_REPLY_CHANNEL.as_str(), "FILE_DATA_CHANNEL");
    }

    #[test]
    fn ignores_unrelated_rpcs() {
        let wire = crate::session::clipboard::encode_format_list_response(1, 2, 3);
        assert_eq!(extract_file_message(&decode_message(&wire).unwrap()), None);
    }
}

//! Generated from `proto/gv_pb.proto`.

use std::fmt;

include!(concat!(env!("OUT_DIR"), "/gv_pb.rs"));

impl fmt::Debug for ClipboardChange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClipboardChange")
            .field("content_id", &self.content_id)
            .field("len", &self.content.len())
            .field("to_be_continued", &self.to_be_continued)
            .finish()
    }
}

impl fmt::Debug for ClipboardTextChangeRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClipboardTextChangeRequest")
            .field("format_id", &self.format_id)
            .field("len", &self.data.len())
            .finish()
    }
}

impl fmt::Debug for ClipboardDataBlock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClipboardDataBlock")
            .field("block_id", &self.block_id)
            .field("len", &self.data.len())
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for ClipboardFileContentsResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClipboardFileContentsResponse")
            .field("task_id", &self.task_id)
            .field("list_index", &self.list_index)
            .field("err", &self.err)
            .field("len", &self.data.len())
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for FileTransferBlock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileTransferBlock")
            .field("id", &self.id)
            .field("block_id", &self.block_id)
            .field("len", &self.data.len())
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for PortMappingFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PortMappingFrame")
            .field("session_id", &self.session_id)
            .field("rule_id", &self.rule_id)
            .field("stream_id", &self.stream_id)
            .field("type", &self.r#type)
            .field("payload_len", &self.payload.len())
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for DrawStrokeRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DrawStrokeRequest")
            .field("stroke_id", &self.stroke_id)
            .field("points_len", &self.points.len())
            .field("screen_id", &self.screen_id)
            .field("has_line_width", &self.line_width.is_some())
            .field("has_color", &self.color.is_some())
            .finish_non_exhaustive()
    }
}

//! The only wire schema. Both the server and the sans-IO client use these types.
include!(concat!(env!("OUT_DIR"), "/agentplatform.v1.rs"));

pub const PROTOCOL_MAJOR: u32 = 1;
pub const SUBPROTOCOL: &str = "ryzhub.v1.protobuf";
/// Large frames to the client come as zstd frames (a message starting with zstd's magic
/// number), others as plain protobuf; frames from the client stay plain protobuf.
pub const SUBPROTOCOL_ZSTD: &str = "ryzhub.v1.protobuf+zstd";

pub fn frame(channel: impl Into<String>, body: frame::Body) -> Frame {
    Frame {
        channel: channel.into(),
        writer_epoch: 0,
        body: Some(body),
    }
}

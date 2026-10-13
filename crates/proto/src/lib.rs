//! The only wire schema. Both the server and the sans-IO client use these types.
include!(concat!(env!("OUT_DIR"), "/agentplatform.v1.rs"));

pub const PROTOCOL_MAJOR: u32 = 1;
pub const PROTOCOL_MINOR: u32 = 8;
pub const SUBPROTOCOL: &str = "ryzhub.v1.protobuf";
/// Large frames to the client come as zstd frames (a message starting with zstd's magic
/// number), others as plain protobuf; frames from the client stay plain protobuf (minor
/// version 6).
pub const SUBPROTOCOL_ZSTD: &str = "ryzhub.v1.protobuf+zstd";
/// Frames as JSON text, for debugging with text tools. Development servers only.
pub const SUBPROTOCOL_JSON: &str = "ryzhub.v1.json";

pub fn frame(channel: impl Into<String>, body: frame::Body) -> Frame {
    Frame {
        minor_version: PROTOCOL_MINOR,
        channel: channel.into(),
        ts_ms: 0,
        writer_epoch: 0,
        body: Some(body),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;

    #[test]
    fn binary_roundtrip_preserves_unicode_and_u64_offsets() {
        let old_cache: RunState =
            serde_json::from_str(r#"{"run_id":"r","state":"running","exec_epoch":1}"#).unwrap();
        assert!(!old_cache.read_only);
        let side = RunState {
            read_only: true,
            ..old_cache
        };
        assert_eq!(
            RunState::decode(side.encode_to_vec().as_slice()).unwrap(),
            side
        );
        let value = frame(
            "conv:example",
            frame::Body::Delta(Delta {
                item_id: "item".into(),
                stream: "text".into(),
                part_index: 0,
                offset: 9_007_199_254_740_993,
                text: "你好🦀".into(),
                base_seq: 12,
                revision: 1,
            }),
        );
        assert_eq!(
            Frame::decode(value.encode_to_vec().as_slice()).unwrap(),
            value
        );
    }
}

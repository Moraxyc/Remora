//! Deterministic malformed-input and boundary robustness tests for `remora-proto` codecs.

use remora_proto::control::{SignKey, share_code_digest, sign, verify, verify_share_code_digest};
use remora_proto::pb;
use remora_proto::session::clipboard::{
    ClipboardAssembler, ClipboardError, MAX_CLIPBOARD_BYTES, chunk_text, encode_clipboard_change,
    encode_data_block, extract_clipboard_message,
};
use remora_proto::session::control::decode_message;
use remora_proto::session::control::encode_message;
use remora_proto::session::file::encode_file_request;
use remora_proto::session::file::{
    FileBlockWindow, FilePathError, extract_file_message, resolve_remote_path,
};
use remora_proto::session::input::{InputDecodeError, decode_input};
use remora_proto::session::media::{decode_capture_index_ext, decode_new_frame_ext};
use remora_proto::session::system::extract_system_message;
use remora_proto::session::system::{encode_privacy_request, encode_virtual_audio_policy_request};
use remora_proto::session::terminal::encode_check_env_request;
use remora_proto::session::terminal::{
    MAX_TERM_PAYLOAD_BYTES, TERM_HEADER_LEN, TERM_MAGIC, TERM_VERSION, TermDecodeError,
    decode_term_frame, extract_terminal_control, resolve_close_target_session,
};
use remora_proto::session::tunnel::{TunnelSyn, encode_syn};
use remora_proto::session::tunnel::{decode_syn, decode_syn_ack, extract_tunnel_frame};
use remora_proto::signal::Decoder;

/// Simple deterministic xorshift64* PRNG so fuzz-like sweeps stay 100% reproducible.
struct XorShift64(u64);

impl XorShift64 {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn fill_bytes(&mut self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            let v = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&v[..chunk.len()]);
        }
    }
}

#[test]
fn protobuf_and_session_extractors_never_panic_on_arbitrary_bytes() {
    let mut rng = XorShift64(0x9E37_79B9_7F4A_7C15);
    let mut buf = Vec::with_capacity(256);

    for len in 0..=64 {
        for _ in 0..8 {
            buf.resize(len, 0);
            rng.fill_bytes(&mut buf);

            if let Ok(msg) = decode_message(&buf) {
                let _ = extract_clipboard_message(&msg);
                let _ = extract_file_message(&msg);
                let _ = extract_terminal_control(&msg);
                let _ = extract_tunnel_frame(&msg);
                let _ = extract_system_message(&msg);
            }
            let _ = decode_input(&buf);
            let _ = decode_term_frame(&buf);
            let _ = decode_syn(&buf);
            let _ = decode_syn_ack(&buf);
            let _ = decode_capture_index_ext(&buf);
            let _ = decode_new_frame_ext(&buf);
        }
    }

    // Seed valid envelopes and mutate via truncation, bit flips, and empty oneofs so
    // extractor branches are genuinely exercised on partial/corrupted payloads.
    let seeds = vec![
        encode_clipboard_change(1, 2, 3, "hello", false),
        encode_data_block(2, 3, 4, "k", 0, b"data"),
        encode_file_request(
            3,
            4,
            5,
            pb::file_transfer_ftp_request::Which::ReadDir(pb::ReadDir {
                id: Some(pb::TaskId {
                    task_id: 1,
                    file_index: 0,
                }),
                path: ":/Default".into(),
            }),
        ),
        encode_check_env_request(4, 5, 6),
        encode_syn(
            5,
            6,
            1,
            2,
            3,
            &TunnelSyn {
                version: 1,
                target_host: "127.0.0.1".into(),
                target_port: 80,
            },
        ),
        encode_privacy_request(
            6,
            7,
            8,
            pb::PrivateScreenSettingRequest {
                enable: true,
                privacy_type: pb::PrivateScreenType::PsNormalPrivacy as i32,
                setting_source: String::new(),
            },
        ),
        encode_virtual_audio_policy_request(7, 8, 9, 1),
        encode_message(
            &remora_proto::session::control::ControlMessage::RpcRequest {
                seq: 8,
                timestamp: 9,
                request: pb::RpcRequest::default(),
            },
        ),
        encode_message(
            &remora_proto::session::control::ControlMessage::RpcResponse {
                seq: 9,
                timestamp: 10,
                response: pb::RpcResponse::default(),
            },
        ),
        vec![],
        vec![0xFF; 32],
        vec![0x80; 32],
        vec![
            0x08, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x7F,
        ],
    ];

    let mut matched_extractors = 0usize;
    for seed in seeds {
        for cut in 0..=seed.len() {
            if let Ok(msg) = decode_message(&seed[..cut]) {
                matched_extractors += usize::from(extract_clipboard_message(&msg).is_some())
                    + usize::from(extract_file_message(&msg).is_some())
                    + usize::from(extract_terminal_control(&msg).is_some())
                    + usize::from(extract_tunnel_frame(&msg).is_some())
                    + usize::from(extract_system_message(&msg).is_some());
            }
        }
        for i in 0..seed.len() {
            let mut mutated = seed.clone();
            mutated[i] ^= 0x55;
            if let Ok(msg) = decode_message(&mutated) {
                let _ = extract_clipboard_message(&msg);
                let _ = extract_file_message(&msg);
                let _ = extract_terminal_control(&msg);
                let _ = extract_tunnel_frame(&msg);
                let _ = extract_system_message(&msg);
            }
        }
    }
    assert!(
        matched_extractors >= 6,
        "seeded mutations must reach extractors"
    );
}

#[test]
fn socketio_decoder_rejects_malformed_frames_and_recovers_cleanly() {
    let bad_text_frames = [
        "",
        "9",
        "0not-json",
        "4",
        "49",
        "42/admin,[\"ev\"]",
        "42not-json",
        "42{}",
        "42[]",
        "42[123]",
        "43[]",
        "4399999999999999999999999999[]",
        "45[\"no-dash\"]",
        "45abc-[\"bad-count\"]",
        "45-1-[\"neg\"]",
        "461-[\"ack-no-id\"]",
    ];
    for frame in bad_text_frames {
        let mut dec = Decoder::new();
        assert!(dec.text(frame).is_err(), "expected error for {frame:?}");
    }

    // Unexpected binary frame when not awaiting attachments.
    let mut dec = Decoder::new();
    assert!(dec.binary(b"orphan").is_err());

    // Text arriving while awaiting binary attachments fails and clears awaiting state.
    assert!(
        dec.text(r#"451-["be-controlled",{"_placeholder":true,"num":0}]"#)
            .unwrap()
            .is_none()
    );
    assert!(dec.is_awaiting_binary());
    assert!(dec.text("2").is_err());
    assert!(!dec.is_awaiting_binary());
    // Subsequent valid ping still decodes cleanly.
    assert!(dec.text("2").unwrap().is_some());
}

#[test]
fn term_frame_decoder_enforces_header_version_and_length_bounds() {
    // Every truncated length below TERM_HEADER_LEN fails with TooShort.
    for len in 0..TERM_HEADER_LEN {
        assert_eq!(
            decode_term_frame(&vec![0u8; len]),
            Err(TermDecodeError::TooShort)
        );
    }

    // Wrong magic.
    let mut bad_magic = [0u8; TERM_HEADER_LEN];
    bad_magic[..4].copy_from_slice(b"NOPE");
    assert_eq!(
        decode_term_frame(&bad_magic),
        Err(TermDecodeError::InvalidMagic)
    );

    // Unsupported version.
    let mut bad_ver = [0u8; TERM_HEADER_LEN];
    bad_ver[..4].copy_from_slice(&TERM_MAGIC);
    bad_ver[4] = TERM_VERSION + 1;
    assert_eq!(
        decode_term_frame(&bad_ver),
        Err(TermDecodeError::UnsupportedVersion(TERM_VERSION + 1))
    );

    // Declared length exceeds MAX_TERM_PAYLOAD_BYTES (both +1 and u32::MAX).
    for declared in [u32::try_from(MAX_TERM_PAYLOAD_BYTES + 1).unwrap(), u32::MAX] {
        let mut huge = [0u8; TERM_HEADER_LEN];
        huge[..4].copy_from_slice(&TERM_MAGIC);
        huge[4] = TERM_VERSION;
        huge[12..16].copy_from_slice(&declared.to_le_bytes());
        assert_eq!(
            decode_term_frame(&huge),
            Err(TermDecodeError::PayloadTooLarge(declared as usize))
        );
    }

    // Length mismatch (declared 4, actual 2).
    let mut mismatch = Vec::from([0u8; TERM_HEADER_LEN + 2]);
    mismatch[..4].copy_from_slice(&TERM_MAGIC);
    mismatch[4] = TERM_VERSION;
    mismatch[12..16].copy_from_slice(&4u32.to_le_bytes());
    assert_eq!(
        decode_term_frame(&mismatch),
        Err(TermDecodeError::LengthMismatch {
            expected: 4,
            actual: 2
        })
    );

    // resolve_close_target_session falls back to header_session_id on invalid or out-of-range JSON.
    assert_eq!(resolve_close_target_session(5, b"not-json"), 5);
    assert_eq!(resolve_close_target_session(5, br#"{"session_id":-1}"#), 5);
    assert_eq!(
        resolve_close_target_session(5, br#"{"session_id":4294967296}"#),
        5
    );
    assert_eq!(resolve_close_target_session(5, br#"{"session_id":42}"#), 42);
}

#[test]
fn input_decoder_rejects_out_of_range_and_non_finite_numbers() {
    assert_eq!(
        decode_input(br#"{"action":"kbd_press","key":999999999999}"#),
        Err(InputDecodeError::InvalidField("key"))
    );
    assert_eq!(
        decode_input(br#"{"action":"mouse_move_absolute","abs_x":1e400,"abs_y":0}"#),
        Err(InputDecodeError::InvalidJson)
    );
    assert_eq!(
        decode_input(br#"{"action":"mouse_move_absolute","abs_x":"bad","abs_y":0}"#),
        Err(InputDecodeError::InvalidField("abs_x"))
    );
    assert_eq!(
        decode_input(br#"{"action":"text_input","content":123}"#),
        Err(InputDecodeError::InvalidField("content"))
    );
    assert_eq!(decode_input(b"{"), Err(InputDecodeError::InvalidJson));
}

#[test]
fn clipboard_assembler_enforces_exact_limit_and_utf8_chunk_boundaries() {
    let mut asm = ClipboardAssembler::new();
    let half = "a".repeat(MAX_CLIPBOARD_BYTES / 2);
    assert_eq!(asm.accept(1, &half, true), Ok(None));
    let other_half = "b".repeat(MAX_CLIPBOARD_BYTES - half.len());
    let assembled = asm.accept(1, &other_half, false).unwrap().unwrap();
    assert_eq!(assembled.len(), MAX_CLIPBOARD_BYTES);

    // Exceeding by 1 byte returns ExceedsLimit.
    let mut asm = ClipboardAssembler::new();
    let max_str = "x".repeat(MAX_CLIPBOARD_BYTES);
    assert_eq!(asm.accept(2, &max_str, true), Ok(None));
    assert_eq!(asm.accept(2, "y", false), Err(ClipboardError::ExceedsLimit));

    // Multi-byte UTF-8 chunks never split inside a scalar value even with tiny limit.
    let text = "你好🦀world🌍测试";
    for limit in 0..=8 {
        let chunks = chunk_text(text, limit);
        assert_eq!(chunks.concat(), text);
        assert!(
            chunks
                .iter()
                .all(|c| !c.is_empty() && c.len() <= limit.max(4))
        );
    }
}

#[test]
fn file_block_window_and_paths_handle_degenerate_and_out_of_order_cases() {
    // window_size=0 clamps to 1, negative total_blocks clamps to 0.
    let mut empty = FileBlockWindow::new(0, -5);
    assert!(empty.is_complete());
    assert_eq!(empty.next_block_to_send(), None);
    assert!(!empty.ack_block(0));

    let mut win = FileBlockWindow::new(3, 5);
    assert_eq!(win.next_block_to_send(), Some(0));
    assert_eq!(win.next_block_to_send(), Some(1));
    assert_eq!(win.next_block_to_send(), Some(2));
    assert_eq!(win.next_block_to_send(), None);

    // Out-of-order and duplicate acks.
    assert!(win.ack_block(2));
    assert!(!win.ack_block(2));
    assert!(!win.ack_block(99));
    assert_eq!(win.next_block_to_send(), Some(3));
    assert!(win.ack_block(0));
    assert_eq!(win.next_block_to_send(), Some(4));
    assert_eq!(win.next_block_to_send(), None);
    assert!(win.ack_block(1));
    assert!(win.ack_block(3));
    assert!(!win.is_complete());
    assert!(win.ack_block(4));
    assert!(win.is_complete());
    assert_eq!(win.acked_count(), 5);

    assert_eq!(
        resolve_remote_path("", "/dl"),
        Err(FilePathError::EmptyPath)
    );
    assert_eq!(
        resolve_remote_path("   \t\n ", "/dl"),
        Err(FilePathError::EmptyPath)
    );
}

#[test]
fn signing_and_share_digest_handle_edge_cases() {
    let key = SignKey::new("");
    let sig = sign(
        &key,
        "post",
        "/api/v1/test?a=1",
        [
            ("X-Param-B", "2"),
            ("x-param-a", "1"),
            ("x-param-sign", "ignored"),
            ("authorization", "ignored"),
        ],
        b"",
    );
    assert!(verify(
        &key,
        "POST",
        "/api/v1/test?a=1",
        [("x-param-a", "1"), ("x-param-b", "2")],
        b"",
        &sig
    ));
    assert!(!verify(
        &key,
        "POST",
        "/api/v1/test?a=1",
        [],
        b"",
        "not-hex"
    ));
    assert!(!verify(
        &key,
        "POST",
        "/api/v1/test?a=1",
        [],
        b"",
        "deadbeef"
    ));

    let digest = share_code_digest("salt", "123456");
    assert!(verify_share_code_digest("salt", "123456", &digest));
    assert!(!verify_share_code_digest("salt", "123457", &digest));
    assert!(!verify_share_code_digest("salt", "123456", "zz"));
    assert!(!verify_share_code_digest("salt", "123456", "abcd"));
}

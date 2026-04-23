use prost::Message;

use crate::generated::zellij::relay::v1 as proto;
use crate::{decode_control_frame, decode_terminal_frame, ControlMessage, TerminalMessage, PROTOCOL_VERSION};

#[test]
fn control_auth_roundtrip() {
    let original = ControlMessage::Auth {
        token: "t".into(),
        session_name: "s".into(),
        protocol_version: PROTOCOL_VERSION,
        zellij_version: "z".into(),
        requested_slug: "prev-slug".into(),
    };
    let bytes = original.encode();
    let decoded = decode_control_frame(&bytes).expect("decode ok");
    match decoded {
        ControlMessage::Auth {
            token,
            session_name,
            protocol_version,
            zellij_version,
            requested_slug,
        } => {
            assert_eq!(token, "t");
            assert_eq!(session_name, "s");
            assert_eq!(protocol_version, PROTOCOL_VERSION);
            assert_eq!(zellij_version, "z");
            assert_eq!(requested_slug, "prev-slug");
        },
        other => panic!("expected Auth, got {:?}", other),
    }
}

#[test]
fn control_established_roundtrip() {
    let original = ControlMessage::Established {
        public_url: "http://localhost:8765/r/abc".into(),
        slug: "abc".into(),
        tunnel_id: "deadbeef-0000".into(),
    };
    let bytes = original.encode();
    let decoded = decode_control_frame(&bytes).expect("decode ok");
    match decoded {
        ControlMessage::Established {
            public_url,
            slug,
            tunnel_id,
        } => {
            assert_eq!(public_url, "http://localhost:8765/r/abc");
            assert_eq!(slug, "abc");
            assert_eq!(tunnel_id, "deadbeef-0000");
        },
        other => panic!("expected Established, got {:?}", other),
    }
}

#[test]
fn control_error_roundtrip() {
    let original = ControlMessage::Error {
        message: "something went wrong".into(),
    };
    let bytes = original.encode();
    let decoded = decode_control_frame(&bytes).expect("decode ok");
    match decoded {
        ControlMessage::Error { message } => {
            assert_eq!(message, "something went wrong");
        },
        other => panic!("expected Error, got {:?}", other),
    }
}

#[test]
fn terminal_ready_roundtrip() {
    let original = TerminalMessage::Ready {
        tunnel_id: "t-id-123".into(),
    };
    let bytes = original.encode();
    let decoded = decode_terminal_frame(&bytes).expect("decode ok");
    match decoded {
        TerminalMessage::Ready { tunnel_id } => {
            assert_eq!(tunnel_id, "t-id-123");
        },
        other => panic!("expected Ready, got {:?}", other),
    }
}

#[test]
fn terminal_error_roundtrip() {
    let original = TerminalMessage::Error {
        message: "bad tunnel".into(),
    };
    let bytes = original.encode();
    let decoded = decode_terminal_frame(&bytes).expect("decode ok");
    match decoded {
        TerminalMessage::Error { message } => {
            assert_eq!(message, "bad tunnel");
        },
        other => panic!("expected Error, got {:?}", other),
    }
}

#[test]
fn control_frame_without_payload_errors() {
    let frame = proto::ControlFrame { payload: None };
    let bytes = frame.encode_to_vec();
    let err = decode_control_frame(&bytes).expect_err("should fail");
    let msg = format!("{}", err);
    assert!(
        msg.contains("no payload"),
        "expected error mentioning 'no payload', got: {msg}"
    );
}

#[test]
fn terminal_frame_without_payload_errors() {
    let frame = proto::TerminalFrame { payload: None };
    let bytes = frame.encode_to_vec();
    let err = decode_terminal_frame(&bytes).expect_err("should fail");
    let msg = format!("{}", err);
    assert!(
        msg.contains("no payload"),
        "expected error mentioning 'no payload', got: {msg}"
    );
}

#[test]
fn control_frame_tolerates_unknown_trailing_bytes() {
    let original = ControlMessage::Auth {
        token: "tok".into(),
        session_name: "sess".into(),
        protocol_version: PROTOCOL_VERSION,
        zellij_version: "0.45.0".into(),
        requested_slug: String::new(),
    };
    let mut bytes = original.encode();
    bytes.extend_from_slice(&[0x78, 0x2a]);
    let decoded = decode_control_frame(&bytes).expect("decode should tolerate unknown bytes");
    match decoded {
        ControlMessage::Auth {
            token,
            session_name,
            protocol_version,
            zellij_version,
            requested_slug: _,
        } => {
            assert_eq!(token, "tok");
            assert_eq!(session_name, "sess");
            assert_eq!(protocol_version, PROTOCOL_VERSION);
            assert_eq!(zellij_version, "0.45.0");
        },
        other => panic!("expected Auth, got {:?}", other),
    }
}

#[test]
fn protocol_version_is_one() {
    assert_eq!(PROTOCOL_VERSION, 1);
}

#[test]
fn auth_challenge_roundtrip() {
    let original = ControlMessage::AuthChallenge {
        request_id: vec![1, 2, 3, 4],
        token_hash: "abc".into(),
    };
    let decoded = decode_control_frame(&original.encode()).unwrap();
    match decoded {
        ControlMessage::AuthChallenge {
            request_id,
            token_hash,
        } => {
            assert_eq!(request_id, vec![1, 2, 3, 4]);
            assert_eq!(token_hash, "abc");
        },
        other => panic!("expected AuthChallenge, got {:?}", other),
    }
}

#[test]
fn auth_response_roundtrip() {
    let original = ControlMessage::AuthResponse {
        request_id: vec![9, 9],
        client_id: 42,
        accepted: true,
        is_read_only: true,
        session_token_hash: "hash".into(),
        e2e_encrypted: true,
    };
    let decoded = decode_control_frame(&original.encode()).unwrap();
    match decoded {
        ControlMessage::AuthResponse {
            request_id,
            client_id,
            accepted,
            is_read_only,
            session_token_hash,
            e2e_encrypted,
        } => {
            assert_eq!(request_id, vec![9, 9]);
            assert_eq!(client_id, 42);
            assert!(accepted);
            assert!(is_read_only);
            assert_eq!(session_token_hash, "hash");
            assert!(e2e_encrypted);
        },
        other => panic!("expected AuthResponse, got {:?}", other),
    }
}

#[test]
fn auth_response_e2e_flag_roundtrip_false() {
    let original = ControlMessage::AuthResponse {
        request_id: vec![1],
        client_id: 1,
        accepted: true,
        is_read_only: false,
        session_token_hash: "h".into(),
        e2e_encrypted: false,
    };
    match decode_control_frame(&original.encode()).unwrap() {
        ControlMessage::AuthResponse { e2e_encrypted, .. } => assert!(!e2e_encrypted),
        other => panic!("expected AuthResponse, got {:?}", other),
    }
}

#[test]
fn client_connected_and_disconnected_roundtrip() {
    let a = ControlMessage::ClientConnected { client_id: 7 };
    match decode_control_frame(&a.encode()).unwrap() {
        ControlMessage::ClientConnected { client_id } => assert_eq!(client_id, 7),
        other => panic!("expected ClientConnected, got {:?}", other),
    }

    let b = ControlMessage::ClientDisconnected { client_id: 7 };
    match decode_control_frame(&b.encode()).unwrap() {
        ControlMessage::ClientDisconnected { client_id } => assert_eq!(client_id, 7),
        other => panic!("expected ClientDisconnected, got {:?}", other),
    }
}

#[test]
fn control_frame_data_roundtrip() {
    let original = ControlMessage::ControlFrameData {
        client_id: 3,
        data: b"hello".to_vec(),
    };
    match decode_control_frame(&original.encode()).unwrap() {
        ControlMessage::ControlFrameData { client_id, data } => {
            assert_eq!(client_id, 3);
            assert_eq!(data, b"hello");
        },
        other => panic!("expected ControlFrameData, got {:?}", other),
    }
}

#[test]
fn read_only_viewer_update_zero_count_roundtrip() {
    let original = ControlMessage::ReadOnlyViewerUpdate {
        token_hash: "deadbeef".into(),
        count: 0,
    };
    match decode_control_frame(&original.encode()).unwrap() {
        ControlMessage::ReadOnlyViewerUpdate { token_hash, count } => {
            assert_eq!(token_hash, "deadbeef");
            assert_eq!(count, 0);
        },
        other => panic!("expected ReadOnlyViewerUpdate, got {:?}", other),
    }
}

#[test]
fn read_only_viewer_update_positive_count_roundtrip() {
    let original = ControlMessage::ReadOnlyViewerUpdate {
        token_hash: "cafebabe".into(),
        count: 17,
    };
    match decode_control_frame(&original.encode()).unwrap() {
        ControlMessage::ReadOnlyViewerUpdate { token_hash, count } => {
            assert_eq!(token_hash, "cafebabe");
            assert_eq!(count, 17);
        },
        other => panic!("expected ReadOnlyViewerUpdate, got {:?}", other),
    }
}

#[test]
fn session_size_roundtrip() {
    let original = ControlMessage::SessionSize {
        client_id: 7,
        rows: 40,
        cols: 120,
    };
    match decode_control_frame(&original.encode()).unwrap() {
        ControlMessage::SessionSize {
            client_id,
            rows,
            cols,
        } => {
            assert_eq!(client_id, 7);
            assert_eq!(rows, 40);
            assert_eq!(cols, 120);
        },
        other => panic!("expected SessionSize, got {:?}", other),
    }
}

#[test]
fn session_size_zero_dimensions_roundtrip() {
    let original = ControlMessage::SessionSize {
        client_id: 0,
        rows: 0,
        cols: 0,
    };
    match decode_control_frame(&original.encode()).unwrap() {
        ControlMessage::SessionSize {
            client_id,
            rows,
            cols,
        } => {
            assert_eq!(client_id, 0);
            assert_eq!(rows, 0);
            assert_eq!(cols, 0);
        },
        other => panic!("expected SessionSize, got {:?}", other),
    }
}

#[test]
fn terminal_frame_data_roundtrip() {
    let original = TerminalMessage::TerminalFrameData {
        client_id: 11,
        data: vec![0xde, 0xad, 0xbe, 0xef],
    };
    match decode_terminal_frame(&original.encode()).unwrap() {
        TerminalMessage::TerminalFrameData { client_id, data } => {
            assert_eq!(client_id, 11);
            assert_eq!(data, vec![0xde, 0xad, 0xbe, 0xef]);
        },
        other => panic!("expected TerminalFrameData, got {:?}", other),
    }
}

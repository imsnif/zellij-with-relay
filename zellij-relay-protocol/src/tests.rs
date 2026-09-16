use prost::Message;

use crate::generated::zellij::relay::v1 as proto;
use crate::{
    decode_control_frame, decode_terminal_frame, ControlMessage, TerminalMessage, TunnelErrorCode,
    PROTOCOL_VERSION,
};

#[test]
fn control_auth_roundtrip() {
    let original = ControlMessage::Auth {
        token: "t".into(),
        session_name: "s".into(),
        protocol_version: PROTOCOL_VERSION,
        zellij_version: "z".into(),
        requested_slug: "prev-slug".into(),
        read_only: true,
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
            read_only,
        } => {
            assert_eq!(token, "t");
            assert_eq!(session_name, "s");
            assert_eq!(protocol_version, PROTOCOL_VERSION);
            assert_eq!(zellij_version, "z");
            assert_eq!(requested_slug, "prev-slug");
            assert!(read_only);
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
        terminal_binding_secret: "a".repeat(64),
    };
    let bytes = original.encode();
    let decoded = decode_control_frame(&bytes).expect("decode ok");
    match decoded {
        ControlMessage::Established {
            public_url,
            slug,
            tunnel_id,
            terminal_binding_secret,
        } => {
            assert_eq!(public_url, "http://localhost:8765/r/abc");
            assert_eq!(slug, "abc");
            assert_eq!(tunnel_id, "deadbeef-0000");
            assert_eq!(terminal_binding_secret, "a".repeat(64));
        },
        other => panic!("expected Established, got {:?}", other),
    }
}

#[test]
fn control_error_roundtrip() {
    let original = ControlMessage::Error {
        message: "something went wrong".into(),
        code: TunnelErrorCode::ProtocolVersionUnsupported {
            supported_min: 1,
            supported_max: 2,
            offered_version: 9,
        },
    };
    let bytes = original.encode();
    let decoded = decode_control_frame(&bytes).expect("decode ok");
    match decoded {
        ControlMessage::Error { message, code } => {
            assert_eq!(message, "something went wrong");
            assert_eq!(
                code,
                TunnelErrorCode::ProtocolVersionUnsupported {
                    supported_min: 1,
                    supported_max: 2,
                    offered_version: 9,
                }
            );
        },
        other => panic!("expected Error, got {:?}", other),
    }
}

#[test]
fn terminal_ready_roundtrip() {
    let original = TerminalMessage::Ready {
        tunnel_id: "t-id-123".into(),
        binding_secret: "b".repeat(64),
    };
    let bytes = original.encode();
    let decoded = decode_terminal_frame(&bytes).expect("decode ok");
    match decoded {
        TerminalMessage::Ready {
            tunnel_id,
            binding_secret,
        } => {
            assert_eq!(tunnel_id, "t-id-123");
            assert_eq!(binding_secret, "b".repeat(64));
        },
        other => panic!("expected Ready, got {:?}", other),
    }
}

#[test]
fn terminal_error_roundtrip() {
    let original = TerminalMessage::Error {
        message: "bad tunnel".into(),
        code: TunnelErrorCode::AuthRejected,
    };
    let bytes = original.encode();
    let decoded = decode_terminal_frame(&bytes).expect("decode ok");
    match decoded {
        TerminalMessage::Error { message, code } => {
            assert_eq!(message, "bad tunnel");
            assert_eq!(code, TunnelErrorCode::AuthRejected);
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
fn tunnel_auth_missing_credential_errors() {
    let auth = proto::TunnelAuth {
        credential: None,
        session_name: "s".into(),
        protocol_version: PROTOCOL_VERSION,
        zellij_version: "z".into(),
        requested_slug: String::new(),
        read_only: false,
    };
    let frame = proto::ControlFrame {
        payload: Some(proto::control_frame::Payload::Auth(auth)),
    };
    let bytes = frame.encode_to_vec();
    let err = decode_control_frame(&bytes).expect_err("should fail");
    let msg = format!("{}", err);
    assert!(
        msg.contains("TunnelAuth missing credential"),
        "expected error mentioning 'TunnelAuth missing credential', got: {msg}"
    );
}

#[test]
fn control_message_debug_redacts_secrets() {
    let auth = ControlMessage::Auth {
        token: "SUPERSECRET".into(),
        session_name: "s".into(),
        protocol_version: PROTOCOL_VERSION,
        zellij_version: "z".into(),
        requested_slug: String::new(),
        read_only: false,
    };
    let debug = format!("{:?}", auth);
    assert!(!debug.contains("SUPERSECRET"), "debug leaked token: {debug}");
    assert!(debug.contains("<redacted>"), "debug missing redaction marker: {debug}");

    let established = ControlMessage::Established {
        public_url: "http://localhost/r/abc".into(),
        slug: "abc".into(),
        tunnel_id: "t-1".into(),
        terminal_binding_secret: "SUPERSECRET".into(),
    };
    let debug = format!("{:?}", established);
    assert!(!debug.contains("SUPERSECRET"), "debug leaked binding secret: {debug}");
    assert!(debug.contains("<redacted>"), "debug missing redaction marker: {debug}");
}

#[test]
fn terminal_message_debug_redacts_secrets() {
    let ready = TerminalMessage::Ready {
        tunnel_id: "t-1".into(),
        binding_secret: "SUPERSECRET".into(),
    };
    let debug = format!("{:?}", ready);
    assert!(!debug.contains("SUPERSECRET"), "debug leaked binding secret: {debug}");
    assert!(debug.contains("<redacted>"), "debug missing redaction marker: {debug}");
}

#[test]
fn control_frame_tolerates_unknown_trailing_bytes() {
    let original = ControlMessage::Auth {
        token: "tok".into(),
        session_name: "sess".into(),
        protocol_version: PROTOCOL_VERSION,
        zellij_version: "0.45.0".into(),
        requested_slug: String::new(),
        read_only: false,
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
            read_only: _,
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
fn pake_challenge_roundtrip() {
    let original = ControlMessage::PakeChallenge {
        request_id: vec![1, 2, 3, 4],
        viewer_msg: vec![0xaa, 0xbb, 0xcc],
        link_id: vec![0x11; 16],
    };
    match decode_control_frame(&original.encode()).unwrap() {
        ControlMessage::PakeChallenge {
            request_id,
            viewer_msg,
            link_id,
        } => {
            assert_eq!(request_id, vec![1, 2, 3, 4]);
            assert_eq!(viewer_msg, vec![0xaa, 0xbb, 0xcc]);
            assert_eq!(link_id, vec![0x11; 16]);
        },
        other => panic!("expected PakeChallenge, got {:?}", other),
    }
}

#[test]
fn pake_challenge_empty_link_id_roundtrip() {
    let original = ControlMessage::PakeChallenge {
        request_id: vec![7],
        viewer_msg: vec![0x01],
        link_id: Vec::new(),
    };
    match decode_control_frame(&original.encode()).unwrap() {
        ControlMessage::PakeChallenge { link_id, .. } => assert!(link_id.is_empty()),
        other => panic!("expected PakeChallenge, got {:?}", other),
    }
}

#[test]
fn pake_response_roundtrip() {
    let original = ControlMessage::PakeResponse {
        request_id: vec![9, 9],
        client_id: 42,
        accepted: true,
        sharer_msg: vec![1, 2, 3],
        sharer_confirm: vec![4, 5, 6],
    };
    match decode_control_frame(&original.encode()).unwrap() {
        ControlMessage::PakeResponse {
            request_id,
            client_id,
            accepted,
            sharer_msg,
            sharer_confirm,
        } => {
            assert_eq!(request_id, vec![9, 9]);
            assert_eq!(client_id, 42);
            assert!(accepted);
            assert_eq!(sharer_msg, vec![1, 2, 3]);
            assert_eq!(sharer_confirm, vec![4, 5, 6]);
        },
        other => panic!("expected PakeResponse, got {:?}", other),
    }
}

#[test]
fn pake_response_rejected_roundtrip() {
    let original = ControlMessage::PakeResponse {
        request_id: vec![1],
        client_id: 0,
        accepted: false,
        sharer_msg: vec![],
        sharer_confirm: vec![],
    };
    match decode_control_frame(&original.encode()).unwrap() {
        ControlMessage::PakeResponse { accepted, .. } => assert!(!accepted),
        other => panic!("expected PakeResponse, got {:?}", other),
    }
}

#[test]
fn pake_confirm_roundtrip() {
    let original = ControlMessage::PakeConfirm {
        request_id: vec![7],
        viewer_confirm: vec![0xde, 0xad],
    };
    match decode_control_frame(&original.encode()).unwrap() {
        ControlMessage::PakeConfirm {
            request_id,
            viewer_confirm,
        } => {
            assert_eq!(request_id, vec![7]);
            assert_eq!(viewer_confirm, vec![0xde, 0xad]);
        },
        other => panic!("expected PakeConfirm, got {:?}", other),
    }
}

#[test]
fn pake_result_roundtrip() {
    for accepted in [true, false] {
        let original = ControlMessage::PakeResult {
            request_id: vec![5, 5],
            client_id: 13,
            accepted,
        };
        match decode_control_frame(&original.encode()).unwrap() {
            ControlMessage::PakeResult {
                request_id,
                client_id,
                accepted: a,
            } => {
                assert_eq!(request_id, vec![5, 5]);
                assert_eq!(client_id, 13);
                assert_eq!(a, accepted);
            },
            other => panic!("expected PakeResult, got {:?}", other),
        }
    }
}

#[test]
fn client_disconnected_roundtrip() {
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

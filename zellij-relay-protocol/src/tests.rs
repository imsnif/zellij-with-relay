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
    };
    let bytes = original.encode();
    let decoded = decode_control_frame(&bytes).expect("decode ok");
    match decoded {
        ControlMessage::Auth {
            token,
            session_name,
            protocol_version,
            zellij_version,
        } => {
            assert_eq!(token, "t");
            assert_eq!(session_name, "s");
            assert_eq!(protocol_version, PROTOCOL_VERSION);
            assert_eq!(zellij_version, "z");
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

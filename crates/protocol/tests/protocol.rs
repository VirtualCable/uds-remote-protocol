//! Tests del crate `protocol`: handshake, mensajes de control y canales.

use protocol::{
    Capability, ChannelId, Command, ControlMessage, HANDSHAKE_SIGNATURE, Hello, PROTOCOL_VERSION,
    ProtocolError, channel,
};

#[test]
fn hello_roundtrip() {
    let hello = Hello {
        version: PROTOCOL_VERSION,
        command: Command::Open,
        ticket: [0x42; 48],
    };
    let bytes = hello.encode();
    let decoded = Hello::decode(&bytes).unwrap();
    assert_eq!(decoded, hello);
}

#[test]
fn hello_has_signature_and_fixed_size() {
    let hello = Hello {
        version: PROTOCOL_VERSION,
        command: Command::Test,
        ticket: [0; 48],
    };
    let bytes = hello.encode();
    assert_eq!(&bytes[..8], &HANDSHAKE_SIGNATURE);
    assert_eq!(bytes.len(), 8 + 2 + 1 + 48);
}

#[test]
fn hello_bad_signature_is_rejected() {
    let hello = Hello {
        version: PROTOCOL_VERSION,
        command: Command::Open,
        ticket: [0; 48],
    };
    let mut bytes = hello.encode();
    bytes[0] ^= 0xFF;
    let err = Hello::decode(&bytes).unwrap_err();
    assert!(matches!(err, ProtocolError::BadSignature));
}

#[test]
fn hello_unknown_command_is_rejected() {
    let hello = Hello {
        version: PROTOCOL_VERSION,
        command: Command::Open,
        ticket: [0; 48],
    };
    let mut bytes = hello.encode();
    bytes[10] = 0xEE;
    let err = Hello::decode(&bytes).unwrap_err();
    assert!(matches!(err, ProtocolError::UnknownCommand(0xEE)));
}

#[test]
fn hello_truncated_is_rejected() {
    let hello = Hello {
        version: PROTOCOL_VERSION,
        command: Command::Recover,
        ticket: [9; 48],
    };
    let bytes = hello.encode();
    assert!(Hello::decode(&bytes[..20]).is_err());
}

#[test]
fn control_message_roundtrip() {
    let msg = ControlMessage::ServerCaps {
        caps: vec![
            Capability {
                kind: 1,
                version: 1,
                params: vec![1, 2, 3],
            },
            Capability {
                kind: 2,
                version: 3,
                params: vec![],
            },
        ],
    };
    let bytes = msg.encode().unwrap();
    let decoded = ControlMessage::decode(&bytes).unwrap();
    assert_eq!(decoded, msg);
}

#[test]
fn control_message_variants_roundtrip() {
    let msgs = [
        ControlMessage::ChannelOpen {
            id: channel::DISPLAY,
            kind: 1,
            version: 1,
        },
        ControlMessage::ChannelOpenAck { id: channel::DISPLAY },
        ControlMessage::UnsupportedChannel { id: 0x0100 },
        ControlMessage::Keepalive,
    ];
    for msg in msgs {
        let bytes = msg.encode().unwrap();
        let decoded = ControlMessage::decode(&bytes).unwrap();
        assert_eq!(decoded, msg);
    }
}

#[test]
fn control_message_garbage_is_rejected() {
    assert!(ControlMessage::decode(&[0xFF, 0xFF, 0xFF]).is_err());
}

#[test]
fn core_channel_ids_are_in_core_range() {
    for id in [
        channel::CONTROL,
        channel::DISPLAY,
        channel::AUDIO_OUT,
        channel::AUDIO_IN,
        channel::INPUT,
    ] {
        assert!(id <= channel::CORE_MAX);
    }
}

#[test]
fn extension_range_starts_after_core() {
    let ext: ChannelId = channel::CORE_MAX + 1;
    assert_eq!(ext, 0x0100);
}

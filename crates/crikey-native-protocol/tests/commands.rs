use crikey_core::command::{PluginCommands, MAX_COMMAND_BYTES};
use crikey_native_protocol::{
    commands::{from_response, to_response},
    message::{CommandDeclaration, CommandsResponse, Envelope, Payload},
    wire::{encode_key, encode_varint, WireType},
    Message,
};

fn declaration() -> CommandDeclaration {
    CommandDeclaration {
        command_id: "command".into(),
        label: "Command".into(),
        item_id: "item".into(),
        action_id: "open".into(),
        argument_policy: 1,
        ..Default::default()
    }
}
fn response(command: CommandDeclaration) -> CommandsResponse {
    CommandsResponse {
        status: 1,
        commands: vec![command],
        ..Default::default()
    }
}
fn nested(tag: u32, bytes: &[u8], out: &mut Vec<u8>) {
    encode_key(tag, WireType::Length, out);
    encode_varint(bytes.len() as u64, out);
    out.extend_from_slice(bytes);
}
#[test]
fn command_wire_roundtrips_unknown_fields_without_acquiring_authority() {
    let mut declaration = declaration();
    // Future fields are retained but cannot become owner/Host/availability.
    declaration.unknown.push_raw(&[0x98, 0x06, 0x01]);
    let mut response = response(declaration);
    response.unknown.push_raw(&[0xa0, 0x06, 0x01]);
    let envelope = Envelope {
        connection_id: 9,
        request_id: 10,
        generation: 11,
        payload: Some(Payload::CommandsResponse(response)),
        ..Default::default()
    };
    let encoded = envelope.encode();
    let decoded = Envelope::decode(&encoded).unwrap();
    assert_eq!(decoded.encode(), encoded);
    let Some(Payload::CommandsResponse(response)) = decoded.payload else {
        panic!("wrong response")
    };
    let PluginCommands::Declarations(commands) = from_response(response).unwrap() else {
        panic!("no declarations")
    };
    assert_eq!(commands[0].item_id.0, "item");
    assert_eq!(commands[0].action_id.as_ref().unwrap().0, "open");
    assert_eq!(
        to_response(PluginCommands::Declarations(commands))
            .unwrap()
            .status,
        1
    );
}
#[test]
fn malformed_and_oversized_plugin_commands_are_refused_whole() {
    for command in [
        CommandDeclaration {
            command_id: String::new(),
            ..declaration()
        },
        CommandDeclaration {
            item_id: "\n".into(),
            ..declaration()
        },
        CommandDeclaration {
            action_id: "\0".into(),
            ..declaration()
        },
        CommandDeclaration {
            argument_policy: 0,
            ..declaration()
        },
        CommandDeclaration {
            label: " ".into(),
            ..declaration()
        },
    ] {
        assert!(from_response(response(command)).is_err());
    }
    for command in [
        CommandDeclaration {
            command_id: "x".repeat(257),
            ..declaration()
        },
        CommandDeclaration {
            item_id: "x".repeat(257),
            ..declaration()
        },
        CommandDeclaration {
            label: "x".repeat(8193),
            ..declaration()
        },
    ] {
        assert!(CommandsResponse::decode(&response(command).encode()).is_err());
    }
    let commands = (0..600)
        .map(|index| CommandDeclaration {
            command_id: index.to_string(),
            label: "x".repeat(8192),
            ..declaration()
        })
        .collect();
    let aggregate = CommandsResponse {
        status: 1,
        commands,
        ..Default::default()
    };
    assert!(
        aggregate
            .commands
            .iter()
            .map(|command| command.label.len())
            .sum::<usize>()
            > MAX_COMMAND_BYTES
    );
    assert!(from_response(aggregate).is_err());
    assert!(from_response(CommandsResponse {
        status: 2,
        commands: vec![declaration()],
        ..Default::default()
    })
    .is_err());
    assert!(from_response(CommandsResponse {
        status: 1,
        commands: vec![declaration(), declaration()],
        ..Default::default()
    })
    .is_err());
    assert_eq!(
        from_response(CommandsResponse {
            status: 2,
            ..Default::default()
        })
        .unwrap(),
        PluginCommands::Unsupported
    );
}
#[test]
fn fragmented_response_merges_and_cannot_bypass_count_limit() {
    let first = response(declaration()).encode();
    let mut second = response(CommandDeclaration {
        command_id: "other".into(),
        ..declaration()
    });
    second.status = 0;
    let mut encoded = Vec::new();
    nested(46, &first, &mut encoded);
    nested(46, &second.encode(), &mut encoded);
    let Some(Payload::CommandsResponse(merged)) = Envelope::decode(&encoded).unwrap().payload else {
        panic!("wrong response")
    };
    assert_eq!(merged.commands.len(), 2);
    assert_eq!(merged.status, 1);
    let mut excessive = Vec::new();
    for _ in 0..10_001 {
        nested(2, &[], &mut excessive);
    }
    assert!(CommandsResponse::decode(&excessive).is_err());
}

//! Untrusted executable declarations: no owner, availability or host target on wire.
use crate::message;
use crikey_core::command::{validate_plugin_commands, PluginCommandDeclaration, PluginCommands};
use crikey_core::{ActionId, ArgumentPolicy, ItemId};

pub fn from_response(response: message::CommandsResponse) -> Result<PluginCommands, String> {
    if response.detail.len() > 8192 {
        return Err("command diagnostic exceeds bounds".into());
    }
    if response.status != 1 {
        if !response.commands.is_empty() {
            return Err("non-success command response contains declarations".into());
        }
        return match response.status {
            2 => Ok(PluginCommands::Unsupported),
            3 => Ok(PluginCommands::Cancelled),
            4 => Err(response.detail),
            _ => Err("invalid command response status".into()),
        };
    }
    if response.commands.len() > crikey_core::command::MAX_COMMANDS {
        return Err("too many commands".into());
    }
    let mut commands = Vec::with_capacity(response.commands.len());
    for command in response.commands {
        commands.push(PluginCommandDeclaration {
            command_id: command.command_id,
            label: command.label,
            item_id: ItemId(command.item_id),
            action_id: (!command.action_id.is_empty()).then_some(ActionId(command.action_id)),
            argument_policy: match command.argument_policy {
                1 => ArgumentPolicy::Forbidden,
                2 => ArgumentPolicy::Optional,
                3 => ArgumentPolicy::Required,
                _ => return Err("invalid command argument policy".into()),
            },
        });
    }
    validate_plugin_commands(&commands).map_err(str::to_owned)?;
    Ok(PluginCommands::Declarations(commands))
}

pub fn to_response(outcome: PluginCommands) -> Result<message::CommandsResponse, String> {
    let (status, declarations) = match outcome {
        PluginCommands::Unsupported => (2, Vec::new()),
        PluginCommands::Cancelled => (3, Vec::new()),
        PluginCommands::Declarations(commands) => {
            validate_plugin_commands(&commands).map_err(str::to_owned)?;
            (1, commands)
        }
    };
    let commands = declarations
        .into_iter()
        .map(|command| message::CommandDeclaration {
            command_id: command.command_id,
            label: command.label,
            item_id: command.item_id.0,
            action_id: command.action_id.map_or_else(String::new, |id| id.0),
            argument_policy: match command.argument_policy {
                ArgumentPolicy::Forbidden => 1,
                ArgumentPolicy::Optional => 2,
                ArgumentPolicy::Required => 3,
            },
            ..Default::default()
        })
        .collect();
    Ok(message::CommandsResponse {
        status,
        commands,
        ..Default::default()
    })
}

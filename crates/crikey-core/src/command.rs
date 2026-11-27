//! Explicit executable declarations; item identity and metadata imply no durability.
use crate::{ActionId, ArgumentPolicy, ItemId, PluginId};

pub const MAX_COMMANDS: usize = 10_000;
pub const MAX_COMMAND_ID_BYTES: usize = 256;
pub const MAX_COMMAND_LABEL_BYTES: usize = 8 * 1024;
pub const MAX_COMMAND_BYTES: usize = 4 * 1024 * 1024;
pub const COMMANDS_CAPABILITY: &str = "commands_v1";

/// Untrusted plugin declaration. Identity, availability and host targets are
/// deliberately absent; the host binds the installed owner and current catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginCommandDeclaration {
    pub command_id: String,
    pub label: String,
    pub item_id: ItemId,
    pub action_id: Option<ActionId>,
    pub argument_policy: ArgumentPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginCommands {
    Unsupported,
    Declarations(Vec<PluginCommandDeclaration>),
    Cancelled,
}

impl PluginCommandDeclaration {
    pub fn validate(&self) -> Result<(), &'static str> {
        for id in [
            Some(self.command_id.as_str()),
            Some(self.item_id.0.as_str()),
            self.action_id.as_ref().map(|id| id.0.as_str()),
        ]
        .into_iter()
        .flatten()
        {
            if id.is_empty() || id.len() > MAX_COMMAND_ID_BYTES || id.chars().any(char::is_control) {
                return Err("invalid plugin command target or identity");
            }
        }
        if self.label.trim().is_empty() || self.label.len() > MAX_COMMAND_LABEL_BYTES {
            return Err("invalid command label");
        }
        Ok(())
    }

    /// Only call after validating this target against the current owner-qualified
    /// catalog. This conversion supplies no authorization or liveness guarantee.
    pub fn bind(self, owner: PluginId, availability: CommandAvailability) -> CommandDeclaration {
        CommandDeclaration {
            reference: CommandRef {
                provider_id: owner,
                command_id: self.command_id,
            },
            label: self.label,
            executable_target: ExecutableTarget::Item {
                item_id: self.item_id,
                action_id: self.action_id,
            },
            availability,
            argument_policy: self.argument_policy,
        }
    }
}

pub fn validate_plugin_commands(commands: &[PluginCommandDeclaration]) -> Result<(), &'static str> {
    if commands.len() > MAX_COMMANDS {
        return Err("too many commands");
    }
    let mut bytes = 0usize;
    let mut identities = std::collections::BTreeSet::new();
    for command in commands {
        command.validate()?;
        if !identities.insert(command.command_id.as_str()) {
            return Err("duplicate command identity");
        }
        for size in [
            command.command_id.len(),
            command.label.len(),
            command.item_id.0.len(),
            command.action_id.as_ref().map_or(0, |id| id.0.len()),
        ] {
            bytes = bytes.checked_add(size).ok_or("command payload overflow")?;
        }
        if bytes > MAX_COMMAND_BYTES {
            return Err("command payload exceeds bounds");
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CommandRef {
    pub provider_id: PluginId,
    pub command_id: String,
}
impl CommandRef {
    pub fn validate(&self) -> Result<(), &'static str> {
        for id in [&self.provider_id.0, &self.command_id] {
            if id.is_empty() || id.len() > MAX_COMMAND_ID_BYTES || id.chars().any(char::is_control) {
                return Err("invalid command identity");
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostCommand {
    OpenSettings,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutableTarget {
    Host(HostCommand),
    /// Resolving this target still requires current item/action and permission checks.
    Item {
        item_id: ItemId,
        action_id: Option<ActionId>,
    },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandAvailability {
    Available,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandDeclaration {
    pub reference: CommandRef,
    pub label: String,
    pub executable_target: ExecutableTarget,
    pub availability: CommandAvailability,
    pub argument_policy: ArgumentPolicy,
}
impl CommandDeclaration {
    pub fn validate(&self) -> Result<(), &'static str> {
        self.reference.validate()?;
        if self.label.trim().is_empty() || self.label.len() > MAX_COMMAND_LABEL_BYTES {
            return Err("invalid command label");
        }
        if let ExecutableTarget::Item { item_id, action_id } = &self.executable_target {
            if item_id.0.is_empty()
                || item_id.0.len() > MAX_COMMAND_ID_BYTES
                || item_id.0.chars().any(char::is_control)
            {
                return Err("invalid executable item");
            }
            if action_id.as_ref().is_some_and(|id| {
                id.0.is_empty() || id.0.len() > MAX_COMMAND_ID_BYTES || id.0.chars().any(char::is_control)
            }) {
                return Err("invalid executable action");
            }
        }
        Ok(())
    }
}

/// Only commands with an existing host dispatch path are declared here.
pub fn builtin_commands() -> Vec<CommandDeclaration> {
    vec![CommandDeclaration {
        reference: CommandRef {
            provider_id: PluginId("crikey.host".into()),
            command_id: "open-settings".into(),
        },
        label: "Open Settings".into(),
        executable_target: ExecutableTarget::Host(HostCommand::OpenSettings),
        availability: CommandAvailability::Available,
        argument_policy: ArgumentPolicy::Forbidden,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plugin_command() -> PluginCommandDeclaration {
        PluginCommandDeclaration {
            command_id: "open".into(),
            label: "Open".into(),
            item_id: ItemId("item".into()),
            action_id: Some(ActionId("action".into())),
            argument_policy: ArgumentPolicy::Forbidden,
        }
    }

    #[test]
    fn plugin_commands_are_bounded_and_bind_only_host_identity() {
        let command = plugin_command();
        validate_plugin_commands(std::slice::from_ref(&command)).unwrap();
        assert!(validate_plugin_commands(&[command.clone(), command.clone()]).is_err());
        let bound = command.bind(
            PluginId("installed.owner".into()),
            CommandAvailability::Unavailable,
        );
        assert_eq!(bound.reference.provider_id.0, "installed.owner");
        assert_eq!(bound.availability, CommandAvailability::Unavailable);
        assert!(matches!(bound.executable_target, ExecutableTarget::Item { .. }));
        for bad in [
            PluginCommandDeclaration {
                command_id: "x".repeat(MAX_COMMAND_ID_BYTES + 1),
                ..plugin_command()
            },
            PluginCommandDeclaration {
                action_id: Some(ActionId("\n".into())),
                ..plugin_command()
            },
            PluginCommandDeclaration {
                label: "x".repeat(MAX_COMMAND_LABEL_BYTES + 1),
                ..plugin_command()
            },
        ] {
            assert!(bad.validate().is_err());
        }
        assert!(validate_plugin_commands(&vec![plugin_command(); MAX_COMMANDS + 1]).is_err());
    }
}

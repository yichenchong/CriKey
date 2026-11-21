//! Host-owned executable registry with checked integration publication epochs.
use crikey_core::{
    command::{
        builtin_commands, CommandAvailability, CommandDeclaration, CommandRef, ExecutableTarget, MAX_COMMANDS,
    },
    PluginId,
};
use std::{collections::BTreeMap, sync::Arc};

#[derive(Debug, Clone)]
pub struct RegisteredCommand {
    pub declaration: CommandDeclaration,
    pub incarnation: u64,
}
#[derive(Debug, Clone, Default)]
pub struct CommandRegistrySnapshot {
    pub revision: u64,
    pub commands: BTreeMap<CommandRef, RegisteredCommand>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryError {
    Invalid(String),
    OwnerMismatch,
    StaleIncarnation,
    Limit,
    RevisionExhausted,
}
#[derive(Debug, Default)]
pub struct CommandRegistry {
    snapshot: Arc<CommandRegistrySnapshot>,
    owners: BTreeMap<PluginId, (u64, bool)>,
}
impl CommandRegistry {
    pub fn with_builtins() -> Self {
        let mut registry = Self::default();
        registry
            .replace_owner(&PluginId("crikey.host".into()), 1, builtin_commands())
            .expect("valid built-in declarations");
        registry
    }
    pub fn snapshot(&self) -> Arc<CommandRegistrySnapshot> {
        Arc::clone(&self.snapshot)
    }
    pub fn changes_since(&self, revision: u64) -> Option<Arc<CommandRegistrySnapshot>> {
        (revision != self.snapshot.revision).then(|| self.snapshot())
    }
    /// Allocates a host publication epoch independent of worker incarnation.
    pub fn publish_owner(
        &mut self,
        owner: &PluginId,
        declarations: Vec<CommandDeclaration>,
    ) -> Result<u64, RegistryError> {
        let epoch = self
            .owners
            .get(owner)
            .map_or(Some(1), |(epoch, _)| epoch.checked_add(1))
            .ok_or(RegistryError::RevisionExhausted)?;
        self.replace_owner(owner, epoch, declarations)
    }
    /// Retains known labels/targets for saved-reference management, never dispatch.
    pub fn revoke_owner(&mut self, owner: &PluginId) -> Result<u64, RegistryError> {
        let mut declarations: Vec<_> = self
            .snapshot
            .commands
            .values()
            .filter(|command| &command.declaration.reference.provider_id == owner)
            .map(|command| command.declaration.clone())
            .collect();
        if declarations
            .iter()
            .all(|declaration| declaration.availability == CommandAvailability::Unavailable)
        {
            return Ok(self.snapshot.revision);
        }
        for declaration in &mut declarations {
            declaration.availability = CommandAvailability::Unavailable;
        }
        self.publish_owner(owner, declarations)
    }
    pub fn replace_owner(
        &mut self,
        owner: &PluginId,
        incarnation: u64,
        declarations: Vec<CommandDeclaration>,
    ) -> Result<u64, RegistryError> {
        if incarnation == 0
            || self.owners.get(owner).is_some_and(|(current, active)| {
                incarnation < *current || (incarnation == *current && !active)
            })
        {
            return Err(RegistryError::StaleIncarnation);
        }
        if declarations.len() > MAX_COMMANDS {
            return Err(RegistryError::Limit);
        }
        let mut incoming = BTreeMap::new();
        for declaration in declarations {
            declaration
                .validate()
                .map_err(|error| RegistryError::Invalid(error.into()))?;
            if &declaration.reference.provider_id != owner {
                return Err(RegistryError::OwnerMismatch);
            }
            if matches!(declaration.executable_target, ExecutableTarget::Host(_)) && owner.0 != "crikey.host"
            {
                return Err(RegistryError::OwnerMismatch);
            }
            if incoming
                .insert(
                    declaration.reference.clone(),
                    RegisteredCommand {
                        declaration,
                        incarnation,
                    },
                )
                .is_some()
            {
                return Err(RegistryError::Invalid("duplicate declaration".into()));
            }
        }
        let retained = self
            .snapshot
            .commands
            .keys()
            .filter(|reference| &reference.provider_id != owner)
            .count();
        if retained + incoming.len() > MAX_COMMANDS
            || (!self.owners.contains_key(owner) && self.owners.len() >= MAX_COMMANDS)
        {
            return Err(RegistryError::Limit);
        }
        let revision = self
            .snapshot
            .revision
            .checked_add(1)
            .ok_or(RegistryError::RevisionExhausted)?;
        let mut commands = self.snapshot.commands.clone();
        commands.retain(|reference, _| &reference.provider_id != owner);
        commands.extend(incoming);
        self.owners.insert(owner.clone(), (incarnation, true));
        self.snapshot = Arc::new(CommandRegistrySnapshot { revision, commands });
        Ok(revision)
    }
    pub fn remove_owner(&mut self, owner: &PluginId, incarnation: u64) -> Result<u64, RegistryError> {
        if self.owners.get(owner) != Some(&(incarnation, true)) {
            return Err(RegistryError::StaleIncarnation);
        }
        let revision = self.replace_owner(owner, incarnation, Vec::new())?;
        self.owners.insert(owner.clone(), (incarnation, false));
        Ok(revision)
    }
    /// Invoke only after resolving again and validating the current dispatch target/permissions.
    pub fn resolve(&self, reference: &CommandRef) -> Option<&RegisteredCommand> {
        self.snapshot
            .commands
            .get(reference)
            .filter(|command| command.declaration.availability == CommandAvailability::Available)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn owner_replacement_rejects_spoofing_and_retired_workers() {
        let mut registry = CommandRegistry::with_builtins();
        let declaration = builtin_commands().remove(0);
        let before = registry.snapshot().revision;
        assert_eq!(
            registry.replace_owner(&PluginId("other".into()), 1, vec![declaration.clone()]),
            Err(RegistryError::OwnerMismatch)
        );
        assert_eq!(registry.snapshot().revision, before);
        registry
            .remove_owner(&declaration.reference.provider_id, 1)
            .unwrap();
        assert!(registry.resolve(&declaration.reference).is_none());
        assert_eq!(
            registry.replace_owner(&declaration.reference.provider_id, 1, vec![declaration.clone()]),
            Err(RegistryError::StaleIncarnation)
        );
        registry
            .replace_owner(&declaration.reference.provider_id, 2, vec![declaration.clone()])
            .unwrap();
        assert_eq!(registry.resolve(&declaration.reference).unwrap().incarnation, 2);
    }
}

#[cfg(test)]
mod integration_epoch_tests {
    use super::*;

    #[test]
    fn shared_targets_remain_distinct_and_revocation_retains_labels() {
        let mut registry = CommandRegistry::with_builtins();
        let owner = PluginId("crikey.host".into());
        let first = builtin_commands().remove(0);
        let mut second = first.clone();
        second.reference.command_id = "another-settings".into();
        registry
            .publish_owner(&owner, vec![first.clone(), second.clone()])
            .unwrap();
        assert_eq!(registry.snapshot().commands.len(), 2);
        assert_eq!(
            registry
                .resolve(&first.reference)
                .unwrap()
                .declaration
                .executable_target,
            registry
                .resolve(&second.reference)
                .unwrap()
                .declaration
                .executable_target
        );
        let admitted_epoch = registry.resolve(&first.reference).unwrap().incarnation;
        registry.revoke_owner(&owner).unwrap();
        assert!(registry.resolve(&first.reference).is_none());
        assert!(registry.resolve(&second.reference).is_none());
        assert_eq!(
            registry.snapshot().commands[&first.reference].declaration.label,
            first.label
        );
        let revision = registry.snapshot().revision;
        registry.revoke_owner(&owner).unwrap();
        assert_eq!(registry.snapshot().revision, revision);
        registry.publish_owner(&owner, vec![first.clone()]).unwrap();
        assert!(registry.resolve(&first.reference).unwrap().incarnation > admitted_epoch);
        assert!(!registry.snapshot().commands.contains_key(&second.reference));
    }

    #[test]
    fn host_epochs_advance_after_explicit_removal() {
        let mut registry = CommandRegistry::with_builtins();
        let declaration = builtin_commands().remove(0);
        let owner = declaration.reference.provider_id.clone();
        registry.remove_owner(&owner, 1).unwrap();
        registry.publish_owner(&owner, vec![declaration.clone()]).unwrap();
        assert_eq!(registry.resolve(&declaration.reference).unwrap().incarnation, 2);
    }
}

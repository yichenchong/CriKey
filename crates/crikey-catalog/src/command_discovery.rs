//! Catalog projection of explicit declarations, never a source of durable identities.
use crate::CatalogStore;
use crikey_core::{
    command::{CommandAvailability, CommandDeclaration, CommandRef, ExecutableTarget},
    ExecutionPolicy, Item,
};

/// Validates the current owner-qualified executable target against catalog contents.
/// Dynamic commands without a catalog item must instead use their provider's live dispatch validator.
pub fn declared_catalog_item<'a>(
    catalog: &'a dyn CatalogStore,
    declaration: &CommandDeclaration,
) -> Option<&'a Item> {
    if declaration.availability != CommandAvailability::Available || declaration.validate().is_err() {
        return None;
    }
    let ExecutableTarget::Item { item_id, .. } = &declaration.executable_target else {
        return None;
    };
    let item = catalog.get(&declaration.reference.provider_id, item_id)?;
    if !valid_declared_item(item, declaration) {
        return None;
    }
    Some(item)
}

/// Canonical command validation shared by catalog discovery and admission.
/// Omitted actions select the actual primary action; only genuinely actionless
/// items may dispatch their default callback.
pub fn valid_declared_item(item: &Item, declaration: &CommandDeclaration) -> bool {
    let ExecutableTarget::Item { item_id, action_id } = &declaration.executable_target else {
        return false;
    };
    if declaration.validate().is_err()
        || declaration.availability != CommandAvailability::Available
        || item.plugin_id != declaration.reference.provider_id
        || &item.stable_id != item_id
        || item.argument_policy != declaration.argument_policy
    {
        return false;
    }
    let selected = action_id
        .as_ref()
        .or_else(|| item.actions.first().map(|action| &action.action_id));
    let Some(selected) = selected else {
        return true;
    };
    let mut actions = item.actions.iter().filter(|action| &action.action_id == selected);
    let Some(action) = actions.next() else {
        return false;
    };
    actions.next().is_none()
        && action.execution_policy == ExecutionPolicy::Plugin
        && (action.applicable_categories.is_empty() || action.applicable_categories.contains(&item.category))
}

/// Hidden affects discovery only. Explicit command invocation must not consult this filter.
pub fn discover_declared<'a>(
    catalog: &'a dyn CatalogStore,
    declarations: &'a [CommandDeclaration],
    hidden: impl Fn(&CommandRef) -> bool,
) -> impl Iterator<Item = (&'a CommandRef, &'a Item)> {
    declarations.iter().filter_map(move |declaration| {
        if hidden(&declaration.reference) {
            return None;
        }
        Some((
            &declaration.reference,
            declared_catalog_item(catalog, declaration)?,
        ))
    })
}

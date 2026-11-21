//! Bounded root result sections, scope declarations and exact memberships.
//! Endpoint admission supplies owner and current executable authority separately.
use crate::content_accounting::{account_owned, ContentFootprint};
use crate::structured_arguments::{
    fail, id, owned, text, ArgumentSchema, Check, ExecutableKey, OwnedItem, ValidationCode,
};
use crate::{preview, PluginId};
use std::{collections::BTreeSet, mem::size_of, sync::Arc};

pub const RESULT_PRESENTATION_CAPABILITY: &str = "result_presentation_v1";
pub const MAX_PRESENTATION_ITEMS: usize = 4096;
pub const MAX_SECTIONS: usize = 128;
pub const MAX_SCOPES: usize = 128;
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SectionId(pub String);
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ScopeId(pub String);
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub id: SectionId,
    pub label: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub id: ScopeId,
    pub label: String,
}
#[derive(Debug, Clone)]
pub struct ItemPresentation {
    pub target: OwnedItem,
    pub section: Option<SectionId>,
    pub scopes: Vec<ScopeId>,
    /// Shared validated pixels; never renderer-side network requests.
    pub image: Option<Arc<preview::PreviewImage>>,
}

/// One schema per exact executable; declarations do not duplicate schemas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutableSchemaBinding {
    pub executable: ExecutableKey,
    pub schema: Arc<ArgumentSchema>,
}
#[derive(Debug, Clone, Default)]
pub struct ResultPresentation {
    pub sections: Vec<Section>,
    pub scopes: Vec<Scope>,
    pub items: Vec<ItemPresentation>,
    pub schemas: Vec<ExecutableSchemaBinding>,
}
impl ResultPresentation {
    /// Validate as a transaction against the complete accepted publication.
    /// No authority is granted by a partially validated sidecar.
    pub fn validate(
        &self,
        owner: &PluginId,
        items: &BTreeSet<OwnedItem>,
        executables: &BTreeSet<ExecutableKey>,
    ) -> Check {
        id(&owner.0)?;
        validate_memberships(&self.sections, &self.scopes, &self.items, |target| {
            target.owner == *owner && items.contains(target)
        })?;
        if self.schemas.len() > MAX_PRESENTATION_ITEMS {
            return Err(fail(ValidationCode::TooMany));
        }
        let mut seen = BTreeSet::new();
        for binding in &self.schemas {
            binding.executable.validate()?;
            if binding.executable.target.owner != *owner {
                return Err(fail(ValidationCode::WrongOwner));
            }
            if !items.contains(&binding.executable.target) || !executables.contains(&binding.executable) {
                return Err(fail(ValidationCode::InvalidReference));
            }
            if !seen.insert(&binding.executable) {
                return Err(fail(ValidationCode::DuplicateId));
            }
            binding.schema.validate()?;
        }
        Ok(())
    }
    pub fn schema_for(&self, executable: &ExecutableKey) -> Option<&Arc<ArgumentSchema>> {
        self.schemas
            .iter()
            .find(|b| &b.executable == executable)
            .map(|b| &b.schema)
    }
    /// Charge into the same accumulator as root/navigation and renderer copies.
    pub fn account<'a>(&'a self, budget: &mut ContentFootprint<'a>) -> Check {
        budget.bytes(size_of::<Self>())?;
        account_declarations(&self.sections, &self.scopes, budget)?;
        budget.array::<ItemPresentation>(self.items.capacity())?;
        for item in &self.items {
            account_item(item, budget)?;
        }
        budget.array::<ExecutableSchemaBinding>(self.schemas.capacity())?;
        for binding in &self.schemas {
            account_owned(&binding.executable.target, budget)?;
            if let Some(action) = &binding.executable.action {
                budget.string(&action.0)?;
            }
            budget.include_schema(&binding.schema)?;
        }
        Ok(())
    }
}
pub(crate) fn validate_memberships(
    sections: &[Section],
    scopes: &[Scope],
    items: &[ItemPresentation],
    available: impl Fn(&OwnedItem) -> bool,
) -> Check {
    if items.len() > MAX_PRESENTATION_ITEMS || sections.len() > MAX_SECTIONS || scopes.len() > MAX_SCOPES {
        return Err(fail(ValidationCode::TooMany));
    }
    let mut declared_sections = BTreeSet::new();
    for section in sections {
        id(&section.id.0)?;
        text(&section.label)?;
        if !declared_sections.insert(&section.id) {
            return Err(fail(ValidationCode::DuplicateId));
        }
    }
    let mut declared_scopes = BTreeSet::new();
    for scope in scopes {
        id(&scope.id.0)?;
        text(&scope.label)?;
        if !declared_scopes.insert(&scope.id) {
            return Err(fail(ValidationCode::DuplicateId));
        }
    }
    let mut seen = BTreeSet::new();
    for item in items {
        owned(&item.target)?;
        if !available(&item.target) {
            return Err(fail(ValidationCode::InvalidReference));
        }
        if !seen.insert(&item.target) {
            return Err(fail(ValidationCode::DuplicateId));
        }
        if item
            .section
            .as_ref()
            .is_some_and(|s| !declared_sections.contains(s))
        {
            return Err(fail(ValidationCode::InvalidReference));
        }
        if item.scopes.len() > MAX_SCOPES {
            return Err(fail(ValidationCode::TooMany));
        }
        let mut assigned = BTreeSet::new();
        for scope in &item.scopes {
            if !declared_scopes.contains(scope) {
                return Err(fail(ValidationCode::InvalidReference));
            }
            if !assigned.insert(scope) {
                return Err(fail(ValidationCode::DuplicateId));
            }
        }
        if let Some(image) = &item.image {
            image
                .validate()
                .map_err(|_| fail(ValidationCode::InvalidPreview))?;
        }
    }
    Ok(())
}
pub(crate) fn account_declarations(
    sections: &Vec<Section>,
    scopes: &Vec<Scope>,
    budget: &mut ContentFootprint<'_>,
) -> Check {
    budget.array::<Section>(sections.capacity())?;
    for section in sections {
        budget.string(&section.id.0)?;
        budget.string(&section.label)?;
    }
    budget.array::<Scope>(scopes.capacity())?;
    for scope in scopes {
        budget.string(&scope.id.0)?;
        budget.string(&scope.label)?;
    }
    Ok(())
}
pub(crate) fn account_item<'a>(item: &'a ItemPresentation, budget: &mut ContentFootprint<'a>) -> Check {
    account_owned(&item.target, budget)?;
    if let Some(section) = &item.section {
        budget.string(&section.0)?;
    }
    budget.array::<ScopeId>(item.scopes.capacity())?;
    for scope in &item.scopes {
        budget.string(&scope.0)?;
    }
    if let Some(image) = &item.image {
        if budget.begin_shared(image, size_of::<preview::PreviewImage>())? {
            budget.charge(image.rgba.capacity(), image.rgba.capacity())?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "result_presentation_tests.rs"]
mod tests;

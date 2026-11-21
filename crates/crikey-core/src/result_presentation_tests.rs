use super::*;
use crate::structured_arguments::{MAX_ID_BYTES, MAX_TEXT_BYTES};
use crate::{ActionId, ItemId};
fn target(owner: &str) -> OwnedItem {
    OwnedItem {
        owner: PluginId(owner.into()),
        item: ItemId("item".into()),
    }
}
fn key(owner: &str, action: Option<&str>) -> ExecutableKey {
    ExecutableKey {
        target: target(owner),
        action: action.map(|a| ActionId(a.into())),
    }
}
fn presentation() -> ResultPresentation {
    ResultPresentation {
        sections: vec![Section {
            id: SectionId("section".into()),
            label: "Section".into(),
        }],
        scopes: vec![Scope {
            id: ScopeId("scope".into()),
            label: "Scope".into(),
        }],
        items: vec![ItemPresentation {
            target: target("owner"),
            section: Some(SectionId("section".into())),
            scopes: vec![ScopeId("scope".into())],
            image: None,
        }],
        schemas: vec![ExecutableSchemaBinding {
            executable: key("owner", None),
            schema: Arc::new(ArgumentSchema::default()),
        }],
    }
}
fn validate(p: &ResultPresentation) -> Check {
    p.validate(
        &PluginId("owner".into()),
        &BTreeSet::from([target("owner")]),
        &BTreeSet::from([key("owner", None), key("owner", Some("run"))]),
    )
}
#[test]
fn exact_actionless_owner_item_binding_cannot_alias_named_action() {
    let p = presentation();
    validate(&p).unwrap();
    assert!(p.schema_for(&key("owner", None)).is_some());
    assert!(p.schema_for(&key("owner", Some("run"))).is_none());
    assert!(p.schema_for(&key("other", None)).is_none());
    let mut foreign = p.clone();
    foreign.schemas[0].executable = key("other", None);
    assert_eq!(validate(&foreign).unwrap_err().code, ValidationCode::WrongOwner);
    let mut missing = p.clone();
    missing.schemas[0].executable = key("owner", Some("missing"));
    assert_eq!(
        validate(&missing).unwrap_err().code,
        ValidationCode::InvalidReference
    );
    let mut duplicate = p.clone();
    duplicate.schemas.push(duplicate.schemas[0].clone());
    assert_eq!(
        validate(&duplicate).unwrap_err().code,
        ValidationCode::DuplicateId
    );
}
#[test]
fn membership_references_duplicates_and_declarations_are_transactional() {
    let mut p = presentation();
    p.items[0].scopes.push(ScopeId("missing".into()));
    assert_eq!(validate(&p).unwrap_err().code, ValidationCode::InvalidReference);
    let mut p = presentation();
    p.items[0].scopes.push(ScopeId("scope".into()));
    assert_eq!(validate(&p).unwrap_err().code, ValidationCode::DuplicateId);
    let mut p = presentation();
    p.items[0].section = Some(SectionId("missing".into()));
    assert_eq!(validate(&p).unwrap_err().code, ValidationCode::InvalidReference);
    let mut p = presentation();
    p.items.push(p.items[0].clone());
    assert_eq!(validate(&p).unwrap_err().code, ValidationCode::DuplicateId);
    let mut p = presentation();
    p.sections.push(p.sections[0].clone());
    assert_eq!(validate(&p).unwrap_err().code, ValidationCode::DuplicateId);
    let mut p = presentation();
    p.scopes.push(p.scopes[0].clone());
    assert_eq!(validate(&p).unwrap_err().code, ValidationCode::DuplicateId);
}
#[test]
fn bounded_counts_ids_and_labels_refuse_without_truncation() {
    let mut p = presentation();
    p.items = vec![p.items[0].clone(); MAX_PRESENTATION_ITEMS + 1];
    assert_eq!(validate(&p).unwrap_err().code, ValidationCode::TooMany);
    let mut p = presentation();
    p.sections = vec![p.sections[0].clone(); MAX_SECTIONS + 1];
    assert_eq!(validate(&p).unwrap_err().code, ValidationCode::TooMany);
    let mut p = presentation();
    p.scopes = vec![p.scopes[0].clone(); MAX_SCOPES + 1];
    assert_eq!(validate(&p).unwrap_err().code, ValidationCode::TooMany);
    let mut p = presentation();
    p.items[0].scopes = vec![ScopeId("scope".into()); MAX_SCOPES + 1];
    assert_eq!(validate(&p).unwrap_err().code, ValidationCode::TooMany);
    let mut p = presentation();
    p.sections[0].id.0 = "x".repeat(MAX_ID_BYTES + 1);
    assert_eq!(validate(&p).unwrap_err().code, ValidationCode::TooLong);
    let mut p = presentation();
    p.scopes[0].label = "x".repeat(MAX_TEXT_BYTES + 1);
    assert_eq!(validate(&p).unwrap_err().code, ValidationCode::TooLong);
}
#[test]
fn shared_schema_accounting_uses_caller_limits_and_no_new_allowance() {
    let p = presentation();
    let mut second = p.clone();
    second.schemas[0].executable.action = Some(ActionId("run".into()));
    let mut budget = ContentFootprint::new(64 * 1024, 0);
    budget.charge(123, 0).unwrap();
    p.account(&mut budget).unwrap();
    second.account(&mut budget).unwrap();
    let used = budget.resources().bytes;
    budget.include_schema(&p.schemas[0].schema).unwrap();
    assert_eq!(budget.resources().bytes, used);
    let mut budget = ContentFootprint::new(123, 0);
    budget.charge(123, 0).unwrap();
    assert_eq!(
        p.account(&mut budget).unwrap_err().code,
        ValidationCode::RetainedBytes
    );
}

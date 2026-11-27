use super::*;
use crate::{LauncherViewModel, NavigationRestoration, PublicationIdentity, UiCommand};
use crikey_core::structured_arguments::{
    ArgumentField, DropdownOption, ExecutableKey, FieldId, OptionId, OwnedItem, SubmissionFailureCode,
    SubmissionFieldCode, SubmissionFieldFailure, ValidationCode, MAX_TEXT_BYTES,
};
use crikey_core::{Generation, ItemId, PluginId};

fn password_schema() -> Arc<ArgumentSchema> {
    Arc::new(ArgumentSchema {
        fields: vec![ArgumentField {
            id: FieldId("password".into()),
            label: "Password".into(),
            required: true,
            placeholder: None,
            validation_message: Some("Use three to eight bytes".into()),
            kind: FieldKind::Password {
                min_bytes: 3,
                max_bytes: 8,
            },
        }],
    })
}
fn model(schema: Arc<ArgumentSchema>) -> LauncherViewModel {
    let mut model = LauncherViewModel::new();
    model.activate();
    model.apply(UiCommand::SetQuery("typed command".into()));
    model.begin_generation(Generation::from_raw(1));
    model.open_inline_arguments(
        1,
        ExecutableKey {
            target: OwnedItem {
                owner: PluginId("typed.owner".into()),
                item: ItemId("item".into()),
            },
            action: None,
        },
        schema,
        PublicationIdentity::Catalog {
            instance: 1,
            generation: Generation::from_raw(1),
            producer: 1,
            publication: 1,
        },
        None,
    );
    model
}
fn paint(
    context: &egui::Context,
    editor: &mut TypedEditor,
    input: &InlineArguments,
    events: Vec<egui::Event>,
) -> (egui::FullOutput, Vec<UiCommand>, Option<TypedInputSubmission>) {
    let mut commands = Vec::new();
    let mut submission = None;
    let mut first = true;
    let output = context.run(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(720.0, 520.0),
            )),
            focused: true,
            events,
            ..Default::default()
        },
        |context| {
            egui::CentralPanel::default().show(context, |ui| {
                if let Some(value) = editor.draw(ui, Some(input), &mut commands, first) {
                    submission = Some(value);
                }
            });
            first = false;
        },
    );
    (output, commands, submission)
}
fn text_position(output: &egui::FullOutput, needle: &str) -> egui::Pos2 {
    fn find(shape: &egui::Shape, needle: &str) -> Option<egui::Pos2> {
        match shape {
            egui::Shape::Text(text) if text.galley.text() == needle => {
                Some(text.pos + text.galley.size() * 0.5)
            }
            egui::Shape::Vec(shapes) => shapes.iter().find_map(|shape| find(shape, needle)),
            _ => None,
        }
    }
    output
        .shapes
        .iter()
        .find_map(|shape| find(&shape.shape, needle))
        .expect("painted control")
}
fn contains_text(output: &egui::FullOutput, needle: &str) -> bool {
    fn contains(shape: &egui::Shape, needle: &str) -> bool {
        match shape {
            egui::Shape::Text(text) => text.galley.text().contains(needle),
            egui::Shape::Vec(shapes) => shapes.iter().any(|shape| contains(shape, needle)),
            _ => false,
        }
    }
    output.shapes.iter().any(|shape| contains(&shape.shape, needle))
}
fn click(position: egui::Pos2) -> Vec<egui::Event> {
    vec![
        egui::Event::PointerMoved(position),
        egui::Event::PointerButton {
            pos: position,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        },
        egui::Event::PointerButton {
            pos: position,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        },
    ]
}

#[test]
fn ordered_native_input_cannot_cancel_a_replacement_typed_context() {
    for replacement in 0..4 {
        let mut model = model(password_schema());
        let original = model.frame().unwrap();
        let source = Arc::new(crate::ActionInputCheckpoint::of(&original));
        let mut input = model.inline_arguments().unwrap().clone();
        let context = crate::create_launcher_context();
        let mut editor = TypedEditor::default();
        paint(&context, &mut editor, &input, Vec::new());
        paint(
            &context,
            &mut editor,
            &input,
            vec![egui::Event::Text("hunter2".into())],
        );
        assert!(!editor.values.0.is_empty());
        match replacement {
            0 => input.revision += 1,
            1 => {
                let mut schema = (*input.schema).clone();
                schema.fields[0].label = "Replacement password".into();
                input.schema = Arc::new(schema);
            }
            2 => input.executable.target.item = ItemId("replacement".into()),
            _ => {
                input.source = PublicationIdentity::Catalog {
                    instance: 1,
                    generation: Generation::from_raw(1),
                    producer: 1,
                    publication: 2,
                }
            }
        }
        assert!(model.open_inline_arguments(
            input.revision,
            input.executable,
            input.schema,
            input.source,
            input.command,
        ));
        editor.retire_unless(model.inline_arguments());
        assert!(editor.values.0.is_empty(), "replacement retires live passwords");
        assert!(model
            .apply(UiCommand::ApplyInputBatch {
                continuation: Some(41),
                source,
                commands: vec![UiCommand::Cancel],
            })
            .is_none());
        assert!(model.inline_arguments().is_some());
        let current = model.frame().unwrap();
        assert!(!current.input_receipt.as_ref().unwrap().accepted);

        // The refusal retires only stale intent; a deliberate current batch
        // still follows the existing reducer/effect path.
        let effect = model.apply(UiCommand::ApplyInputBatch {
            continuation: Some(42),
            source: Arc::new(crate::ActionInputCheckpoint::of(&current)),
            commands: vec![UiCommand::Cancel],
        });
        let Some(crate::UiEffect::InputBatch { commands, .. }) = effect else {
            panic!("current typed input must be admitted");
        };
        for command in commands {
            model.apply(command);
        }
        model.finish_input_batch(42);
        assert!(model.inline_arguments().is_none());
    }
}

#[test]
fn native_password_input_is_fixed_masked_move_only_and_excluded_from_drafts() {
    let model = model(password_schema());
    let input = model.inline_arguments().unwrap();
    let context = crate::create_launcher_context();
    let mut editor = TypedEditor::default();
    paint(&context, &mut editor, input, Vec::new());
    let (_, commands, _) = paint(
        &context,
        &mut editor,
        input,
        vec![egui::Event::Text("hunter2".into())],
    );
    assert!(commands
        .iter()
        .any(|command| matches!(command, UiCommand::UserActivity)));
    assert!(!format!("{commands:?}").contains("hunter2"));
    assert!(commands
        .iter()
        .any(|command| matches!(command, UiCommand::SetTypedDraft { draft, .. } if draft.is_empty())));
    let (output, _, _) = paint(&context, &mut editor, input, Vec::new());
    text_position(&output, "••••••••");
    assert!(!contains_text(&output, "hunter2"));
    let position = text_position(&output, "Run command");
    let (_, _, submission) = paint(&context, &mut editor, input, click(position));
    let submission = submission.expect("valid native input submits");
    assert!(!format!("{submission:?}").contains("hunter2"));
    assert!(
        matches!(submission.values.0.get(&FieldId("password".into())), Some(StructuredValue::Password(password)) if password.expose_plaintext() == "hunter2")
    );
    assert!(
        editor.values.0.is_empty(),
        "a password must be re-entered after submission"
    );
    editor.retire_unless(None);
    assert!(editor.values.0.is_empty());
}

#[test]
fn native_submit_enforces_required_bounds_and_dropdown_membership() {
    let schema = Arc::new(ArgumentSchema {
        fields: vec![
            ArgumentField {
                id: FieldId("text".into()),
                label: "Text".into(),
                required: true,
                placeholder: None,
                validation_message: None,
                kind: FieldKind::Text {
                    min_bytes: 3,
                    max_bytes: 8,
                    default: None,
                    multiline: false,
                },
            },
            ArgumentField {
                id: FieldId("choice".into()),
                label: "Choice".into(),
                required: true,
                placeholder: None,
                validation_message: None,
                kind: FieldKind::Dropdown {
                    options: vec![DropdownOption {
                        id: OptionId("one".into()),
                        label: "One".into(),
                    }],
                    default: None,
                },
            },
        ],
    });
    let model = model(schema);
    let input = model.inline_arguments().unwrap();
    let context = crate::create_launcher_context();
    let mut editor = TypedEditor::default();
    let (output, _, _) = paint(&context, &mut editor, input, Vec::new());
    let position = text_position(&output, "Run command");
    let (_, _, submission) = paint(&context, &mut editor, input, click(position));
    assert!(submission.is_none());
    assert!(editor
        .errors
        .iter()
        .any(|error| error.code == ValidationCode::Required));
    editor.values.0.insert(
        FieldId("text".into()),
        StructuredValue::NonSecret(NonSecretValue::Text("x".into())),
    );
    editor.values.0.insert(
        FieldId("choice".into()),
        StructuredValue::NonSecret(NonSecretValue::Dropdown(OptionId("absent".into()))),
    );
    let (output, _, _) = paint(&context, &mut editor, input, Vec::new());
    let position = text_position(&output, "Run command");
    let (_, _, submission) = paint(&context, &mut editor, input, click(position));
    assert!(submission.is_none());
    assert!(editor
        .errors
        .iter()
        .any(|error| error.code == ValidationCode::TooShort));
    assert!(editor
        .errors
        .iter()
        .any(|error| error.code == ValidationCode::InvalidSelection));
    editor.values.0.insert(
        FieldId("text".into()),
        StructuredValue::NonSecret(NonSecretValue::Text("123456789".into())),
    );
    let (output, _, _) = paint(&context, &mut editor, input, Vec::new());
    let position = text_position(&output, "Run command");
    let (_, _, submission) = paint(&context, &mut editor, input, click(position));
    assert!(submission.is_none());
    assert!(editor
        .errors
        .iter()
        .any(|error| error.code == ValidationCode::TooLong));
}

#[test]
fn p3_restores_only_shared_nonsecret_drafts_and_rejects_retired_submit_contexts() {
    let mut schema = (*password_schema()).clone();
    schema.fields.push(ArgumentField {
        id: FieldId("text".into()),
        label: "Text".into(),
        required: false,
        placeholder: None,
        validation_message: None,
        kind: FieldKind::Text {
            min_bytes: 0,
            max_bytes: 8,
            default: None,
            multiline: false,
        },
    });
    let mut model = model(Arc::new(schema));
    let original = model.inline_arguments().unwrap().clone();
    assert!(model.accepts_typed_input(original.revision, original.view, original.generation));
    model.apply(UiCommand::SetTypedDraft {
        revision: 1,
        draft_revision: 1,
        draft: [(
            FieldId("password".into()),
            NonSecretValue::Text("forbidden".into()),
        )]
        .into(),
    });
    assert!(model.inline_arguments().unwrap().draft.values.is_empty());
    model.apply(UiCommand::SetTypedDraft {
        revision: 1,
        draft_revision: 2,
        draft: [(FieldId("text".into()), NonSecretValue::Text("draft".into()))].into(),
    });
    let retained = model.inline_arguments().unwrap().draft.clone();
    model.open_settings(None);
    assert!(model.inline_arguments().is_none());
    model.close_settings();
    assert!(Arc::ptr_eq(&retained, &model.inline_arguments().unwrap().draft));
    assert!(!model.accepts_typed_input(original.revision, original.view, original.generation));
    model.begin_generation(Generation::from_raw(2));
    assert!(!model.accepts_typed_input(original.revision, original.view, original.generation));
    assert_eq!(model.inline_arguments().unwrap().draft.values.len(), 1);
    model.apply(UiCommand::Cancel);
    assert!(model.inline_arguments().is_none());
}

#[test]
fn scope_change_retires_old_work_and_unavailable_restoration_never_broadens() {
    let mut model = model(password_schema());
    let generation = Generation::from_raw(1);
    let scope = crate::ScopeIdentity {
        owner: PluginId("typed.owner".into()),
        id: crikey_core::result_presentation::ScopeId("private".into()),
        source: PublicationIdentity::Catalog {
            instance: 1,
            generation,
            producer: 1,
            publication: 1,
        },
    };
    model.set_root_presentation(
        generation,
        vec![crate::ScopeChoice {
            identity: scope.clone(),
            label: "Private".into(),
        }],
        Vec::new(),
        Vec::new(),
    );
    assert!(matches!(
        model.apply(UiCommand::SetScope(Some(scope.clone()))),
        Some(crate::UiEffect::Query(_))
    ));
    assert!(!model.admits_root_generation(generation));
    assert!(model.inline_arguments().is_none());
    model.restore_navigation_state(NavigationRestoration {
        query: "typed command".into(),
        scope: Some(scope.clone()),
        ..Default::default()
    });
    model.begin_generation(Generation::from_raw(2));
    model.set_root_presentation(Generation::from_raw(2), Vec::new(), Vec::new(), Vec::new());
    assert_eq!(model.selected_scope(), Some(&scope));
    let frame = model.frame().unwrap();
    assert_eq!(frame.navigation.selected_scope, Some(scope));
    assert!(frame.navigation.scopes.is_empty());
}

#[test]
fn mixed_p3_budget_charges_shared_drafts_once_and_native_editor_separately() {
    let schema = Arc::new(ArgumentSchema {
        fields: (0..64)
            .map(|index| ArgumentField {
                id: FieldId(format!("field-{index}")),
                label: "Text".into(),
                required: false,
                placeholder: None,
                validation_message: None,
                kind: FieldKind::Text {
                    min_bytes: 0,
                    max_bytes: MAX_TEXT_BYTES,
                    default: None,
                    multiline: false,
                },
            })
            .collect(),
    });
    let mut model = model(schema);
    model.apply(UiCommand::SetTypedDraft {
        revision: 1,
        draft_revision: 1,
        draft: (0..64)
            .map(|index| {
                (
                    FieldId(format!("field-{index}")),
                    NonSecretValue::Text("x".repeat(MAX_TEXT_BYTES)),
                )
            })
            .collect(),
    });
    let state = model.navigation_restoration();
    let shared: Vec<_> = (0..16)
        .map(|_| crate::navigation::Entry {
            route: crate::NavigationRoute::Root,
            rows: Arc::default(),
            state: state.clone(),
        })
        .collect();
    assert!(crate::navigation::retained_bytes(shared.iter()) < crate::navigation::MAX_NAVIGATION_BYTES);
    let unique: Vec<_> = shared
        .into_iter()
        .map(|mut entry| {
            let input = entry.state.inline_arguments.as_mut().unwrap();
            input.draft = Arc::new((*input.draft).clone());
            entry
        })
        .collect();
    assert!(crate::navigation::retained_bytes(unique.iter()) > crate::navigation::MAX_NAVIGATION_BYTES);
}

#[test]
fn fixed_field_outcomes_cannot_publish_to_another_draft_or_name_unknown_fields() {
    let mut model = model(password_schema());
    model.inline_arguments_completed(
        99,
        0,
        SubmissionOutcome::InvalidFields(vec![SubmissionFieldFailure {
            field: FieldId("password".into()),
            code: SubmissionFieldCode::Rejected,
        }]),
    );
    assert!(model.inline_arguments().unwrap().outcome.is_none());
    model.inline_arguments_completed(
        1,
        0,
        SubmissionOutcome::InvalidFields(vec![SubmissionFieldFailure {
            field: FieldId("other".into()),
            code: SubmissionFieldCode::Rejected,
        }]),
    );
    assert!(model.inline_arguments().unwrap().outcome.is_none());
    model.inline_arguments_completed(
        1,
        0,
        SubmissionOutcome::InvalidFields(vec![SubmissionFieldFailure {
            field: FieldId("password".into()),
            code: SubmissionFieldCode::Rejected,
        }]),
    );
    let input = model.inline_arguments().unwrap();
    let (output, _, _) = paint(
        &crate::create_launcher_context(),
        &mut TypedEditor::default(),
        input,
        Vec::new(),
    );
    text_position(&output, "Use three to eight bytes");
    model.apply(UiCommand::Cancel);
    model.inline_arguments_completed(1, 0, SubmissionOutcome::Completed);
    assert!(model.inline_arguments().is_none());
}

#[test]
fn editing_or_resubmitting_retires_older_fixed_field_failures() {
    let mut model = model(password_schema());
    let identity = model.inline_arguments().unwrap().clone();
    model.apply(UiCommand::TypedInputActivity {
        revision: 1,
        draft_revision: 1,
    });
    model.apply(UiCommand::SetTypedDraft {
        revision: 1,
        draft_revision: 1,
        draft: Default::default(),
    });
    assert!(model.accepts_typed_draft(1, 1, identity.view, identity.generation));
    model.inline_arguments_completed(
        1,
        0,
        SubmissionOutcome::InvalidFields(vec![SubmissionFieldFailure {
            field: FieldId("password".into()),
            code: SubmissionFieldCode::Rejected,
        }]),
    );
    assert!(model.inline_arguments().unwrap().outcome.is_none());
    model.apply(UiCommand::TypedInputActivity {
        revision: 1,
        draft_revision: 2,
    });
    model.apply(UiCommand::SetTypedDraft {
        revision: 1,
        draft_revision: 2,
        draft: Default::default(),
    });
    assert!(!model.accepts_typed_draft(1, 1, identity.view, identity.generation));
    model.inline_arguments_completed(1, 1, SubmissionOutcome::Failed(SubmissionFailureCode::Failed));
    assert!(model.inline_arguments().unwrap().outcome.is_none());
    model.inline_arguments_completed(
        1,
        2,
        SubmissionOutcome::InvalidFields(vec![SubmissionFieldFailure {
            field: FieldId("password".into()),
            code: SubmissionFieldCode::Rejected,
        }]),
    );
    assert!(matches!(
        model.inline_arguments().unwrap().outcome,
        Some(SubmissionOutcome::InvalidFields(_))
    ));
}

#[test]
fn shortcut_root_uses_the_existing_p3_stack_and_restores_a_nonsecret_draft() {
    let mut schema = (*password_schema()).clone();
    schema.fields.push(ArgumentField {
        id: FieldId("text".into()),
        label: "Text".into(),
        required: false,
        placeholder: None,
        validation_message: None,
        kind: FieldKind::Text {
            min_bytes: 0,
            max_bytes: 8,
            default: None,
            multiline: false,
        },
    });
    let mut model = model(Arc::new(schema));
    model.apply(UiCommand::SetTypedDraft {
        revision: 1,
        draft_revision: 1,
        draft: [(FieldId("text".into()), NonSecretValue::Text("draft".into()))].into(),
    });
    let original = model.inline_arguments().unwrap().clone();
    assert!(model.open_root_input());
    assert!(model.inline_arguments().is_none());
    assert!(model.selected_scope().is_none());
    model.apply(UiCommand::SetQuery("second command".into()));
    model.begin_generation(Generation::from_raw(2));
    model.apply(UiCommand::Back);
    assert!(Arc::ptr_eq(
        &original.draft,
        &model.inline_arguments().unwrap().draft
    ));
    assert_eq!(model.take_query_refresh().as_deref(), Some("typed command"));
    assert!(!model.accepts_typed_input(original.revision, original.view, original.generation));
}

#[test]
fn blank_scoped_roots_survive_shortcut_input_including_unavailable_scopes() {
    for available in [true, false] {
        let mut model = LauncherViewModel::new();
        model.activate();
        let scope = crate::ScopeIdentity {
            owner: PluginId("typed.owner".into()),
            id: crikey_core::result_presentation::ScopeId("private".into()),
            source: PublicationIdentity::Catalog {
                instance: 1,
                generation: Generation::from_raw(1),
                producer: 1,
                publication: 1,
            },
        };
        model.restore_navigation_state(NavigationRestoration {
            scope: Some(scope.clone()),
            ..Default::default()
        });
        model.begin_generation(Generation::from_raw(1));
        model.set_root_presentation(
            Generation::from_raw(1),
            if available {
                vec![crate::ScopeChoice {
                    identity: scope.clone(),
                    label: "Private".into(),
                }]
            } else {
                Vec::new()
            },
            Vec::new(),
            Vec::new(),
        );
        assert!(model.open_root_input());
        assert!(model.selected_scope().is_none());
        assert!(model.frame().unwrap().navigation.can_go_back);
        model.apply(UiCommand::Back);
        assert_eq!(model.selected_scope(), Some(&scope));
        assert!(model.navigation_restoration().query.is_empty());
        assert_eq!(model.take_query_refresh().as_deref(), Some(""));
    }
}

fn long_text_schema(bytes: usize) -> Arc<ArgumentSchema> {
    Arc::new(ArgumentSchema {
        fields: vec![ArgumentField {
            id: FieldId("text".into()),
            label: "Text".into(),
            required: true,
            placeholder: None,
            validation_message: None,
            kind: FieldKind::Text {
                min_bytes: 1,
                max_bytes: MAX_TEXT_BYTES,
                default: Some("a".repeat(bytes)),
                multiline: false,
            },
        }],
    })
}

#[test]
fn native_long_default_edits_stay_byte_bounded_and_submit_after_deletion() {
    let model = model(long_text_schema(5000));
    let input = model.inline_arguments().unwrap();
    let context = crate::create_launcher_context();
    let mut editor = TypedEditor::default();
    paint(&context, &mut editor, input, Vec::new());
    paint(&context, &mut editor, input, vec![egui::Event::Text("x".into())]);
    let Some(StructuredValue::NonSecret(NonSecretValue::Text(text))) =
        editor.values.0.get(&FieldId("text".into()))
    else {
        panic!("native text value");
    };
    assert_eq!(text.len(), 5001);
    assert!(
        text.capacity() <= MAX_TEXT_BYTES,
        "ordinary growth must not poison later validation"
    );
    let (output, _, _) = paint(
        &context,
        &mut editor,
        input,
        vec![egui::Event::Key {
            key: egui::Key::Backspace,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }],
    );
    let Some(StructuredValue::NonSecret(NonSecretValue::Text(text))) =
        editor.values.0.get(&FieldId("text".into()))
    else {
        panic!("native text value");
    };
    assert_eq!(text.len(), 5000);
    assert!(text.capacity() <= MAX_TEXT_BYTES);
    let position = text_position(&output, "Run command");
    let (_, _, submission) = paint(&context, &mut editor, input, click(position));
    let submission = submission.expect("valid edited default submits");
    assert!(input.schema.validate_values(&submission.values).is_ok());
}

#[test]
fn native_text_rejects_utf8_insertions_that_exceed_the_byte_bound() {
    let model = model(long_text_schema(MAX_TEXT_BYTES - 1));
    let input = model.inline_arguments().unwrap();
    let context = crate::create_launcher_context();
    let mut editor = TypedEditor::default();
    paint(&context, &mut editor, input, Vec::new());
    let (output, _, _) = paint(&context, &mut editor, input, vec![egui::Event::Text("é".into())]);
    let Some(StructuredValue::NonSecret(NonSecretValue::Text(text))) =
        editor.values.0.get(&FieldId("text".into()))
    else {
        panic!("native text value");
    };
    assert_eq!(text.len(), MAX_TEXT_BYTES - 1);
    assert!(text.capacity() <= MAX_TEXT_BYTES);
    let position = text_position(&output, "Run command");
    let (_, _, submission) = paint(&context, &mut editor, input, click(position));
    assert!(submission.is_some());
}

#[test]
fn back_source_refusal_retires_only_its_notice_when_current_typed_input_completes() {
    for unrelated_notice in [None, Some("Unrelated provider failure")] {
        let mut model = model(password_schema());
        let original = model.inline_arguments().unwrap().clone();
        assert!(model.open_root_input());
        model.apply(UiCommand::SetQuery("another command".into()));
        model.begin_generation(Generation::from_raw(2));
        model.apply(UiCommand::Back);
        assert!(
            model.inline_arguments().is_some(),
            "Back restores the old logical nonsecret context"
        );
        assert_eq!(model.take_query_refresh().as_deref(), Some("typed command"));
        // The production source-admission gate refuses the restored old source.
        model.retire_unavailable_inline_arguments();
        assert!(model.inline_arguments().is_none());
        assert!(model
            .frame()
            .unwrap()
            .navigation
            .unavailable
            .as_deref()
            .is_some_and(|notice| notice.starts_with("Command input source changed")));
        if let Some(notice) = unrelated_notice {
            model.set_navigation_notice(notice.into());
        }
        model.begin_generation(Generation::from_raw(3));
        assert!(model.open_inline_arguments(
            2,
            original.executable,
            original.schema,
            PublicationIdentity::Catalog {
                instance: 1,
                generation: Generation::from_raw(3),
                producer: 2,
                publication: 2,
            },
            None,
        ));
        model.inline_arguments_completed(2, 0, SubmissionOutcome::Completed);
        assert_eq!(
            model.inline_arguments().unwrap().outcome,
            Some(SubmissionOutcome::Completed)
        );
        assert_eq!(
            model.frame().unwrap().navigation.unavailable.as_deref(),
            unrelated_notice
        );
    }
}

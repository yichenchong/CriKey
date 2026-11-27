//! Root-only typed input. Live values never enter cloneable frames or commands.
use crikey_core::structured_arguments::{
    ArgumentSchema, ArgumentValues, FieldError, FieldKind, NonSecretValue, Password, StructuredValue,
    SubmissionOutcome,
};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineArguments {
    pub revision: u64,
    pub draft_revision: u64,
    pub draft_retained: bool,
    pub schema: Arc<ArgumentSchema>,
    pub outcome: Option<SubmissionOutcome>,
    pub draft: Arc<crikey_core::structured_arguments::NonSecretDraft>,
    pub executable: crikey_core::structured_arguments::ExecutableKey,
    pub source: crate::PublicationIdentity,
    pub fingerprint: [u8; 32],
    pub view: crate::ViewId,
    pub generation: crikey_core::Generation,
    pub command: Option<(crikey_core::CommandRef, u64)>,
}

/// Continuation authority, deliberately excluding all draft and live values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TypedInputIdentity {
    revision: u64,
    view: crate::ViewId,
    generation: crikey_core::Generation,
    fingerprint: [u8; 32],
    executable: crikey_core::structured_arguments::ExecutableKey,
    source: crate::PublicationIdentity,
    command: Option<(crikey_core::CommandRef, u64)>,
}

impl TypedInputIdentity {
    pub(crate) fn of(input: &InlineArguments) -> Self {
        Self {
            revision: input.revision,
            view: input.view,
            generation: input.generation,
            fingerprint: input.fingerprint,
            executable: input.executable.clone(),
            source: input.source.clone(),
            command: input.command.clone(),
        }
    }

    pub(crate) fn matches(&self, input: &InlineArguments) -> bool {
        self.revision == input.revision
            && self.view == input.view
            && self.generation == input.generation
            && self.fingerprint == input.fingerprint
            && self.executable == input.executable
            && self.source == input.source
            && self.command == input.command
    }
}

/// Move-only payload. Debug deliberately excludes even nonsecret argument text.
pub struct TypedInputSubmission {
    pub revision: u64,
    pub draft_revision: u64,
    pub view: crate::ViewId,
    pub generation: crikey_core::Generation,
    pub values: ArgumentValues,
}
impl std::fmt::Debug for TypedInputSubmission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TypedInputSubmission")
            .field("revision", &self.revision)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Default)]
pub(crate) struct TypedEditor {
    revision: Option<TypedInputIdentity>,
    draft_revision: u64,
    values: ArgumentValues,
    composing: bool,
    errors: Vec<FieldError>,
}

/// egui indexes characters; the schema and retained-value budget bound UTF-8 bytes.
struct BoundedTextBuffer<'a> {
    text: &'a mut String,
    max_bytes: usize,
}

impl<'a> BoundedTextBuffer<'a> {
    fn new(text: &'a mut String, max_bytes: usize) -> Self {
        let mut buffer = Self { text, max_bytes };
        buffer.compact();
        buffer
    }

    fn compact(&mut self) {
        if self.text.len() <= crikey_core::structured_arguments::MAX_TEXT_BYTES
            && self.text.capacity() > crikey_core::structured_arguments::MAX_TEXT_BYTES
        {
            *self.text = std::mem::take(self.text).into_boxed_str().into_string();
        }
    }

    fn byte_index(&self, character: usize) -> usize {
        self.text
            .char_indices()
            .nth(character)
            .map_or(self.text.len(), |(byte, _)| byte)
    }
}

impl egui::TextBuffer for BoundedTextBuffer<'_> {
    fn is_mutable(&self) -> bool {
        true
    }
    fn as_str(&self) -> &str {
        self.text
    }

    fn insert_text(&mut self, text: &str, character: usize) -> usize {
        let Some(bytes) = self
            .text
            .len()
            .checked_add(text.len())
            .filter(|bytes| *bytes <= self.max_bytes)
        else {
            return 0;
        };
        let at = self.byte_index(character);
        if bytes > self.text.capacity() {
            self.text.reserve_exact(bytes - self.text.len());
        }
        self.text.insert_str(at, text);
        text.chars().count()
    }

    fn delete_char_range(&mut self, characters: std::ops::Range<usize>) {
        let start = self.byte_index(characters.start);
        let end = self.byte_index(characters.end);
        if start <= end {
            self.text.replace_range(start..end, "");
        }
        self.compact();
    }
}

impl TypedEditor {
    pub(crate) fn clear(&mut self) {
        *self = Self::default();
    }
    pub(crate) fn retire_unless(&mut self, input: Option<&InlineArguments>) {
        let matches = match (&self.revision, input) {
            (Some(identity), Some(input)) => identity.matches(input),
            (None, None) => true,
            _ => false,
        };
        if !matches {
            self.clear();
        }
    }
    pub(crate) fn draw(
        &mut self,
        ui: &mut egui::Ui,
        input: Option<&InlineArguments>,
        commands: &mut Vec<crate::UiCommand>,
        admit_input: bool,
    ) -> Option<TypedInputSubmission> {
        let Some(input) = input else {
            self.clear();
            return None;
        };
        let fresh = self
            .revision
            .as_ref()
            .is_none_or(|identity| !identity.matches(input));
        if fresh {
            self.clear();
            self.revision = Some(TypedInputIdentity::of(input));
            self.draft_revision = input.draft_revision;
            self.values = input.schema.default_values().ok()?;
            for (id, value) in &input.draft.values {
                self.values
                    .0
                    .insert(id.clone(), StructuredValue::NonSecret(value.clone()));
            }
        }
        if admit_input {
            ui.input(|state| {
                for event in &state.events {
                    match event {
                        egui::Event::Ime(egui::ImeEvent::Preedit(text)) => self.composing = !text.is_empty(),
                        egui::Event::Ime(egui::ImeEvent::Commit(_) | egui::ImeEvent::Disabled) => {
                            self.composing = false
                        }
                        _ => {}
                    }
                }
            });
        }
        let ime_batch = ui.input(|state| {
            state
                .events
                .iter()
                .any(|event| matches!(event, egui::Event::Ime(_)))
        });
        let cancel = admit_input
            && !self.composing
            && !ime_batch
            && ui.input_mut(|state| state.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
        ui.separator();
        let mut changed = false;
        for (index, field) in input.schema.fields.iter().enumerate() {
            ui.label(if field.required {
                format!("{} *", field.label)
            } else {
                field.label.clone()
            });
            match &field.kind {
                FieldKind::Text {
                    multiline, max_bytes, ..
                } => {
                    let entry =
                        self.values.0.entry(field.id.clone()).or_insert_with(|| {
                            StructuredValue::NonSecret(NonSecretValue::Text(String::new()))
                        });
                    if let StructuredValue::NonSecret(NonSecretValue::Text(text)) = entry {
                        let mut buffer = BoundedTextBuffer::new(text, *max_bytes);
                        let edit = if *multiline {
                            egui::TextEdit::multiline(&mut buffer)
                        } else {
                            egui::TextEdit::singleline(&mut buffer)
                        };
                        let id = egui::Id::new(("inline-text", input.revision, &field.id.0));
                        if fresh && index == 0 {
                            ui.memory_mut(|memory| memory.request_focus(id));
                        }
                        changed |= ui
                            .add(edit.id(id).hint_text(field.placeholder.as_deref().unwrap_or("")))
                            .changed();
                    }
                }
                FieldKind::Password { .. } => {
                    let entry = self.values.0.entry(field.id.clone()).or_insert_with(|| {
                        StructuredValue::Password(Password::new(String::new()).expect("empty password"))
                    });
                    if let StructuredValue::Password(password) = entry {
                        // A fixed mask is the only password text ever painted. Editing is
                        // append/backspace native text input, not a cloneable egui buffer.
                        let response = ui.add(
                            egui::Button::new(if password.is_empty() {
                                "Enter password"
                            } else {
                                password.masked()
                            })
                            .sense(egui::Sense::click()),
                        );
                        if response.clicked() || fresh && index == 0 {
                            response.request_focus();
                        }
                        if response.has_focus() {
                            ui.ctx().output_mut(|output| {
                                output.ime = Some(egui::output::IMEOutput {
                                    rect: response.rect,
                                    cursor_rect: response.rect,
                                });
                            });
                            ui.input_mut(|state| {
                                state.events.retain(|event| match event {
                                    egui::Event::Text(text)
                                    | egui::Event::Paste(text)
                                    | egui::Event::Ime(egui::ImeEvent::Commit(text)) => {
                                        let end = password.expose_plaintext().len();
                                        changed |= password.replace_range(end..end, text).is_ok();
                                        false
                                    }
                                    egui::Event::Key {
                                        key: egui::Key::Backspace,
                                        pressed: true,
                                        ..
                                    } => {
                                        let text = password.expose_plaintext();
                                        let end = text.len();
                                        let start = text.char_indices().last().map_or(0, |(at, _)| at);
                                        changed |= password.replace_range(start..end, "").is_ok();
                                        false
                                    }
                                    egui::Event::Ime(_) => false,
                                    _ => true,
                                });
                            });
                        }
                    }
                }
                FieldKind::Dropdown { options, .. } => {
                    let selected = match self.values.0.get(&field.id) {
                        Some(StructuredValue::NonSecret(NonSecretValue::Dropdown(id))) => Some(id.clone()),
                        _ => None,
                    };
                    egui::ComboBox::from_id_salt((&field.id.0, input.revision))
                        .selected_text(
                            options
                                .iter()
                                .find(|option| Some(&option.id) == selected.as_ref())
                                .map_or("Choose…", |option| option.label.as_str()),
                        )
                        .show_ui(ui, |ui| {
                            for option in options {
                                if ui
                                    .selectable_label(Some(&option.id) == selected.as_ref(), &option.label)
                                    .clicked()
                                {
                                    self.values.0.insert(
                                        field.id.clone(),
                                        StructuredValue::NonSecret(NonSecretValue::Dropdown(
                                            option.id.clone(),
                                        )),
                                    );
                                    changed = true;
                                }
                            }
                        });
                }
            }
            let invalid = self
                .errors
                .iter()
                .any(|error| error.field.as_ref() == Some(&field.id))
                || matches!(&input.outcome, Some(SubmissionOutcome::InvalidFields(errors)) if errors.iter().any(|error| error.field == field.id));
            if invalid {
                ui.label(field.validation_message.as_deref().unwrap_or("Check this field"));
            }
        }
        if changed && admit_input {
            self.draft_revision = self.draft_revision.checked_add(1)?;
            self.errors.clear();
            commands.push(crate::UiCommand::TypedInputActivity {
                revision: input.revision,
                draft_revision: self.draft_revision,
            });
            commands.push(crate::UiCommand::UserActivity);
            if let Ok(draft) = input.schema.snapshot_values(&self.values) {
                commands.push(crate::UiCommand::SetTypedDraft {
                    revision: input.revision,
                    draft_revision: self.draft_revision,
                    draft,
                });
            }
        }
        if cancel {
            commands.push(crate::UiCommand::Cancel);
        }
        if let Some(SubmissionOutcome::Failed(_)) = input.outcome {
            ui.label("Submission refused; retry with the current command");
        }
        if matches!(input.outcome, Some(SubmissionOutcome::Completed)) {
            ui.label("Command completed");
        }
        if ui.button("Run command").clicked() && admit_input {
            match input.schema.validate_values(&self.values) {
                Ok(()) => {
                    let draft = input.schema.snapshot_values(&self.values).unwrap_or_default();
                    self.draft_revision = self.draft_revision.checked_add(1)?;
                    commands.push(crate::UiCommand::TypedInputActivity {
                        revision: input.revision,
                        draft_revision: self.draft_revision,
                    });
                    commands.push(crate::UiCommand::SetTypedDraft {
                        revision: input.revision,
                        draft_revision: self.draft_revision,
                        draft: draft.clone(),
                    });
                    let values = std::mem::take(&mut self.values);
                    self.values.0 = draft
                        .into_iter()
                        .map(|(id, value)| (id, StructuredValue::NonSecret(value)))
                        .collect();
                    return Some(TypedInputSubmission {
                        revision: input.revision,
                        draft_revision: self.draft_revision,
                        view: input.view,
                        generation: input.generation,
                        values,
                    });
                }
                Err(errors) => self.errors = errors,
            }
        }
        None
    }
}

#[cfg(test)]
#[path = "typed_input_tests.rs"]
mod tests;

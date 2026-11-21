//! Typed executable arguments, move-only live input and nonsecret snapshots.
//! Validation is post-construction; transports must enforce decode budgets first.
use crate::content_accounting::{account_schema, ContentFootprint};
use crate::{ActionId, ItemId, PluginId};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    ops::Range,
};

pub const TYPED_ARGUMENTS_CAPABILITY: &str = "typed_arguments_v1";
pub const MAX_FIELDS: usize = 64;
pub const MAX_TEXT_BYTES: usize = 8 * 1024;
pub const MAX_OPTIONS: usize = 256;
pub const MAX_ID_BYTES: usize = 256;
pub const MAX_SCHEMA_BYTES: usize = 256 * 1024;
macro_rules! identity {
    ($($name:ident),+ $(,)?) => {$(
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(pub String);
    )+};
}
identity!(FieldId, OptionId);
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OwnedItem {
    pub owner: PluginId,
    pub item: ItemId,
}
/// Exact executable identity, after resolving an omitted declared-command
/// action to the actual primary. None denotes genuinely actionless execution.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ExecutableKey {
    pub target: OwnedItem,
    pub action: Option<ActionId>,
}
impl ExecutableKey {
    pub fn validate(&self) -> Check {
        owned(&self.target)?;
        if let Some(action) = &self.action {
            id(&action.0)?;
        }
        Ok(())
    }
}
/// No Clone, Display, deref, serialization, or plaintext Debug. Render only a
/// fixed mask, never one character per secret byte. This is not locked memory.
/// Drop's ordinary buffer fill is best effort, not cryptographic erasure.
/// Transport/editor copies and crash collection require separate host policy.
pub struct Password(Vec<u8>);
impl Password {
    pub fn new(value: String) -> Result<Self, ValidationCode> {
        if value.len() > MAX_TEXT_BYTES || value.capacity() > MAX_TEXT_BYTES {
            let mut bytes = value.into_bytes();
            bytes.fill(0);
            return Err(ValidationCode::TooLong);
        }
        // Ownership transfers without copying the plaintext allocation.
        Ok(Self(value.into_bytes()))
    }
    /// In-place UTF-8 editing for native controls; no egui or plaintext copy.
    /// Byte ranges must be character boundaries. Refusals leave input intact.
    pub fn replace_range(&mut self, range: Range<usize>, text: &str) -> Result<(), ValidationCode> {
        let boundary =
            |index| index == self.0.len() || self.0.get(index).is_some_and(|byte| byte & 0xc0 != 0x80);
        if range.start > range.end || !boundary(range.start) || !boundary(range.end) {
            return Err(ValidationCode::InvalidBoundary);
        }
        let bytes = self.0.len() - (range.end - range.start);
        let bytes = bytes.checked_add(text.len()).ok_or(ValidationCode::TooLong)?;
        if bytes > MAX_TEXT_BYTES {
            return Err(ValidationCode::TooLong);
        }
        if bytes > self.0.capacity() {
            self.0.reserve_exact(bytes - self.0.len());
        }
        let old_len = self.0.len();
        if bytes > old_len {
            self.0.resize(bytes, 0);
        }
        // Move the existing suffix only; insertion borrows the caller's bytes.
        // All mutation is private and the validated edit preserves UTF-8.
        self.0.copy_within(range.end..old_len, range.start + text.len());
        self.0[range.start..range.start + text.len()].copy_from_slice(text.as_bytes());
        if bytes < old_len {
            self.0[bytes..old_len].fill(0);
            self.0.truncate(bytes);
        }
        Ok(())
    }
    pub(crate) fn retained_bytes(&self) -> usize {
        self.0.capacity()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn masked(&self) -> &'static str {
        if self.is_empty() {
            ""
        } else {
            "••••••••"
        }
    }
    /// Explicit plaintext borrow for a native text-buffer adapter or dispatch.
    /// Pair with replace_range for bounded in-place editing; never log, clone
    /// into renderer state, or include this borrow in a retained snapshot.
    pub fn expose_plaintext(&self) -> &str {
        std::str::from_utf8(&self.0).expect("password UTF-8 invariant")
    }
}
impl fmt::Debug for Password {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Password([REDACTED])")
    }
}
impl Drop for Password {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NonSecretValue {
    Text(String),
    Dropdown(OptionId),
}
#[derive(Debug)]
pub enum StructuredValue {
    NonSecret(NonSecretValue),
    Password(Password),
}
#[derive(Debug, Default)]
pub struct ArgumentValues(pub BTreeMap<FieldId, StructuredValue>);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropdownOption {
    pub id: OptionId,
    pub label: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldKind {
    Text {
        min_bytes: usize,
        max_bytes: usize,
        default: Option<String>,
        multiline: bool,
    },
    /// Password defaults are unrepresentable, including in the wire schema.
    Password { min_bytes: usize, max_bytes: usize },
    Dropdown {
        options: Vec<DropdownOption>,
        default: Option<OptionId>,
    },
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArgumentField {
    pub id: FieldId,
    pub label: String,
    pub required: bool,
    pub placeholder: Option<String>,
    /// Static provider-authored guidance, never a substituted rejected value.
    pub validation_message: Option<String>,
    pub kind: FieldKind,
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArgumentSchema {
    pub fields: Vec<ArgumentField>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationCode {
    InvalidBoundary,
    TooMany,
    TooLong,
    EmptyId,
    ControlInId,
    DuplicateId,
    InvalidDefault,
    InvalidBounds,
    Required,
    TooShort,
    WrongType,
    UnknownField,
    InvalidSelection,
    InvalidReference,
    WrongOwner,
    Overflow,
    RetainedBytes,
    RetainedPixels,
    InvalidPreview,
    StaleSurface,
    UnsupportedEvent,
    ForbiddenExecutionPolicy,
    InvalidArguments,
}
/// Errors contain only a field identity and fixed code: never rejected values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldError {
    /// None for global failures; never echoes an unknown submitted identity.
    pub field: Option<FieldId>,
    pub code: ValidationCode,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationError {
    pub field: Option<FieldId>,
    pub code: ValidationCode,
}
impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Even provider-authored identifiers are excluded from diagnostic text.
        write!(f, "invalid structured input: {:?}", self.code)
    }
}
impl std::error::Error for ValidationError {}
impl From<ValidationCode> for ValidationError {
    fn from(code: ValidationCode) -> Self {
        Self { field: None, code }
    }
}
pub(crate) type Check<T = ()> = Result<T, ValidationError>;
pub(crate) fn fail(code: ValidationCode) -> ValidationError {
    code.into()
}
pub(crate) fn id(value: &str) -> Check {
    if value.is_empty() {
        return Err(fail(ValidationCode::EmptyId));
    }
    if value.len() > MAX_ID_BYTES {
        return Err(fail(ValidationCode::TooLong));
    }
    if value.chars().any(char::is_control) {
        return Err(fail(ValidationCode::ControlInId));
    }
    Ok(())
}
pub(crate) fn text(value: &str) -> Check {
    if value.len() > MAX_TEXT_BYTES {
        Err(fail(ValidationCode::TooLong))
    } else {
        Ok(())
    }
}
pub(crate) fn owned(value: &OwnedItem) -> Check {
    id(&value.owner.0)?;
    id(&value.item.0)
}
fn length(bytes: usize, min: usize, max: usize, required: bool) -> Result<(), ValidationCode> {
    if bytes == 0 && required {
        return Err(ValidationCode::Required);
    }
    if bytes > max {
        return Err(ValidationCode::TooLong);
    }
    if bytes != 0 && bytes < min {
        return Err(ValidationCode::TooShort);
    }
    Ok(())
}
impl ArgumentSchema {
    pub fn validate(&self) -> Check {
        if self.fields.len() > MAX_FIELDS {
            return Err(fail(ValidationCode::TooMany));
        }
        let mut seen = BTreeSet::new();
        for field in &self.fields {
            let result = (|| {
                id(&field.id.0)?;
                if !seen.insert(&field.id) {
                    return Err(fail(ValidationCode::DuplicateId));
                }
                text(&field.label)?;
                if let Some(value) = &field.placeholder {
                    text(value)?;
                }
                if let Some(value) = &field.validation_message {
                    text(value)?;
                }
                match &field.kind {
                    FieldKind::Text {
                        min_bytes,
                        max_bytes,
                        default,
                        ..
                    } => {
                        bounds(*min_bytes, *max_bytes)?;
                        if let Some(value) = default {
                            length(value.len(), *min_bytes, *max_bytes, field.required)
                                .map_err(|_| fail(ValidationCode::InvalidDefault))?;
                        }
                    }
                    FieldKind::Password { min_bytes, max_bytes } => bounds(*min_bytes, *max_bytes)?,
                    FieldKind::Dropdown { options, default } => {
                        if options.is_empty() || options.len() > MAX_OPTIONS {
                            return Err(fail(ValidationCode::TooMany));
                        }
                        let mut ids = BTreeSet::new();
                        for option in options {
                            id(&option.id.0)?;
                            text(&option.label)?;
                            if !ids.insert(&option.id) {
                                return Err(fail(ValidationCode::DuplicateId));
                            }
                        }
                        if default.as_ref().is_some_and(|v| !ids.contains(v)) {
                            return Err(fail(ValidationCode::InvalidDefault));
                        }
                    }
                }
                Ok(())
            })();
            result.map_err(|mut error: ValidationError| {
                error.field = id(&field.id.0).ok().map(|_| field.id.clone());
                error
            })?;
        }
        let mut budget = ContentFootprint::new(MAX_SCHEMA_BYTES, 0);
        account_schema(self, &mut budget)?;
        Ok(())
    }
    /// SHA-256 of the validated, domain-separated v1 structural encoding.
    /// Optional transport omissions must be normalized before construction.
    /// Strings use exact UTF-8; neither Unicode nor ordering is normalized.
    pub fn fingerprint(&self) -> Result<[u8; 32], ValidationError> {
        self.validate()?;
        let mut hash = Sha256::new();
        hash.update(b"crikey.argument-schema\0");
        hash.update(1u32.to_le_bytes());
        hash.update((self.fields.len() as u32).to_le_bytes());
        for field in &self.fields {
            hash_string(&mut hash, &field.id.0);
            hash_string(&mut hash, &field.label);
            hash.update([u8::from(field.required)]);
            hash_optional_string(&mut hash, field.placeholder.as_deref());
            hash_optional_string(&mut hash, field.validation_message.as_deref());
            match &field.kind {
                FieldKind::Text {
                    min_bytes,
                    max_bytes,
                    default,
                    multiline,
                } => {
                    hash.update([0]);
                    hash.update((*min_bytes as u32).to_le_bytes());
                    hash.update((*max_bytes as u32).to_le_bytes());
                    hash.update([u8::from(*multiline)]);
                    hash_optional_string(&mut hash, default.as_deref());
                }
                FieldKind::Password { min_bytes, max_bytes } => {
                    hash.update([1]);
                    hash.update((*min_bytes as u32).to_le_bytes());
                    hash.update((*max_bytes as u32).to_le_bytes());
                }
                FieldKind::Dropdown { options, default } => {
                    hash.update([2]);
                    hash.update((options.len() as u32).to_le_bytes());
                    for option in options {
                        hash_string(&mut hash, &option.id.0);
                        hash_string(&mut hash, &option.label);
                    }
                    hash_optional_string(&mut hash, default.as_ref().map(|value| value.0.as_str()));
                }
            }
        }
        Ok(hash.finalize().into())
    }
    /// Defaults apply only when constructing a new draft, never on submission.
    pub fn default_values(&self) -> Check<ArgumentValues> {
        self.validate()?;
        let mut values = BTreeMap::new();
        for field in &self.fields {
            let value = match &field.kind {
                FieldKind::Text { default: Some(v), .. } => Some(NonSecretValue::Text(v.clone())),
                FieldKind::Dropdown { default: Some(v), .. } => Some(NonSecretValue::Dropdown(v.clone())),
                _ => None,
            };
            if let Some(value) = value {
                values.insert(field.id.clone(), StructuredValue::NonSecret(value));
            }
        }
        Ok(ArgumentValues(values))
    }
    pub fn validate_values(&self, values: &ArgumentValues) -> Result<(), Vec<FieldError>> {
        if let Err(error) = self.validate() {
            return Err(vec![FieldError {
                field: error.field,
                code: error.code,
            }]);
        }
        if values.0.len() > MAX_FIELDS {
            return Err(vec![FieldError {
                field: None,
                code: ValidationCode::TooMany,
            }]);
        }
        for key in values.0.keys() {
            if let Err(error) = id(&key.0) {
                return Err(vec![FieldError {
                    field: None,
                    code: error.code,
                }]);
            }
        }
        let mut errors = Vec::new();
        for key in values.0.keys() {
            if !self.fields.iter().any(|f| &f.id == key) {
                errors.push(FieldError {
                    field: None,
                    code: ValidationCode::UnknownField,
                });
                if errors.len() == MAX_FIELDS + 1 {
                    return Err(errors);
                }
            }
        }
        for field in &self.fields {
            let result = match values.0.get(&field.id) {
                None if field.required => Err(ValidationCode::Required),
                None => Ok(()),
                Some(value) => validate_value(field, value),
            };
            if let Err(code) = result {
                errors.push(FieldError {
                    field: Some(field.id.clone()),
                    code,
                });
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
    /// Takes only nonsecret draft fields, allowing incomplete required values.
    /// Passwords are omitted, never replaced by a placeholder or a length.
    pub fn snapshot_values(
        &self,
        values: &ArgumentValues,
    ) -> Result<BTreeMap<FieldId, NonSecretValue>, Vec<FieldError>> {
        if let Err(errors) = self.validate_values(values) {
            let errors: Vec<_> = errors
                .into_iter()
                .filter(|e| !matches!(e.code, ValidationCode::Required | ValidationCode::TooShort))
                .collect();
            if !errors.is_empty() {
                return Err(errors);
            }
        }
        Ok(values
            .0
            .iter()
            .filter_map(|(id, value)| match value {
                StructuredValue::NonSecret(value) => Some((id.clone(), value.clone())),
                StructuredValue::Password(_) => None,
            })
            .collect())
    }
}
// Every caller hashes only after schema validation, which bounds every string,
// count and byte bound well below u32::MAX before any narrowing conversion.
fn hash_string(hash: &mut Sha256, value: &str) {
    hash.update((value.len() as u32).to_le_bytes());
    hash.update(value.as_bytes());
}
fn hash_optional_string(hash: &mut Sha256, value: Option<&str>) {
    hash.update([u8::from(value.is_some())]);
    if let Some(value) = value {
        hash_string(hash, value);
    }
}
fn bounds(min: usize, max: usize) -> Check {
    if min > max || max > MAX_TEXT_BYTES {
        Err(fail(ValidationCode::InvalidBounds))
    } else {
        Ok(())
    }
}
fn validate_value(field: &ArgumentField, value: &StructuredValue) -> Result<(), ValidationCode> {
    match (&field.kind, value) {
        (
            FieldKind::Text {
                min_bytes, max_bytes, ..
            },
            StructuredValue::NonSecret(NonSecretValue::Text(v)),
        ) => {
            if v.capacity() > MAX_TEXT_BYTES {
                return Err(ValidationCode::TooLong);
            }
            length(v.len(), *min_bytes, *max_bytes, field.required)
        }
        (FieldKind::Password { min_bytes, max_bytes }, StructuredValue::Password(v)) => {
            length(v.0.len(), *min_bytes, *max_bytes, field.required)
        }
        (FieldKind::Dropdown { options, .. }, StructuredValue::NonSecret(NonSecretValue::Dropdown(v))) => {
            if v.0.capacity() > MAX_ID_BYTES {
                return Err(ValidationCode::TooLong);
            }
            if options.iter().any(|o| &o.id == v) {
                Ok(())
            } else {
                Err(ValidationCode::InvalidSelection)
            }
        }
        _ => Err(ValidationCode::WrongType),
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NonSecretDraft {
    pub executable: ExecutableKey,
    pub values: BTreeMap<FieldId, NonSecretValue>,
}
/// Mutually exclusive by construction; structured values are never flattened
/// into shell/query/legacy text. No Clone, Serialize, or Display on live input.
pub enum SubmissionArguments {
    None,
    LegacyText(String),
    Structured(ArgumentValues),
}
impl fmt::Debug for SubmissionArguments {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::None => "SubmissionArguments::None",
            Self::LegacyText(_) => "SubmissionArguments::LegacyText([REDACTED])",
            Self::Structured(_) => "SubmissionArguments::Structured([REDACTED])",
        })
    }
}
#[derive(Debug)]
pub struct ActionSubmission {
    pub executable: ExecutableKey,
    pub arguments: SubmissionArguments,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmissionFailureCode {
    Unavailable,
    Denied,
    Cancelled,
    Timeout,
    Unsupported,
    Failed,
    SchemaMismatch,
    ResourceLimit,
    ProtocolViolation,
}
/// A callback may reject a declared field but cannot supply diagnostic text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmissionFieldCode {
    Required,
    TooShort,
    TooLong,
    InvalidSelection,
    Rejected,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmissionFieldFailure {
    pub field: FieldId,
    pub code: SubmissionFieldCode,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmissionOutcome {
    Completed,
    InvalidFields(Vec<SubmissionFieldFailure>),
    Failed(SubmissionFailureCode),
}
impl SubmissionOutcome {
    /// UI messages resolve only from the current schema's validation_message.
    /// Callback stdout/exception/traceback text is never part of this contract.
    pub fn validate(&self, schema: Option<&ArgumentSchema>) -> Check {
        match self {
            Self::InvalidFields(errors) => {
                let schema = schema.ok_or_else(|| fail(ValidationCode::InvalidArguments))?;
                schema.validate()?;
                if errors.is_empty() || errors.len() > MAX_FIELDS {
                    return Err(fail(ValidationCode::TooMany));
                }
                let mut seen = BTreeSet::new();
                for error in errors {
                    id(&error.field.0)?;
                    if !schema.fields.iter().any(|f| f.id == error.field) {
                        return Err(fail(ValidationCode::UnknownField));
                    }
                    if !seen.insert(&error.field) {
                        return Err(fail(ValidationCode::DuplicateId));
                    }
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}
pub(crate) fn validate_nonsecret(
    field: &ArgumentField,
    value: &NonSecretValue,
) -> Result<(), ValidationCode> {
    match (&field.kind, value) {
        (FieldKind::Text { max_bytes, .. }, NonSecretValue::Text(v)) => {
            if v.len() > *max_bytes || v.capacity() > MAX_TEXT_BYTES {
                Err(ValidationCode::TooLong)
            } else {
                Ok(())
            }
        }
        (FieldKind::Dropdown { options, .. }, NonSecretValue::Dropdown(v)) => {
            if v.0.capacity() > MAX_ID_BYTES {
                return Err(ValidationCode::TooLong);
            }
            if options.iter().any(|o| &o.id == v) {
                Ok(())
            } else {
                Err(ValidationCode::InvalidSelection)
            }
        }
        _ => Err(ValidationCode::WrongType),
    }
}

impl ArgumentSchema {
    pub fn snapshot_draft(
        &self,
        executable: ExecutableKey,
        values: &ArgumentValues,
    ) -> Result<NonSecretDraft, Vec<FieldError>> {
        executable.validate().map_err(|error| {
            vec![FieldError {
                field: None,
                code: error.code,
            }]
        })?;
        Ok(NonSecretDraft {
            executable,
            values: self.snapshot_values(values)?,
        })
    }
    /// Drafts may be incomplete but cannot contain a password or unknown field.
    pub fn validate_draft(&self, draft: &NonSecretDraft) -> Check {
        self.validate()?;
        draft.executable.validate()?;
        if draft.values.len() > MAX_FIELDS {
            return Err(fail(ValidationCode::TooMany));
        }
        for (key, value) in &draft.values {
            id(&key.0)?;
            let field = self
                .fields
                .iter()
                .find(|f| &f.id == key)
                .ok_or_else(|| fail(ValidationCode::UnknownField))?;
            validate_nonsecret(field, value).map_err(|code| ValidationError {
                field: Some(key.clone()),
                code,
            })?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "structured_arguments_tests.rs"]
mod tests;

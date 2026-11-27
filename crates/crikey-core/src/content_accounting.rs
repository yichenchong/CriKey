//! Caller-owned mixed-content footprint calculation; no stack or allowance.
use crate::structured_arguments::*;
use crate::structured_arguments::{fail, Check};
use std::{any::TypeId, collections::HashSet, fmt, marker::PhantomData, mem::size_of, sync::Arc};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RetainedResources {
    pub bytes: usize,
    pub pixels: usize,
}
/// Content accounting for ONE host-owned mixed-stack admission calculation.
/// No depth, entry list, independent session allowance or retention decisions.
/// Charge root/custom/settings content and renderer copies through the same
/// accumulator. Keep all charged Arc owners alive for its borrow lifetime.
/// Discard the calculation on any error; no partial admission is valid.
pub struct ContentFootprint<'a> {
    resources: RetainedResources,
    byte_limit: usize,
    pixel_limit: usize,
    shared: HashSet<(TypeId, usize)>,
    retained: PhantomData<&'a ()>,
}
impl fmt::Debug for ContentFootprint<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ContentFootprint")
            .field("byte_limit", &self.byte_limit)
            .field("pixel_limit", &self.pixel_limit)
            .field("shared_allocations", &self.shared.len())
            .finish_non_exhaustive()
    }
}
impl<'a> ContentFootprint<'a> {
    pub fn new(byte_limit: usize, pixel_limit: usize) -> Self {
        Self {
            resources: RetainedResources::default(),
            byte_limit,
            pixel_limit,
            shared: HashSet::new(),
            retained: PhantomData,
        }
    }
    pub fn resources(&self) -> RetainedResources {
        self.resources
    }
    /// Pixels are also host/GPU retained bytes, not a separate free allowance.
    pub fn charge(&mut self, bytes: usize, pixels: usize) -> Check {
        if pixels > bytes {
            return Err(fail(ValidationCode::InvalidBounds));
        }
        let bytes = self
            .resources
            .bytes
            .checked_add(bytes)
            .ok_or_else(|| fail(ValidationCode::Overflow))?;
        let pixels = self
            .resources
            .pixels
            .checked_add(pixels)
            .ok_or_else(|| fail(ValidationCode::Overflow))?;
        if bytes > self.byte_limit {
            return Err(fail(ValidationCode::RetainedBytes));
        }
        if pixels > self.pixel_limit {
            return Err(fail(ValidationCode::RetainedPixels));
        }
        self.resources = RetainedResources { bytes, pixels };
        Ok(())
    }
    /// Shared-allocation seam for existing root/preview/navigation consumers.
    /// `bytes` includes the complete allocation (including Arc header), while
    /// inline parent handles must already be charged with their parent.
    pub fn charge_shared<T: ?Sized + 'static>(
        &mut self,
        allocation: &'a Arc<T>,
        bytes: usize,
        pixels: usize,
    ) -> Check<bool> {
        let key = (TypeId::of::<T>(), Arc::as_ptr(allocation) as *const () as usize);
        if self.shared.contains(&key) {
            return Ok(false);
        }
        self.charge(bytes, pixels)?;
        self.shared.insert(key);
        Ok(true)
    }
    pub(crate) fn begin_shared<T: ?Sized + 'static>(
        &mut self,
        allocation: &'a Arc<T>,
        inline_bytes: usize,
    ) -> Check<bool> {
        let bytes = inline_bytes
            .checked_add(2 * size_of::<usize>())
            .ok_or_else(|| fail(ValidationCode::Overflow))?;
        self.charge_shared(allocation, bytes, 0)
    }
    pub(crate) fn bytes(&mut self, bytes: usize) -> Check {
        self.charge(bytes, 0)
    }
    pub(crate) fn string(&mut self, value: &String) -> Check {
        self.bytes(value.capacity())
    }
    pub(crate) fn map_record<K, V>(&mut self) -> Check {
        // Conservative full standard-library BTree node per record: eleven
        // pair slots plus links/header. Count unused node capacity as retained.
        let bytes = size_of::<(K, V)>()
            .checked_mul(11)
            .and_then(|n| n.checked_add(16 * size_of::<usize>()))
            .ok_or_else(|| fail(ValidationCode::Overflow))?;
        self.bytes(bytes)
    }
    pub(crate) fn array<T>(&mut self, capacity: usize) -> Check {
        self.bytes(
            capacity
                .checked_mul(size_of::<T>())
                .ok_or_else(|| fail(ValidationCode::Overflow))?,
        )
    }
    /// Charge a shared schema once in the caller's mixed-content calculation.
    pub fn include_schema(&mut self, schema: &'a Arc<ArgumentSchema>) -> Check {
        if self.begin_shared(schema, size_of::<ArgumentSchema>())? {
            account_schema(schema, self)?;
        }
        Ok(())
    }
    pub fn include_values(&mut self, values: &ArgumentValues) -> Check {
        self.bytes(size_of::<ArgumentValues>())?;
        for (key, value) in &values.0 {
            self.map_record::<FieldId, StructuredValue>()?;
            self.string(&key.0)?;
            match value {
                StructuredValue::NonSecret(NonSecretValue::Text(v)) => self.string(v)?,
                StructuredValue::NonSecret(NonSecretValue::Dropdown(v)) => self.string(&v.0)?,
                StructuredValue::Password(v) => self.bytes(v.retained_bytes())?,
            }
        }
        Ok(())
    }
    pub fn include_draft(&mut self, draft: &NonSecretDraft) -> Check {
        self.bytes(size_of::<NonSecretDraft>())?;
        account_owned(&draft.executable.target, self)?;
        if let Some(action) = &draft.executable.action {
            self.string(&action.0)?;
        }
        for (key, value) in &draft.values {
            self.map_record::<FieldId, NonSecretValue>()?;
            self.string(&key.0)?;
            match value {
                NonSecretValue::Text(v) => self.string(v)?,
                NonSecretValue::Dropdown(v) => self.string(&v.0)?,
            }
        }
        Ok(())
    }
}
pub(crate) fn account_owned(value: &OwnedItem, budget: &mut ContentFootprint<'_>) -> Check {
    budget.string(&value.owner.0)?;
    budget.string(&value.item.0)
}
pub(crate) fn account_schema(schema: &ArgumentSchema, budget: &mut ContentFootprint<'_>) -> Check {
    budget.array::<ArgumentField>(schema.fields.capacity())?;
    for field in &schema.fields {
        budget.string(&field.id.0)?;
        budget.string(&field.label)?;
        if let Some(value) = &field.placeholder {
            budget.string(value)?;
        }
        if let Some(value) = &field.validation_message {
            budget.string(value)?;
        }
        match &field.kind {
            FieldKind::Text { default: Some(v), .. } => budget.string(v)?,
            FieldKind::Dropdown { options, default } => {
                budget.array::<DropdownOption>(options.capacity())?;
                for option in options {
                    budget.string(&option.id.0)?;
                    budget.string(&option.label)?;
                }
                if let Some(v) = default {
                    budget.string(&v.0)?;
                }
            }
            _ => (),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_allocations_and_renderer_copies_use_one_budget() {
        let allocation = Arc::new([0u8; 8]);
        let alias = Arc::clone(&allocation);
        let mut budget = ContentFootprint::new(32, 16);
        assert!(budget.charge_shared(&allocation, 24, 8).unwrap());
        assert!(!budget.charge_shared(&alias, 24, 8).unwrap());
        budget.charge(8, 8).unwrap();
        assert_eq!(
            budget.resources(),
            RetainedResources {
                bytes: 32,
                pixels: 16
            }
        );
        assert_eq!(
            budget.charge(1, 0).unwrap_err().code,
            ValidationCode::RetainedBytes
        );
    }

    #[test]
    fn overflow_and_pixel_bounds_refuse_without_partial_charge() {
        let mut budget = ContentFootprint::new(usize::MAX, usize::MAX);
        assert_eq!(
            budget.charge(0, 1).unwrap_err().code,
            ValidationCode::InvalidBounds
        );
        assert_eq!(budget.resources(), RetainedResources::default());
        budget.charge(usize::MAX, 0).unwrap();
        assert_eq!(budget.charge(1, 0).unwrap_err().code, ValidationCode::Overflow);
        assert_eq!(budget.resources().bytes, usize::MAX);
        let mut pixels = ContentFootprint::new(32, 8);
        assert_eq!(
            pixels.charge(16, 9).unwrap_err().code,
            ValidationCode::RetainedPixels
        );
        assert_eq!(pixels.resources(), RetainedResources::default());
    }

    #[test]
    fn optional_schema_metadata_charges_retained_capacity() {
        let mut schema = ArgumentSchema {
            fields: vec![ArgumentField {
                id: FieldId("field".into()),
                label: "Field".into(),
                required: false,
                placeholder: None,
                validation_message: None,
                kind: FieldKind::Password {
                    min_bytes: 0,
                    max_bytes: MAX_TEXT_BYTES,
                },
            }],
        };
        let mut absent = ContentFootprint::new(usize::MAX, 0);
        account_schema(&schema, &mut absent).unwrap();
        let baseline = absent.resources().bytes;
        schema.fields[0].placeholder = Some(String::with_capacity(128));
        schema.fields[0].validation_message = Some(String::with_capacity(256));
        let capacity = schema.fields[0].placeholder.as_ref().unwrap().capacity()
            + schema.fields[0].validation_message.as_ref().unwrap().capacity();
        let mut present = ContentFootprint::new(usize::MAX, 0);
        account_schema(&schema, &mut present).unwrap();
        assert_eq!(present.resources().bytes, baseline + capacity);
        let mut bounded = ContentFootprint::new(baseline + capacity - 1, 0);
        assert_eq!(
            account_schema(&schema, &mut bounded).unwrap_err().code,
            ValidationCode::RetainedBytes
        );
    }
}

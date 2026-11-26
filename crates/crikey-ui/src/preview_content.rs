//! Provider-thread preparation of immutable host-rendered preview content.
pub use crikey_core::preview::{PreviewBlock, PreviewBlockKind, PreviewSpan};
use crikey_core::{PreviewContent, PreviewText};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq)]
pub struct PreparedPreview {
    pub content: Arc<PreviewContent>,
    pub blocks: Arc<[PreviewBlock]>,
}

/// Call on the provider worker, never from paint or the UI event loop.
pub fn prepare_preview(mut content: Arc<PreviewContent>) -> Result<PreparedPreview, String> {
    content.validate().map_err(|error| error.to_string())?;
    let markdown = matches!(content.text, Some(PreviewText::Markdown(_)));
    let metadata_bytes = content
        .values
        .iter()
        .map(|value| value.label.len() + value.value.len())
        .sum::<usize>();
    let blocks = match &content.text {
        Some(PreviewText::Markdown(source)) => crikey_core::preview::parse_preview_markdown_with_budget(
            source,
            crikey_core::preview::MAX_PREVIEW_NODES.saturating_sub(content.values.len()),
            crikey_core::preview::MAX_PREVIEW_TEXT_BYTES.saturating_sub(metadata_bytes),
        )
        .map_err(|error| error.to_string())?,
        _ => Vec::new(),
    };
    let nodes = blocks
        .iter()
        .try_fold(content.values.len(), |count, block| {
            count
                .checked_add(1)
                .and_then(|count| count.checked_add(block.spans.len()))
        })
        .ok_or("Preview node count overflow")?;
    if nodes > crikey_core::preview::MAX_PREVIEW_NODES {
        return Err("Prepared preview exceeds node limit".into());
    }
    if markdown {
        if let Some(content) = Arc::get_mut(&mut content) {
            content.text = None;
        }
    }
    Ok(PreparedPreview {
        content,
        blocks: blocks.into(),
    })
}

//! Bounded host-rendered preview content. No URLs are fetched by this model.

pub const PREVIEW_CAPABILITY: &str = "preview_v1";
pub const PREVIEW_METADATA_KEY: &str = "crikey.preview";
pub const MAX_PREVIEW_TEXT_BYTES: usize = 256 * 1024;
pub const MAX_PREVIEW_NODES: usize = 4096;
pub const MAX_PREVIEW_VALUES: usize = 128;
pub const MAX_PREVIEW_VALUE_BYTES: usize = 8 * 1024;
pub const MAX_PREVIEW_IMAGE_EDGE: u32 = 1024;
pub const MAX_PREVIEW_IMAGE_BYTES: usize = 1024 * 1024;
pub const MAX_RETAINED_PREVIEW_PIXELS: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewDescriptor {
    Inline(PreviewContent),
    Deferred,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PreviewContent {
    pub text: Option<PreviewText>,
    pub image: Option<PreviewImage>,
    pub values: Vec<PreviewValue>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewText {
    Plain(String),
    /// A safe, non-executable markdown source. Renderers must not interpret
    /// embedded HTML or fetch images; links require deliberate activation.
    Markdown(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewValue {
    pub label: String,
    pub value: String,
    pub kind: PreviewValueKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewValueKind {
    Text,
    Link,
    Tag,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewOutcome {
    Content(PreviewContent),
    Empty,
    Unsupported,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid preview: {0}")]
pub struct PreviewError(pub &'static str);

/// Only these link schemes can be handed to a deliberate host activation.
pub fn validate_preview_link(value: &str) -> Result<(), PreviewError> {
    if value.len() > MAX_PREVIEW_VALUE_BYTES || value.chars().any(char::is_control) {
        return Err(PreviewError("invalid link length or control character"));
    }
    if !(value.starts_with("https://") || value.starts_with("http://")) {
        return Err(PreviewError("unsupported link scheme"));
    }
    Ok(())
}

impl PreviewImage {
    pub fn validate(&self) -> Result<(), PreviewError> {
        let bytes = self
            .width
            .checked_mul(self.height)
            .and_then(|n| n.checked_mul(4))
            .ok_or(PreviewError("image dimensions overflow"))?;
        if self.width == 0
            || self.height == 0
            || self.width > MAX_PREVIEW_IMAGE_EDGE
            || self.height > MAX_PREVIEW_IMAGE_EDGE
            || bytes as usize > MAX_PREVIEW_IMAGE_BYTES
            || bytes as usize != self.rgba.len()
        {
            return Err(PreviewError("image dimensions or RGBA byte count exceed bounds"));
        }
        Ok(())
    }
}

impl PreviewContent {
    pub fn validate(&self) -> Result<(), PreviewError> {
        if self.values.len() > MAX_PREVIEW_VALUES {
            return Err(PreviewError("too many metadata values"));
        }
        let mut total = 0usize;
        for value in &self.values {
            if value.label.len() > MAX_PREVIEW_VALUE_BYTES || value.value.len() > MAX_PREVIEW_VALUE_BYTES {
                return Err(PreviewError("metadata value exceeds bounds"));
            }
            total = total
                .checked_add(value.label.len())
                .and_then(|n| n.checked_add(value.value.len()))
                .ok_or(PreviewError("text size overflow"))?;
            if value.kind == PreviewValueKind::Link {
                validate_preview_link(&value.value)?;
            }
        }
        if let Some(text) = &self.text {
            let source = match text {
                PreviewText::Plain(s) | PreviewText::Markdown(s) => s,
            };
            total = total
                .checked_add(source.len())
                .ok_or(PreviewError("text size overflow"))?;
        }
        if total > MAX_PREVIEW_TEXT_BYTES {
            return Err(PreviewError("text exceeds bounds"));
        }
        if let Some(image) = &self.image {
            image.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewBlockKind {
    Paragraph,
    Heading(u8),
    Code,
    Bullet,
    Rule,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewBlock {
    pub kind: PreviewBlockKind,
    pub spans: Vec<PreviewSpan>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewSpan {
    pub text: String,
    pub strong: bool,
    pub emphasis: bool,
    pub code: bool,
    pub link: Option<String>,
}

/// Prepares safe markdown once, on a provider worker, never during paint.
pub fn parse_preview_markdown(source: &str) -> Result<Vec<PreviewBlock>, PreviewError> {
    parse_preview_markdown_with_budget(source, MAX_PREVIEW_NODES, MAX_PREVIEW_TEXT_BYTES)
}

/// Shares the preview's aggregate node/text budget with its metadata values.
/// Both the source and the retained AST fit the supplied byte budget.
pub fn parse_preview_markdown_with_budget(
    source: &str,
    max_nodes: usize,
    max_bytes: usize,
) -> Result<Vec<PreviewBlock>, PreviewError> {
    use pulldown_cmark::{Event, Parser, Tag, TagEnd};
    let max_nodes = max_nodes.min(MAX_PREVIEW_NODES);
    let max_bytes = max_bytes.min(MAX_PREVIEW_TEXT_BYTES);
    if source.len() > max_bytes {
        return Err(PreviewError("markdown source exceeds byte limit"));
    }
    let mut blocks = Vec::new();
    let mut current = PreviewBlock {
        kind: PreviewBlockKind::Paragraph,
        spans: Vec::new(),
    };
    let mut strong = 0usize;
    let mut emphasis = 0usize;
    let mut link: Option<String> = None;
    let mut nodes = 0usize;
    let mut bytes = 0usize;
    let flush = |blocks: &mut Vec<PreviewBlock>, current: &mut PreviewBlock| {
        if !current.spans.is_empty() {
            blocks.push(std::mem::replace(
                current,
                PreviewBlock {
                    kind: PreviewBlockKind::Paragraph,
                    spans: Vec::new(),
                },
            ));
        }
    };
    for event in Parser::new(source) {
        nodes = nodes
            .checked_add(1)
            .ok_or(PreviewError("markdown node count overflow"))?;
        if nodes > max_nodes {
            return Err(PreviewError("markdown exceeds node limit"));
        }
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {}
                Tag::Heading { level, .. } => {
                    flush(&mut blocks, &mut current);
                    current.kind = PreviewBlockKind::Heading(level as u8);
                }
                Tag::CodeBlock(_) => {
                    flush(&mut blocks, &mut current);
                    current.kind = PreviewBlockKind::Code;
                }
                Tag::Item => {
                    flush(&mut blocks, &mut current);
                    current.kind = PreviewBlockKind::Bullet;
                }
                Tag::List(_) | Tag::BlockQuote(_) => flush(&mut blocks, &mut current),
                Tag::Strong => strong += 1,
                Tag::Emphasis => emphasis += 1,
                Tag::Link { dest_url, .. } => {
                    validate_preview_link(&dest_url)?;
                    link = Some(dest_url.into_string());
                }
                Tag::Image { .. } | Tag::HtmlBlock => {
                    return Err(PreviewError("embedded markdown assets and HTML are unsupported"))
                }
                _ => return Err(PreviewError("unsupported markdown structure")),
            },
            Event::End(tag) => match tag {
                TagEnd::Strong => strong = strong.saturating_sub(1),
                TagEnd::Emphasis => emphasis = emphasis.saturating_sub(1),
                TagEnd::Link => link = None,
                TagEnd::Paragraph
                | TagEnd::Heading(_)
                | TagEnd::CodeBlock
                | TagEnd::Item
                | TagEnd::BlockQuote(_)
                | TagEnd::List(_) => flush(&mut blocks, &mut current),
                _ => {}
            },
            Event::Rule => {
                flush(&mut blocks, &mut current);
                blocks.push(PreviewBlock {
                    kind: PreviewBlockKind::Rule,
                    spans: Vec::new(),
                });
            }
            Event::Html(_) | Event::InlineHtml(_) => {
                return Err(PreviewError("markdown HTML is unsupported"))
            }
            Event::Text(_) | Event::Code(_) | Event::SoftBreak | Event::HardBreak => {
                let code = matches!(event, Event::Code(_)) || current.kind == PreviewBlockKind::Code;
                let text = match event {
                    Event::Text(text) | Event::Code(text) => text.into_string(),
                    _ => "\n".into(),
                };
                bytes = bytes
                    .checked_add(text.len())
                    .and_then(|n| n.checked_add(link.as_ref().map_or(0, String::len)))
                    .ok_or(PreviewError("prepared markdown size overflow"))?;
                if bytes > max_bytes {
                    return Err(PreviewError("prepared markdown exceeds byte limit"));
                }
                current.spans.push(PreviewSpan {
                    text,
                    strong: strong != 0,
                    emphasis: emphasis != 0,
                    code,
                    link: link.clone(),
                });
            }
            _ => return Err(PreviewError("unsupported markdown event")),
        }
    }
    flush(&mut blocks, &mut current);
    Ok(blocks)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn aggregate_text_and_images_are_bounded() {
        let content = PreviewContent {
            text: Some(PreviewText::Plain("x".repeat(MAX_PREVIEW_TEXT_BYTES + 1))),
            ..Default::default()
        };
        assert!(content.validate().is_err());
        assert!(PreviewImage {
            width: 1024,
            height: 1024,
            rgba: vec![]
        }
        .validate()
        .is_err());
        assert!(validate_preview_link("javascript:alert(1)").is_err());
        assert!(validate_preview_link("mailto:person@example.org").is_err());
        assert!(validate_preview_link("https://example.org").is_ok());
        assert!(parse_preview_markdown("<script>x</script>").is_err());
    }

    #[test]
    fn markdown_is_formatted_once_as_safe_bounded_blocks() {
        let blocks = parse_preview_markdown("# Heading\n\n**strong** and *emphasis* with `<tag>` and [site](https://example.org)\n\n- item\n\n```\nif a < b {}\n```").unwrap();
        assert_eq!(blocks[0].kind, PreviewBlockKind::Heading(1));
        assert!(blocks
            .iter()
            .flat_map(|b| &b.spans)
            .any(|s| s.strong && s.text == "strong"));
        assert!(blocks
            .iter()
            .flat_map(|b| &b.spans)
            .any(|s| s.emphasis && s.text == "emphasis"));
        assert!(blocks
            .iter()
            .flat_map(|b| &b.spans)
            .any(|s| s.code && s.text == "<tag>"));
        assert!(blocks
            .iter()
            .flat_map(|b| &b.spans)
            .any(|s| s.link.as_deref() == Some("https://example.org")));
        assert!(blocks.iter().any(|b| b.kind == PreviewBlockKind::Bullet));
        assert!(parse_preview_markdown("literal a < b").is_ok());
        assert!(parse_preview_markdown("![asset](https://example.org/image.png)").is_err());
        assert!(parse_preview_markdown("[bad](javascript:alert%281%29)").is_err());
        assert!(parse_preview_markdown(&"**x** ".repeat(MAX_PREVIEW_NODES)).is_err());
        assert!(parse_preview_markdown_with_budget("plain", 2, 256).is_err());
        assert!(parse_preview_markdown_with_budget("plain", 4096, 4).is_err());
    }
}

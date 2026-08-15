//! Compiled activation patterns shared by the manifest model and the query
//! scheduler (spec 8.11, 19.4).
//!
//! # Why this type exists rather than a bare `Regex`
//!
//! Two layers need the same compiled pattern. `crikey-plugin-model` resolves a
//! manifest's `[activation] patterns` into a policy, and
//! `crikey-input-scheduler` decides relevance from that policy on every
//! keystroke. Both hold it inside types that derive `PartialEq`/`Eq` —
//! `QueryPolicy` and `ActivationPolicy` — and [`regex::Regex`] implements
//! neither, because two distinct patterns can accept the same language and no
//! cheap test tells them apart. Equality here is equality of the *declaration*,
//! which is what a policy comparison actually asks: did the author write the
//! same manifest field.
//!
//! # Bounds
//!
//! A pattern is third-party text. The `regex` crate's automata guarantee
//! linear-time matching, so a pattern cannot make the scheduler hang the way a
//! backtracking engine would; what it can still do is compile to a large
//! program. [`MAX_PATTERN_BYTES`] bounds the declaration and
//! [`COMPILED_SIZE_LIMIT`] bounds the compiled form, so a manifest is refused
//! at parse time rather than after it has claimed the memory.

use std::fmt;

use regex::{Regex, RegexBuilder};

/// Largest accepted activation pattern, in bytes of source text.
pub const MAX_PATTERN_BYTES: usize = 512;

/// Largest accepted compiled program for one activation pattern, in bytes.
///
/// The `regex` default is 10 MiB, sized for a general-purpose engine. An
/// activation pattern decides whether one plugin sees one keystroke; anything
/// that does not fit here is a declaration the author should simplify, not a
/// cost the launcher should carry per plugin.
pub const COMPILED_SIZE_LIMIT: usize = 64 * 1024;

/// Why an activation pattern was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ActivationPatternError {
    /// The declaration was empty or only whitespace.
    #[error("an activation pattern must not be empty")]
    Empty,
    /// The declaration exceeded [`MAX_PATTERN_BYTES`].
    #[error("an activation pattern must be at most {MAX_PATTERN_BYTES} bytes, this one is {actual}")]
    TooLong { actual: usize },
    /// The pattern is not valid regular-expression syntax, or compiles to a
    /// program larger than [`COMPILED_SIZE_LIMIT`].
    #[error("the activation pattern `{pattern}` did not compile: {reason}")]
    Malformed { pattern: String, reason: String },
}

/// One compiled activation pattern from a plugin manifest.
///
/// Matching is unanchored, exactly as the `regex` crate defines it: a pattern
/// admits a query when it matches anywhere inside it. An author who wants the
/// whole query to match writes `^…$`, and one who wants case-insensitivity
/// writes `(?i)`. The host does not fold case here — unlike prefixes and
/// keywords, which are compared case-insensitively because they are literals
/// with no way to say otherwise.
#[derive(Debug, Clone)]
pub struct ActivationPattern {
    source: String,
    matcher: Regex,
}

impl ActivationPattern {
    /// Compiles one declared pattern, refusing anything outside the bounds
    /// above.
    pub fn new(pattern: &str) -> Result<Self, ActivationPatternError> {
        let source = pattern.trim();
        if source.is_empty() {
            return Err(ActivationPatternError::Empty);
        }
        if source.len() > MAX_PATTERN_BYTES {
            return Err(ActivationPatternError::TooLong { actual: source.len() });
        }
        let matcher = RegexBuilder::new(source)
            .size_limit(COMPILED_SIZE_LIMIT)
            .build()
            .map_err(|error| ActivationPatternError::Malformed {
                pattern: source.to_owned(),
                reason: error.to_string(),
            })?;
        Ok(Self {
            source: source.to_owned(),
            matcher,
        })
    }

    /// Retains a declaration that could not be compiled, as a pattern that
    /// admits nothing.
    ///
    /// The failure direction is the whole point. A manifest parsed by this
    /// workspace is refused before it reaches a policy, but a `Manifest` value
    /// built in code is not, and *dropping* its uncompilable declaration would
    /// leave a plugin whose author asked for gating with no gate at all —
    /// widening relevance to every keystroke. Keeping the declaration as a
    /// pattern that never matches fails closed instead, and `source` still
    /// names what the author wrote so a diagnostic can quote it.
    pub fn never_matching(source: &str) -> Self {
        Self {
            source: source.trim().to_owned(),
            // The complement of "any character at all": valid syntax, and no
            // subject can satisfy it.
            matcher: Regex::new(r"[^\s\S]").expect("the never-matching pattern is a constant"),
        }
    }

    /// The declaration as written, after trimming.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Whether `query` is admitted by this pattern.
    pub fn is_match(&self, query: &str) -> bool {
        self.matcher.is_match(query)
    }
}

impl PartialEq for ActivationPattern {
    fn eq(&self, other: &Self) -> bool {
        self.source == other.source
    }
}

impl Eq for ActivationPattern {}

impl fmt::Display for ActivationPattern {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pattern_matches_unanchored_and_keeps_its_source() {
        let pattern = ActivationPattern::new(r"\d{4}").expect("a valid pattern compiles");
        assert_eq!(pattern.source(), r"\d{4}");
        assert!(pattern.is_match("issue 1234"), "matching is unanchored");
        assert!(!pattern.is_match("issue 12"));
    }

    #[test]
    fn an_anchored_pattern_rejects_a_partial_query() {
        let pattern = ActivationPattern::new(r"^gh \d+$").expect("a valid pattern compiles");
        assert!(pattern.is_match("gh 42"));
        assert!(
            !pattern.is_match("check gh 42"),
            "`^` means the author asked for a whole-query match"
        );
    }

    #[test]
    fn equality_is_equality_of_the_declaration() {
        let written = ActivationPattern::new("^a+$").expect("compiles");
        let same = ActivationPattern::new("  ^a+$  ").expect("compiles");
        let equivalent_language = ActivationPattern::new("^aa*$").expect("compiles");
        assert_eq!(written, same, "trimming happens before the source is kept");
        assert_ne!(
            written, equivalent_language,
            "two spellings of one language are two declarations"
        );
    }

    #[test]
    fn malformed_empty_and_oversized_declarations_are_refused() {
        assert!(matches!(
            ActivationPattern::new("   "),
            Err(ActivationPatternError::Empty)
        ));
        assert!(matches!(
            ActivationPattern::new("("),
            Err(ActivationPatternError::Malformed { .. })
        ));
        let oversized = "a".repeat(MAX_PATTERN_BYTES + 1);
        assert!(matches!(
            ActivationPattern::new(&oversized),
            Err(ActivationPatternError::TooLong { .. })
        ));
    }

    #[test]
    fn a_never_matching_declaration_admits_nothing_but_keeps_its_source() {
        let retained = ActivationPattern::never_matching("(");
        assert_eq!(retained.source(), "(", "the declaration is quotable");
        for query in ["", "(", "anything at all", "\u{1F600}"] {
            assert!(
                !retained.is_match(query),
                "an uncompilable declaration must admit nothing, not everything"
            );
        }
    }

    #[test]
    fn a_pattern_that_compiles_too_large_is_refused_rather_than_allocated() {
        // Well-formed syntax, deliberately expensive to compile: ten thousand
        // repetitions of a Unicode word class. The size limit is the only thing
        // standing between a manifest and this allocation, so the assertion
        // reads the reason rather than accepting any `Malformed` — a pattern
        // rejected for *syntax* would prove nothing about the bound.
        let Err(ActivationPatternError::Malformed { reason, .. }) =
            ActivationPattern::new(r"(?:\w{100}){100}")
        else {
            panic!("a pattern over the compiled size limit must be refused at parse time");
        };
        assert!(
            reason.contains("size limit"),
            "the refusal must come from the compiled size bound, not from syntax: {reason}"
        );
    }
}

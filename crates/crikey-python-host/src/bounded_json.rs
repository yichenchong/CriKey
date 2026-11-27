//! Allocation-free grammar/shape admission, followed by a bounded projection.
//! Unknown additive members are traversed and charged but never materialized.
use serde_json::{Map, Value};

const MAX_ALLOC: usize = 64 * 1024 * 1024;
const ROOT: &[&str] = &[
    "id",
    "kind",
    "protocol_version",
    "capabilities",
    "items",
    "done",
    "generation",
    "state",
    "log",
    "error",
    "status",
    "detail",
    "identity",
    "content",
    "task_id",
    "declarations",
    "presentation",
    "failures",
    "code",
];
const TYPED: &[&str] = &[
    "sections",
    "scopes",
    "items",
    "schemas",
    "item_id",
    "action_id",
    "schema",
    "fields",
    "id",
    "label",
    "required",
    "placeholder",
    "guidance",
    "kind",
    "min_bytes",
    "max_bytes",
    "multiline",
    "default",
    "options",
    "section",
    "field_id",
    "code",
];
const ITEM: &[&str] = &[
    "stable_id",
    "label",
    "target",
    "category",
    "description",
    "icon_reference",
    "score_hint",
    "search_terms",
    "metadata",
    "actions",
    "argument_policy",
    "hit_policy",
];
const ACTION: &[&str] = &[
    "action_id",
    "label",
    "description",
    "icon_reference",
    "applicable_categories",
    "execution_policy",
];
const PREVIEW: &[&str] = &["text", "markdown", "image", "values"];
const IMAGE: &[&str] = &["width", "height", "rgba"];
const PREVIEW_VALUE: &[&str] = &["label", "value", "kind"];
const COMMAND: &[&str] = &["command_id", "label", "item_id", "action_id", "argument_policy"];
const ERROR: &[&str] = &["message", "traceback"];

pub(crate) fn decode(text: &str, sensitive: bool) -> Option<Map<String, Value>> {
    let mut scan = Parser {
        bytes: text.as_bytes(),
        at: 0,
        allocated: 0,
        content: 0,
        last_id: None,
    };
    scan.value(0, false, "", false, sensitive)?;
    scan.ws();
    if scan.at != scan.bytes.len() {
        return None;
    }
    let mut parser = Parser {
        bytes: text.as_bytes(),
        at: 0,
        allocated: 0,
        content: 0,
        last_id: None,
    };
    match parser.value(0, true, "", false, sensitive)? {
        Some(Value::Object(map)) => Some(map),
        _ => None,
    }
}

struct Parser<'a> {
    bytes: &'a [u8],
    at: usize,
    allocated: usize,
    content: usize,
    last_id: Option<(usize, usize, usize)>,
}
impl Parser<'_> {
    fn ws(&mut self) {
        while self
            .bytes
            .get(self.at)
            .is_some_and(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
        {
            self.at += 1;
        }
    }
    fn charge(&mut self, n: usize) -> Option<()> {
        self.allocated = self.allocated.checked_add(n)?;
        (self.allocated <= MAX_ALLOC).then_some(())
    }
    fn string(&mut self) -> Option<(usize, usize, usize)> {
        if self.bytes.get(self.at) != Some(&b'"') {
            return None;
        }
        let start = self.at;
        self.at += 1;
        let mut decoded = 0usize;
        loop {
            let byte = *self.bytes.get(self.at)?;
            self.at += 1;
            match byte {
                b'"' => {
                    self.charge(decoded.checked_add(24)?)?;
                    return Some((start, self.at, decoded));
                }
                0..=31 => return None,
                b'\\' => {
                    let escape = *self.bytes.get(self.at)?;
                    self.at += 1;
                    match escape {
                        b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => decoded += 1,
                        b'u' => {
                            let first = self.hex()?;
                            let cp = if (0xd800..=0xdbff).contains(&first) {
                                if self.bytes.get(self.at..self.at + 2)? != b"\\u" {
                                    return None;
                                }
                                self.at += 2;
                                let second = self.hex()?;
                                if !(0xdc00..=0xdfff).contains(&second) {
                                    return None;
                                }
                                0x10000 + ((first - 0xd800) << 10) + second - 0xdc00
                            } else {
                                if (0xdc00..=0xdfff).contains(&first) {
                                    return None;
                                }
                                first
                            };
                            decoded += char::from_u32(cp)?.len_utf8();
                        }
                        _ => return None,
                    }
                }
                _ => decoded += 1,
            }
        }
    }
    fn hex(&mut self) -> Option<u32> {
        let mut value = 0;
        for _ in 0..4 {
            value = value * 16 + char::from(*self.bytes.get(self.at)?).to_digit(16)?;
            self.at += 1;
        }
        Some(value)
    }
    fn matches(&self, span: (usize, usize, usize), key: &str) -> bool {
        // Match decoded ASCII keys without allocating, including escaped keys.
        let mut at = span.0 + 1;
        for wanted in key.bytes() {
            let Some(mut got) = self.bytes.get(at).copied() else {
                return false;
            };
            at += 1;
            if got == b'\\' {
                if self.bytes.get(at) != Some(&b'u') {
                    return false;
                }
                at += 1;
                let mut cp = 0u32;
                for _ in 0..4 {
                    let Some(d) = self.bytes.get(at).and_then(|b| char::from(*b).to_digit(16)) else {
                        return false;
                    };
                    cp = cp * 16 + d;
                    at += 1;
                }
                if cp > 127 {
                    return false;
                }
                got = cp as u8;
            }
            if got != wanted {
                return false;
            }
        }
        at + 1 == span.1
    }
    fn decoded_id(&self, span: (usize, usize, usize), out: &mut [u8; 256]) -> Option<usize> {
        let mut parser = Parser {
            bytes: self.bytes,
            at: span.0 + 1,
            allocated: 0,
            content: 0,
            last_id: None,
        };
        let mut length = 0;
        while parser.at < span.1 - 1 {
            let byte = self.bytes[parser.at];
            parser.at += 1;
            if byte != b'\\' {
                *out.get_mut(length)? = byte;
                length += 1;
                continue;
            }
            let escape = self.bytes[parser.at];
            parser.at += 1;
            if escape == b'u' {
                let first = parser.hex()?;
                let cp = if (0xd800..=0xdbff).contains(&first) {
                    parser.at += 2;
                    0x10000 + ((first - 0xd800) << 10) + parser.hex()? - 0xdc00
                } else {
                    first
                };
                let mut bytes = [0; 4];
                let text = char::from_u32(cp)?.encode_utf8(&mut bytes);
                out.get_mut(length..length + text.len())?
                    .copy_from_slice(text.as_bytes());
                length += text.len();
            } else {
                *out.get_mut(length)? = match escape {
                    b'b' => 8,
                    b'f' => 12,
                    b'n' => 10,
                    b'r' => 13,
                    b't' => 9,
                    other => other,
                };
                length += 1;
            }
        }
        Some(length)
    }
    fn value(
        &mut self,
        depth: usize,
        materialize: bool,
        context: &str,
        typed: bool,
        sensitive: bool,
    ) -> Option<Option<Value>> {
        if depth > 64 {
            return None;
        }
        self.ws();
        self.charge(48)?;
        if context.is_empty() {
            self.content = self.content.checked_add(48)?;
        }
        let allocation_start = self.content;
        match *self.bytes.get(self.at)? {
            b'{' => {
                self.at += 1;
                self.ws();
                let mut map = Map::new();
                let mut seen = 0u64;
                let mut object_id = None;
                let mut duplicate = false;
                let mut strict = typed;
                let mut password = false;
                let mut has_default = false;
                let mut typed_result = false;
                let mut result_id = None;
                let mut result_status = None;
                let mut result_code = None;
                if self.bytes.get(self.at) == Some(&b'}') {
                    self.at += 1;
                    return Some(materialize.then_some(Value::Object(map)));
                }
                loop {
                    self.ws();
                    let span = self.string()?;
                    // Include worst-case BTree node capacity before allocating a
                    // materialized member, even for discarded additive content.
                    self.charge(1024)?;
                    let opaque = matches!(context, "metadata" | "identity");
                    let known = if depth == 0 {
                        ROOT
                    } else if typed {
                        TYPED
                    } else {
                        match context {
                            "items" => ITEM,
                            "actions" => ACTION,
                            "content" => PREVIEW,
                            "image" => IMAGE,
                            "values" => PREVIEW_VALUE,
                            "declarations" => COMMAND,
                            "error" => ERROR,
                            _ => &[],
                        }
                    };
                    let index = known.iter().position(|key| self.matches(span, key));
                    if index.is_none() {
                        self.content = self.content.checked_add(span.2 + 24)?;
                    }
                    if !typed && context == "declarations" && index.is_none() {
                        return None;
                    }
                    let key = index.map(|i| known[i]).unwrap_or("");
                    if let Some(i) = index {
                        let bit = 1u64 << i;
                        duplicate |= seen & bit != 0;
                        seen |= bit;
                    }
                    strict |= matches!(key, "presentation" | "failures");
                    if strict && duplicate {
                        return None;
                    }
                    self.ws();
                    if self.bytes.get(self.at) != Some(&b':') {
                        return None;
                    }
                    self.at += 1;
                    self.ws();
                    let value_start = self.at;
                    if (key == "presentation" && self.bytes.get(value_start) != Some(&b'{'))
                        || (key == "failures" && self.bytes.get(value_start) != Some(&b'['))
                    {
                        return None;
                    }
                    if typed {
                        let shape = *self.bytes.get(value_start)?;
                        let valid = match key {
                            "sections" | "scopes" | "items" | "schemas" | "fields" | "options" => {
                                shape == b'['
                            }
                            "schema" => shape == b'{',
                            "required" | "multiline" => matches!(shape, b't' | b'f'),
                            "min_bytes" | "max_bytes" => shape.is_ascii_digit(),
                            "id" | "label" | "item_id" | "field_id" | "kind" | "code" => shape == b'"',
                            "action_id" | "section" | "placeholder" | "guidance" | "default" => {
                                matches!(shape, b'"' | b'n')
                            }
                            _ => true,
                        };
                        if !valid {
                            return None;
                        }
                    }
                    let child_typed =
                        (typed && index.is_some()) || key == "presentation" || key == "failures";
                    let suppressed = sensitive && matches!(key, "log" | "error" | "detail");
                    let keep = materialize && !suppressed && (index.is_some() || opaque);
                    let value = self.value(
                        depth + 1,
                        keep,
                        if opaque { context } else { key },
                        child_typed,
                        sensitive,
                    )?;
                    if depth == 0 {
                        let span = (value_start, self.at, 0);
                        match key {
                            "id" => result_id = Some(span),
                            "status" => result_status = Some(span),
                            "code" => result_code = Some(span),
                            _ => {}
                        }
                    }
                    if matches!(key, "id" | "field_id") && typed && self.bytes.get(value_start) == Some(&b'"')
                    {
                        let mut id_scan = Parser {
                            bytes: self.bytes,
                            at: value_start,
                            allocated: 0,
                            content: 0,
                            last_id: None,
                        };
                        object_id = Some(id_scan.string()?);
                    }
                    if key == "kind" && self.bytes.get(value_start) == Some(&b'"') {
                        let span = (value_start, self.at, 0);
                        if depth == 0 && self.matches(span, "typed_execute_result") {
                            strict = true;
                            typed_result = true;
                        }
                        if typed && self.matches(span, "password") {
                            password = true;
                        }
                    }
                    has_default |= key == "default";
                    if materialize && suppressed {
                        let replacement = match key {
                            "log" => Value::Array(Vec::new()),
                            "error" if self.bytes.get(value_start..self.at) == Some(b"null") => Value::Null,
                            "error" => serde_json::json!({"message":"plugin callback failed","traceback":""}),
                            _ => Value::String("plugin callback failed".into()),
                        };
                        map.insert(key.into(), replacement);
                    }
                    if let Some(value) = value {
                        let name: String = serde_json::from_slice(&self.bytes[span.0..span.1]).ok()?;
                        map.insert(name, value);
                    }
                    self.ws();
                    match self.bytes.get(self.at)? {
                        b'}' => {
                            self.at += 1;
                            break;
                        }
                        b',' => self.at += 1,
                        _ => return None,
                    }
                }
                if strict && duplicate {
                    return None;
                }
                if typed_result {
                    let id = result_id?;
                    std::str::from_utf8(&self.bytes[id.0..id.1])
                        .ok()?
                        .parse::<u64>()
                        .ok()?;
                    let status = result_status?;
                    if !["completed", "field_failures", "failed"]
                        .iter()
                        .any(|s| self.matches(status, s))
                    {
                        return None;
                    }
                    if self.matches(status, "failed") || result_code.is_some() {
                        let code = result_code?;
                        if ![
                            "unavailable",
                            "denied",
                            "cancelled",
                            "timeout",
                            "unsupported",
                            "failed",
                        ]
                        .iter()
                        .any(|s| self.matches(code, s))
                        {
                            return None;
                        }
                    }
                }
                if password && has_default {
                    return None;
                }
                if context == "schema" && self.content - allocation_start > 256 * 1024 {
                    return None;
                }
                self.last_id = object_id;
                Some(materialize.then_some(Value::Object(map)))
            }
            b'[' => {
                self.at += 1;
                self.ws();
                let mut values = Vec::new();
                let mut count = 0usize;
                let mut ids = if typed
                    && matches!(context, "fields" | "options" | "failures" | "sections" | "scopes")
                {
                    Some([(0, 0, 0); 256])
                } else {
                    None
                };
                let max = if typed {
                    match context {
                        "fields" | "failures" => 64,
                        "options" => 256,
                        "sections" | "scopes" => 128,
                        _ => 4096,
                    }
                } else {
                    2 * 1024 * 1024
                };
                if self.bytes.get(self.at) == Some(&b']') {
                    self.at += 1;
                    return Some(materialize.then_some(Value::Array(values)));
                }
                loop {
                    count += 1;
                    if count > max {
                        return None;
                    }
                    self.ws();
                    if typed
                        && matches!(
                            context,
                            "fields" | "options" | "sections" | "schemas" | "items" | "failures"
                        )
                        && self.bytes.get(self.at) != Some(&b'{')
                    {
                        return None;
                    }
                    self.last_id = None;
                    let value = self.value(depth + 1, materialize, context, typed, sensitive)?;
                    if let Some(ids) = ids.as_mut() {
                        let span = self.last_id?;
                        let mut current = [0; 256];
                        let len = self.decoded_id(span, &mut current)?;
                        if len == 0
                            || std::str::from_utf8(&current[..len])
                                .ok()?
                                .chars()
                                .any(char::is_control)
                        {
                            return None;
                        }
                        for prior in &ids[..count - 1] {
                            let mut previous = [0; 256];
                            let previous_len = self.decoded_id(*prior, &mut previous)?;
                            if current[..len] == previous[..previous_len] {
                                return None;
                            }
                        }
                        ids[count - 1] = span;
                    }
                    if let Some(value) = value {
                        values.push(value);
                    }
                    self.ws();
                    match self.bytes.get(self.at)? {
                        b']' => {
                            self.at += 1;
                            break;
                        }
                        b',' => self.at += 1,
                        _ => return None,
                    }
                }
                if typed {
                    let element = match context {
                        "fields" => std::mem::size_of::<crikey_core::structured_arguments::ArgumentField>(),
                        "options" => std::mem::size_of::<crikey_core::structured_arguments::DropdownOption>(),
                        _ => 0,
                    };
                    self.content = self.content.checked_add(count.checked_mul(element)?)?;
                }
                Some(materialize.then_some(Value::Array(values)))
            }
            b'"' => {
                let span = self.string()?;
                if typed
                    && context == "kind"
                    && !["text", "password", "dropdown"]
                        .iter()
                        .any(|s| self.matches(span, s))
                {
                    return None;
                }
                if typed
                    && context == "code"
                    && ![
                        "required",
                        "too_short",
                        "too_long",
                        "invalid_selection",
                        "rejected",
                    ]
                    .iter()
                    .any(|s| self.matches(span, s))
                {
                    return None;
                }
                if context != "kind" {
                    self.content = self.content.checked_add(span.2)?;
                }
                if typed && context == "scopes" {
                    self.last_id = Some(span);
                }
                if typed
                    && span.2
                        > if matches!(
                            context,
                            "id" | "item_id" | "action_id" | "field_id" | "section" | "scopes"
                        ) {
                            256
                        } else {
                            8192
                        }
                {
                    return None;
                }
                if materialize {
                    Some(Some(Value::String(
                        serde_json::from_slice(&self.bytes[span.0..span.1]).ok()?,
                    )))
                } else {
                    Some(None)
                }
            }
            _ => {
                let start = self.at;
                while self
                    .bytes
                    .get(self.at)
                    .is_some_and(|b| !b.is_ascii_whitespace() && !b",]}".contains(b))
                {
                    self.at += 1;
                }
                let token = self.bytes.get(start..self.at)?;
                if token.is_empty() || token.len() > 128 {
                    return None;
                }
                // Grammar admission does not construct a serde Value, even for scalars.
                if !matches!(token, b"null" | b"true" | b"false") {
                    let mut at = usize::from(token.first() == Some(&b'-'));
                    match token.get(at)? {
                        b'0' => at += 1,
                        b'1'..=b'9' => {
                            at += 1;
                            while token.get(at).is_some_and(u8::is_ascii_digit) {
                                at += 1;
                            }
                        }
                        _ => return None,
                    }
                    if token.get(at) == Some(&b'.') {
                        at += 1;
                        let begin = at;
                        while token.get(at).is_some_and(u8::is_ascii_digit) {
                            at += 1;
                        }
                        if at == begin {
                            return None;
                        }
                    }
                    if matches!(token.get(at), Some(b'e' | b'E')) {
                        at += 1;
                        if matches!(token.get(at), Some(b'+' | b'-')) {
                            at += 1;
                        }
                        let begin = at;
                        while token.get(at).is_some_and(u8::is_ascii_digit) {
                            at += 1;
                        }
                        if at == begin {
                            return None;
                        }
                    }
                    if at != token.len() {
                        return None;
                    }
                    if !std::str::from_utf8(token).ok()?.parse::<f64>().ok()?.is_finite() {
                        return None;
                    }
                }
                if typed
                    && matches!(context, "min_bytes" | "max_bytes")
                    && std::str::from_utf8(token).ok()?.parse::<u64>().ok()? > 8192
                {
                    return None;
                }
                if materialize {
                    Some(Some(serde_json::from_slice(token).ok()?))
                } else {
                    Some(None)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn grammar_and_typed_bounds() {
        assert!(decode(r#"{"kind":"catalog_batch","future":{"x":"\ud83d\ude00"}}"#, false).is_some());
        assert!(decode(r#"{"presentation":{"fields":[],"f\u0069elds":[]}}"#, false).is_none());
        assert!(decode(r#"{"future":"\ud800"}"#, false).is_none());
        assert!(decode(
            &format!("{{\"future\":{}0{}}}", "[".repeat(65), "]".repeat(65)),
            false
        )
        .is_none());
        assert_eq!(
            decode(r#"{"kind":"x","future":[1,2],"log":["secret"]}"#, true).unwrap()["log"],
            serde_json::json!([])
        );
    }
    #[test]
    fn typed_shapes_are_admitted_before_materialization() {
        let fields = (0..65)
            .map(|n| format!(r#"{{"id":"f{n}","label":"Field","kind":"text"}}"#))
            .collect::<Vec<_>>()
            .join(",");
        assert!(decode(
            &format!(r#"{{"presentation":{{"schemas":[{{"schema":{{"fields":[{fields}]}}}}]}}}}"#),
            false
        )
        .is_none());
        assert!(decode(r#"{"presentation":{"schemas":[{"schema":{"fields":[{"id":"a","label":"A","kind":"password","default":null}]}}]}}"#, false).is_none());
        assert!(decode(r#"{"presentation":{"schemas":[{"schema":{"fields":[{"id":"é","label":"A","kind":"text"},{"id":"\u00e9","label":"B","kind":"text"}]}}]}}"#, false).is_none());
        assert!(decode(
            r#"{"kind":"typed_execute_result","status":"completed","sta\u0074us":"completed"}"#,
            true
        )
        .is_none());
        assert_eq!(
            decode(r#"{"kind":"execute_result","status":"old","status":"ok"}"#, false).unwrap()["status"],
            "ok"
        );
        let unknown = "x".repeat(8192);
        let schema = format!(
            r#"{{"presentation":{{"schemas":[{{"schema":{{"unknown":[{}],"fields":[]}}}}]}}}}"#,
            (0..40)
                .map(|_| format!("\"{unknown}\""))
                .collect::<Vec<_>>()
                .join(",")
        );
        assert!(decode(&schema, false).is_none());
    }
}

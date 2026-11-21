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
    "descriptor",
    "route",
    "session",
    "navigation",
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
const DESCRIPTOR: &[&str] = &["title", "definitions", "view"];
const DEFINITION: &[&str] = &["item", "actionless_policy", "primary_action", "action_sections"];
const VIEW: &[&str] = &[
    "kind",
    "items",
    "sections",
    "scopes",
    "initial_selection",
    "initial_scope",
    "columns",
    "content",
    "target_item_id",
    "item_id",
    "action_id",
];
const MENU: &[&str] = &[
    "kind",
    "id",
    "label",
    "description",
    "icon_reference",
    "entries",
    "action_id",
    "schema",
    "shortcut",
    "destructive",
];
const ACTION_SECTION: &[&str] = &["id", "label", "entries"];
const MEMBERSHIP: &[&str] = &["item_id", "section", "scopes", "image"];
const SESSION: &[&str] = &["logical_view", "surface"];
const NAVIGATION: &[&str] = &["kind", "route"];

pub(crate) fn decode(text: &str, sensitive: bool) -> Option<Map<String, Value>> {
    let mut scan = Parser::new(text.as_bytes(), 0);
    scan.value(0, false, "", false, sensitive, false)?;
    scan.ws();
    if scan.at != scan.bytes.len() {
        return None;
    }
    let mut parser = Parser::new(text.as_bytes(), 0);
    match parser.value(0, true, "", false, sensitive, false)? {
        Some(Value::Object(map)) => Some(map),
        _ => None,
    }
}

type IdSpan = (usize, usize, usize);
struct ArrayAdmission<'a> {
    context: &'a str,
    ids: &'a mut [IdSpan],
}
struct Parser<'a> {
    bytes: &'a [u8],
    at: usize,
    allocated: usize,
    content: usize,
    last_id: Option<(usize, usize, usize)>,
    menu_depth: usize,
    menu_count: usize,
    last_array_len: usize,
    menu_names: [IdSpan; 256],
    menu_name_count: usize,
    menu_targets: [IdSpan; 256],
    menu_target_count: usize,
}
impl<'a> Parser<'a> {
    fn new(bytes: &'a [u8], at: usize) -> Self {
        Self {
            bytes,
            at,
            allocated: 0,
            content: 0,
            last_id: None,
            menu_depth: 0,
            menu_count: 0,
            last_array_len: 0,
            menu_names: [(0, 0, 0); 256],
            menu_name_count: 0,
            menu_targets: [(0, 0, 0); 256],
            menu_target_count: 0,
        }
    }
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
        let mut at = span.0 + 1;
        let hex = |at: &mut usize| {
            let mut value = 0u32;
            for _ in 0..4 {
                value = value * 16 + char::from(*self.bytes.get(*at)?).to_digit(16)?;
                *at += 1;
            }
            Some(value)
        };
        let mut length = 0;
        while at < span.1 - 1 {
            let byte = self.bytes[at];
            at += 1;
            if byte != b'\\' {
                *out.get_mut(length)? = byte;
                length += 1;
                continue;
            }
            let escape = self.bytes[at];
            at += 1;
            if escape == b'u' {
                let first = hex(&mut at)?;
                let cp = if (0xd800..=0xdbff).contains(&first) {
                    at += 2;
                    0x10000 + ((first - 0xd800) << 10) + hex(&mut at)? - 0xdc00
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
    fn unique_id(&self, span: IdSpan, previous: &[IdSpan], nullable: bool) -> Option<usize> {
        let identity = |span: IdSpan, out: &mut [u8; 256]| {
            if nullable && self.bytes.get(span.0..span.1) == Some(b"null") {
                return Some(0);
            }
            if self.bytes.get(span.0) != Some(&b'"') {
                return None;
            }
            let length = self.decoded_id(span, out)?;
            if length == 0
                || std::str::from_utf8(&out[..length])
                    .ok()?
                    .chars()
                    .any(char::is_control)
            {
                return None;
            }
            Some(length)
        };
        let mut current = [0; 256];
        let length = identity(span, &mut current)?;
        use std::hash::Hasher;
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        hasher.write(&current[..length]);
        let hash = hasher.finish() as usize;
        for prior in previous {
            if prior.2 != hash {
                continue;
            }
            let mut other = [0; 256];
            let other_length = identity(*prior, &mut other)?;
            if current[..length] == other[..other_length] {
                return None;
            }
        }
        Some(hash)
    }

    // Only definitions and collection memberships need the large identity table.
    // Keeping it in this nonrecursive frame avoids multiplying it by JSON depth.
    #[inline(never)]
    fn large_array(
        &mut self,
        depth: usize,
        materialize: bool,
        context: &str,
        typed: bool,
        sensitive: bool,
        route: bool,
    ) -> Option<Option<Value>> {
        self.array(
            depth,
            materialize,
            ArrayAdmission {
                context,
                ids: &mut [(0, 0, 0); 4096],
            },
            typed,
            sensitive,
            route,
        )
    }

    fn array(
        &mut self,
        depth: usize,
        materialize: bool,
        admission: ArrayAdmission<'_>,
        typed: bool,
        sensitive: bool,
        route: bool,
    ) -> Option<Option<Value>> {
        let ArrayAdmission { context, ids } = admission;
        self.at += 1;
        if route && context == "entries" {
            self.menu_depth += 1;
            if self.menu_depth > 4 {
                return None;
            }
        }
        self.ws();
        let rgba = route && context == "rgba";
        let mut values = if rgba {
            // Pixel scalars are admitted as u8 below. Exact slot capacity avoids
            // geometric JSON Value amplification while retaining the wire format.
            let end = self.bytes[self.at..].iter().position(|b| *b == b']')? + self.at;
            let count = if end == self.at {
                0
            } else {
                self.bytes[self.at..end].iter().filter(|b| **b == b',').count() + 1
            };
            if count > 1024 * 1024 {
                return None;
            }
            self.charge(count.checked_mul(std::mem::size_of::<Value>())?)?;
            if materialize {
                Vec::with_capacity(count)
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };
        let track = (typed && matches!(context, "fields" | "options" | "failures" | "sections" | "scopes"))
            || (route
                && matches!(
                    context,
                    "definitions" | "items" | "sections" | "scopes" | "actions" | "action_sections"
                ));
        let max = if route {
            match context {
                "definitions" | "items" => 4096,
                "sections"
                | "scopes"
                | "action_sections"
                | "values"
                | "search_terms"
                | "applicable_categories" => 128,
                "actions" | "entries" => 256,
                "rgba" => 1024 * 1024,
                _ => 4096,
            }
        } else if typed {
            match context {
                "fields" | "failures" => 64,
                "options" => 256,
                "sections" | "scopes" => 128,
                _ => 4096,
            }
        } else {
            2 * 1024 * 1024
        };
        let mut count = 0;
        if self.bytes.get(self.at) != Some(&b']') {
            loop {
                count += 1;
                if count > max {
                    return None;
                }
                self.ws();
                if rgba && !self.bytes.get(self.at)?.is_ascii_digit() {
                    return None;
                }
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
                let value = self.value(depth + 1, materialize, context, typed, sensitive, route)?;
                if track {
                    let mut span = self.last_id?;
                    span.2 = self.unique_id(span, ids.get(..count - 1)?, false)?;
                    *ids.get_mut(count - 1)? = span;
                }
                if let Some(value) = value {
                    values.push(value);
                }
                self.ws();
                match self.bytes.get(self.at)? {
                    b']' => break,
                    b',' => self.at += 1,
                    _ => return None,
                }
            }
        }
        self.at += 1;
        self.last_array_len = count;
        if route && context == "entries" {
            self.menu_depth -= 1;
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

    fn value(
        &mut self,
        depth: usize,
        materialize: bool,
        context: &str,
        typed: bool,
        sensitive: bool,
        route: bool,
    ) -> Option<Option<Value>> {
        if depth > 64 {
            return None;
        }
        self.ws();
        // RGBA array slots are charged explicitly, at exact capacity, by array().
        self.charge(
            if route && context == "rgba" && self.bytes.get(self.at) != Some(&b'[') {
                16
            } else {
                48
            },
        )?;
        if context.is_empty() {
            self.content = self.content.checked_add(48)?;
        }
        let allocation_start = self.content;
        match *self.bytes.get(self.at)? {
            b'{' => {
                self.at += 1;
                if route && context == "definitions" {
                    self.menu_count = 0;
                    self.menu_name_count = 0;
                    self.menu_target_count = 0;
                }
                if route && context == "entries" {
                    self.menu_count += 1;
                    if self.menu_count > 256 {
                        return None;
                    }
                }
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
                let mut members = 0usize;
                let mut image_width = None;
                let mut image_height = None;
                let mut image_pixels = None;
                let mut menu_kind = None;
                let mut menu_target = None;
                if self.bytes.get(self.at) == Some(&b'}') {
                    self.at += 1;
                    if route && context == "image" {
                        return None;
                    }
                    return Some(materialize.then_some(Value::Object(map)));
                }
                loop {
                    members += 1;
                    if route && context == "metadata" && members > 128 {
                        return None;
                    }
                    self.ws();
                    let span = self.string()?;
                    if route && context == "metadata" && span.2 > 8192 {
                        return None;
                    }
                    // Include worst-case BTree node capacity before allocating a
                    // materialized member, even for discarded additive content.
                    self.charge(1024)?;
                    let opaque = matches!(context, "metadata" | "identity");
                    let known = if depth == 0 {
                        ROOT
                    } else if route && context != "schema" {
                        match context {
                            "descriptor" => DESCRIPTOR,
                            "definitions" => DEFINITION,
                            "view" => VIEW,
                            "action_sections" => ACTION_SECTION,
                            "entries" => MENU,
                            "item" => ITEM,
                            "actions" => ACTION,
                            "items" => MEMBERSHIP,
                            "sections" | "scopes" => &["id", "label"],
                            "content" => PREVIEW,
                            "image" => IMAGE,
                            "values" => PREVIEW_VALUE,
                            "session" => SESSION,
                            "navigation" => NAVIGATION,
                            _ => &[],
                        }
                    } else if typed {
                        TYPED
                    } else {
                        match context {
                            "items" | "item" => ITEM,
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
                    strict |= route
                        || matches!(
                            key,
                            "presentation" | "failures" | "descriptor" | "session" | "navigation"
                        );
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
                        (typed && index.is_some()) || matches!(key, "presentation" | "failures" | "schema");
                    let child_route = (route && (index.is_some() || opaque) && key != "schema")
                        || matches!(key, "descriptor" | "session" | "navigation" | "route");
                    let suppressed = sensitive && matches!(key, "log" | "error" | "detail");
                    let keep = materialize && !suppressed && (index.is_some() || opaque);
                    let value = self.value(
                        depth + 1,
                        keep,
                        if opaque { context } else { key },
                        child_typed,
                        sensitive,
                        child_route,
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
                    if ((typed && matches!(key, "id" | "field_id"))
                        || (route
                            && (matches!(key, "id" | "stable_id" | "item_id")
                                || (context == "actions" && key == "action_id"))))
                        && self.bytes.get(value_start) == Some(&b'"')
                    {
                        object_id = Some((value_start, self.at, 0));
                    }
                    if route && context == "definitions" && key == "item" {
                        object_id = self.last_id;
                    }
                    if route && context == "entries" {
                        if key == "kind" {
                            menu_kind = Some((value_start, self.at, 0));
                        }
                        if key == "action_id" {
                            menu_target = Some((value_start, self.at, 0));
                        }
                    }
                    if route && context == "image" {
                        match key {
                            "width" | "height" => {
                                let dimension = std::str::from_utf8(&self.bytes[value_start..self.at])
                                    .ok()?
                                    .parse::<u32>()
                                    .ok()?;
                                if dimension == 0 || dimension > 1024 {
                                    return None;
                                }
                                if key == "width" {
                                    image_width = Some(dimension);
                                } else {
                                    image_height = Some(dimension);
                                }
                            }
                            "rgba" => {
                                if self.bytes.get(value_start) != Some(&b'[') {
                                    return None;
                                }
                                image_pixels = Some(self.last_array_len);
                            }
                            _ => {}
                        }
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
                    if !["completed", "field_failures", "failed", "navigate"]
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
                if route && context == "image" {
                    let expected = image_width?.checked_mul(image_height?)?.checked_mul(4)? as usize;
                    if expected > 1024 * 1024 || image_pixels? != expected {
                        return None;
                    }
                }
                if route && context == "entries" {
                    let kind = menu_kind?;
                    if self.matches(kind, "submenu") {
                        let mut span = object_id?;
                        span.2 = self.unique_id(span, &self.menu_names[..self.menu_name_count], false)?;
                        *self.menu_names.get_mut(self.menu_name_count)? = span;
                        self.menu_name_count += 1;
                    } else if self.matches(kind, "action") {
                        let mut span = menu_target?;
                        span.2 = self.unique_id(span, &self.menu_targets[..self.menu_target_count], true)?;
                        *self.menu_targets.get_mut(self.menu_target_count)? = span;
                        self.menu_target_count += 1;
                    } else {
                        return None;
                    }
                }
                self.last_id = object_id;
                Some(materialize.then_some(Value::Object(map)))
            }
            b'[' => {
                if route && matches!(context, "definitions" | "items") {
                    self.large_array(depth, materialize, context, typed, sensitive, route)
                } else {
                    self.array(
                        depth,
                        materialize,
                        ArrayAdmission {
                            context,
                            ids: &mut [(0, 0, 0); 256],
                        },
                        typed,
                        sensitive,
                        route,
                    )
                }
            }
            b'"' => {
                let span = self.string()?;
                if route
                    && span.2
                        > if context == "text" {
                            256 * 1024
                        } else if matches!(
                            context,
                            "id" | "item_id"
                                | "action_id"
                                | "stable_id"
                                | "route"
                                | "primary_action"
                                | "initial_selection"
                                | "initial_scope"
                                | "target_item_id"
                                | "section"
                                | "scopes"
                        ) {
                            256
                        } else {
                            8192
                        }
                {
                    return None;
                }
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
                if (typed || route) && context == "scopes" {
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
                if route && context == "rgba" {
                    std::str::from_utf8(token).ok()?.parse::<u8>().ok()?;
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

#[cfg(test)]
mod standard_view_bounds {
    use super::decode;
    #[test]
    fn projects_only_context_known_members_and_preserves_schema() {
        let frame=decode(r#"{"id":2,"kind":"structured_view_result","status":"content","route":"route","session":{"logical_view":1,"surface":2},"descriptor":{"title":"View","unknown":{"items":[{"evil":"ignored"}]},"definitions":[{"item":{"stable_id":"local","label":"Local","target":"local","metadata":{"kept":"yes"},"actions":[]},"actionless_policy":"plugin","primary_action":null,"action_sections":[{"id":"main","label":"Main","entries":[{"kind":"action","action_id":null,"schema":{"fields":[{"id":"password","label":"Password","kind":"password","max_bytes":128}]},"destructive":false}]}]}],"view":{"kind":"form","item_id":"local","action_id":null}}}"#,false).unwrap();
        assert!(frame["descriptor"].get("unknown").is_none());
        assert_eq!(
            frame["descriptor"]["definitions"][0]["item"]["metadata"]["kept"],
            "yes"
        );
        assert_eq!(
            frame["descriptor"]["definitions"][0]["action_sections"][0]["entries"][0]["schema"]["fields"][0]
                ["kind"],
            "password"
        );
    }
    #[test]
    fn refuses_duplicate_members_menu_depth_and_definition_counts_before_projection() {
        assert!(decode(
            r#"{"kind":"structured_view_result","descriptor":{"title":"one","title":"two"}}"#,
            false
        )
        .is_none());
        let mut entry = r#"{"kind":"action","action_id":null}"#.to_owned();
        for _ in 0..4 {
            entry = format!(r#"{{"kind":"submenu","id":"menu","entries":[{entry}]}}"#);
        }
        assert!(decode(
            &format!(
                r#"{{"descriptor":{{"definitions":[{{"action_sections":[{{"entries":[{entry}]}}]}}]}}}}"#
            ),
            false
        )
        .is_none());
        let definitions = (0..4097)
            .map(|i| format!(r#"{{"item":{{"stable_id":"id{i}"}}}}"#))
            .collect::<Vec<_>>()
            .join(",");
        assert!(decode(
            &format!(r#"{{"descriptor":{{"definitions":[{definitions}]}}}}"#),
            false
        )
        .is_none());
    }
    #[test]
    fn image_dimensions_and_integer_pixels_fail_in_allocation_free_pass() {
        let excessive = std::iter::repeat_n("0", 1024 * 1024)
            .collect::<Vec<_>>()
            .join(",");
        for pixels in [
            excessive,
            "256,0,0,0".into(),
            "0.0,0,0,0".into(),
            "-1,0,0,0".into(),
            "true,0,0,0".into(),
        ] {
            let text = format!(
                r#"{{"descriptor":{{"view":{{"content":{{"image":{{"rgba":[{pixels}],"height":1,"width":1}}}}}}}}}}"#
            );
            let mut scan = super::Parser::new(text.as_bytes(), 0);
            assert!(scan.value(0, false, "", false, false, false).is_none());
        }
        let text =
            r#"{"descriptor":{"view":{"content":{"image":{"rgba":[0,1,254,255],"height":1,"width":1}}}}}"#;
        assert!(super::Parser::new(text.as_bytes(), 0)
            .value(0, false, "", false, false, false)
            .is_some());
    }

    #[test]
    fn decoded_route_ids_and_cross_section_menu_names_are_unique_before_projection() {
        for text in [
            r#"{"descriptor":{"definitions":[{"item":{"stable_id":"same"}},{"item":{"stable_id":"\u0073ame"}}]}}"#,
            r#"{"descriptor":{"view":{"items":[{"item_id":"same"},{"item_id":"same"}]}}}"#,
            r#"{"descriptor":{"view":{"sections":[{"id":"same"},{"id":"same"}]}}}"#,
            r#"{"descriptor":{"view":{"scopes":[{"id":"same"},{"id":"same"}]}}}"#,
            r#"{"descriptor":{"definitions":[{"item":{"stable_id":"local","actions":[{"action_id":"same"},{"action_id":"same"}]}}]}}"#,
            r#"{"descriptor":{"definitions":[{"item":{"stable_id":"local"},"action_sections":[{"id":"one","entries":[{"kind":"submenu","id":"same","entries":[]}]},{"id":"two","entries":[{"kind":"submenu","id":"same","entries":[]}]}]}]}}"#,
            r#"{"descriptor":{"definitions":[{"item":{"stable_id":"local"},"action_sections":[{"id":"one","entries":[{"kind":"action","action_id":null}]},{"id":"two","entries":[{"kind":"action","action_id":null}]}]}]}}"#,
        ] {
            let mut scan = super::Parser::new(text.as_bytes(), 0);
            assert!(scan.value(0, false, "", false, false, false).is_none(), "{text}");
        }
        let definitions = (0..4096)
            .map(|i| format!(r#"{{"item":{{"stable_id":"id{i}"}}}}"#))
            .collect::<Vec<_>>()
            .join(",");
        let text = format!(r#"{{"descriptor":{{"definitions":[{definitions}]}}}}"#);
        assert!(super::Parser::new(text.as_bytes(), 0)
            .value(0, false, "", false, false, false)
            .is_some());
    }
}

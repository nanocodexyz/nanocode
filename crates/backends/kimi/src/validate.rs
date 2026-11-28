//! Tool-argument validation as Kimi Code runs it: AJV (`strict: false`, `allErrors: true`, no coercion or
//! defaults) against the tool's declared JSON Schema, every error formatted and joined with "; ".
//! Covers the keywords the built-in contracts and ordinary MCP schemas use; unknown keywords are ignored,
//! as AJV does with `strict: false`.

use serde_json::Value;

/// A JSON Schema with its object keys in declaration order: AJV reports property errors in the order
/// the schema declares the properties, and `serde_json::Value` (without `preserve_order`) sorts them.
#[derive(Clone, Debug)]
pub enum Node {
    Map(Vec<(String, Self)>),
    List(Vec<Self>),
    Leaf(Value),
}

impl Node {
    pub fn parse(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    pub fn get(&self, key: &str) -> Option<&Self> {
        match self {
            Self::Map(entries) => entries
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, node)| node),
            _ => None,
        }
    }

    fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    fn value(&self) -> Value {
        match self {
            Self::Map(entries) => Value::Object(
                entries
                    .iter()
                    .map(|(key, node)| (key.clone(), node.value()))
                    .collect(),
            ),
            Self::List(items) => Value::Array(items.iter().map(Self::value).collect()),
            Self::Leaf(value) => value.clone(),
        }
    }

    fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Leaf(value) => value.as_f64(),
            _ => None,
        }
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            Self::Leaf(value) => value.as_str(),
            _ => None,
        }
    }

    fn list(&self) -> Option<&[Self]> {
        match self {
            Self::List(items) => Some(items),
            _ => None,
        }
    }
}

impl<'de> serde::Deserialize<'de> for Node {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Node;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("JSON")
            }
            fn visit_bool<E>(self, value: bool) -> Result<Node, E> {
                Ok(Node::Leaf(Value::Bool(value)))
            }
            fn visit_i64<E>(self, value: i64) -> Result<Node, E> {
                Ok(Node::Leaf(Value::from(value)))
            }
            fn visit_u64<E>(self, value: u64) -> Result<Node, E> {
                Ok(Node::Leaf(Value::from(value)))
            }
            fn visit_f64<E>(self, value: f64) -> Result<Node, E> {
                Ok(Node::Leaf(Value::from(value)))
            }
            fn visit_str<E>(self, value: &str) -> Result<Node, E> {
                Ok(Node::Leaf(Value::String(value.to_owned())))
            }
            fn visit_unit<E>(self) -> Result<Node, E> {
                Ok(Node::Leaf(Value::Null))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Node, A::Error> {
                let mut items = Vec::new();
                while let Some(item) = seq.next_element()? {
                    items.push(item);
                }
                Ok(Node::List(items))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Node, A::Error> {
                let mut entries = Vec::new();
                while let Some(entry) = map.next_entry::<String, Node>()? {
                    entries.push(entry);
                }
                Ok(Node::Map(entries))
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

pub fn errors(schema: &Node, args: &Value) -> Option<String> {
    let mut found = Vec::new();
    check(schema, args, "", &mut found);
    (!found.is_empty()).then(|| found.join("; "))
}

fn format_number(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e21 {
        format!("{value:.0}")
    } else {
        value.to_string()
    }
}

fn is_type(kind: &str, value: &Value) -> bool {
    match kind {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        "number" => value.is_number(),
        "integer" => value.as_f64().is_some_and(|number| number.fract() == 0.0),
        _ => true,
    }
}

const fn group_of(value: &Value) -> Option<&'static str> {
    match value {
        Value::Number(_) => Some("number"),
        Value::String(_) => Some("string"),
        Value::Array(_) => Some("array"),
        Value::Object(_) => Some("object"),
        _ => None,
    }
}

const GROUPS: [(&str, &[&str]); 4] = [
    (
        "number",
        &[
            "maximum",
            "minimum",
            "exclusiveMaximum",
            "exclusiveMinimum",
            "multipleOf",
        ],
    ),
    ("string", &["maxLength", "minLength", "pattern", "format"]),
    (
        "array",
        &[
            "maxItems",
            "minItems",
            "uniqueItems",
            "items",
            "prefixItems",
            "contains",
        ],
    ),
    (
        "object",
        &[
            "maxProperties",
            "minProperties",
            "required",
            "additionalProperties",
            "properties",
        ],
    ),
];

fn push(found: &mut Vec<String>, path: &str, message: impl AsRef<str>) {
    match path.is_empty() {
        true => found.push(message.as_ref().to_owned()),
        false => found.push(format!("{path} {}", message.as_ref())),
    }
}

fn check(schema: &Node, data: &Value, path: &str, found: &mut Vec<String>) {
    if !matches!(schema, Node::Map(_)) {
        if matches!(schema, Node::Leaf(Value::Bool(false))) {
            push(found, path, "boolean schema is false");
        }
        return;
    }
    let types = match schema.get("type") {
        Some(Node::Leaf(Value::String(kind))) => vec![kind.as_str()],
        Some(Node::List(kinds)) => kinds.iter().filter_map(Node::as_str).collect(),
        _ => Vec::new(),
    };
    let grouped = |kind: &str| {
        GROUPS
            .iter()
            .find(|(group, _)| *group == kind)
            .is_some_and(|(_, keywords)| keywords.iter().any(|key| schema.contains_key(key)))
    };
    let type_message = format!("must be {}", types.join(","));
    let upfront = !types.is_empty() && !(types.len() == 1 && grouped(types[0]));
    if upfront && !types.iter().any(|kind| is_type(kind, data)) {
        push(found, path, &type_message);
    }
    untyped(schema, data, path, found);
    for (group, _) in GROUPS {
        if !grouped(group) {
            continue;
        }
        if group_of(data) == Some(group) {
            typed(group, schema, data, path, found);
        } else if !upfront && types.len() == 1 && types[0] == group {
            push(found, path, &type_message);
        }
    }
}

fn untyped(schema: &Node, data: &Value, path: &str, found: &mut Vec<String>) {
    if let Some(constant) = schema.get("const")
        && !equal(&constant.value(), data)
    {
        push(found, path, "must be equal to constant");
    }
    if let Some(allowed) = schema.get("enum").and_then(Node::list)
        && !allowed.iter().any(|item| equal(&item.value(), data))
    {
        push(found, path, "must be equal to one of the allowed values");
    }
    if let Some(not) = schema.get("not") {
        let mut inner = Vec::new();
        check(not, data, path, &mut inner);
        if inner.is_empty() {
            push(found, path, "must NOT be valid");
        }
    }
    if let Some(branches) = schema.get("anyOf").and_then(Node::list) {
        let mut collected = Vec::new();
        let mut passed = false;
        for branch in branches {
            let mut inner = Vec::new();
            check(branch, data, path, &mut inner);
            passed |= inner.is_empty();
            collected.extend(inner);
        }
        if !passed {
            found.extend(collected);
            push(found, path, "must match a schema in anyOf");
        }
    }
    if let Some(branches) = schema.get("oneOf").and_then(Node::list) {
        let mut collected = Vec::new();
        let mut passing = 0;
        for branch in branches {
            let mut inner = Vec::new();
            check(branch, data, path, &mut inner);
            passing += usize::from(inner.is_empty());
            collected.extend(inner);
        }
        if passing != 1 {
            if passing == 0 {
                found.extend(collected);
            }
            push(found, path, "must match exactly one schema in oneOf");
        }
    }
    if let Some(branches) = schema.get("allOf").and_then(Node::list) {
        for branch in branches {
            check(branch, data, path, found);
        }
    }
}

fn typed(group: &str, schema: &Node, data: &Value, path: &str, found: &mut Vec<String>) {
    let limit = |key: &str| schema.get(key).and_then(Node::as_f64);
    match group {
        "number" => {
            let value = data.as_f64().unwrap_or_default();
            for (key, operator, fails) in [
                (
                    "maximum",
                    "<=",
                    (|v: f64, l: f64| v > l) as fn(f64, f64) -> bool,
                ),
                ("minimum", ">=", |v, l| v < l),
                ("exclusiveMaximum", "<", |v, l| v >= l),
                ("exclusiveMinimum", ">", |v, l| v <= l),
            ] {
                if let Some(bound) = limit(key)
                    && fails(value, bound)
                {
                    push(
                        found,
                        path,
                        format!("must be {operator} {}", format_number(bound)),
                    );
                }
            }
            if let Some(step) = limit("multipleOf")
                && step != 0.0
                && (value / step).fract() != 0.0
            {
                push(
                    found,
                    path,
                    format!("must be multiple of {}", format_number(step)),
                );
            }
        }
        "string" => {
            let length = data.as_str().map_or(0, |text| text.encode_utf16().count()) as f64;
            if let Some(bound) = limit("maxLength")
                && length > bound
            {
                push(
                    found,
                    path,
                    format!(
                        "must NOT have more than {} characters",
                        format_number(bound)
                    ),
                );
            }
            if let Some(bound) = limit("minLength")
                && length < bound
            {
                push(
                    found,
                    path,
                    format!(
                        "must NOT have fewer than {} characters",
                        format_number(bound)
                    ),
                );
            }
        }
        "array" => {
            let items = data.as_array().map(Vec::as_slice).unwrap_or_default();
            let count = items.len() as f64;
            if let Some(bound) = limit("maxItems")
                && count > bound
            {
                push(
                    found,
                    path,
                    format!("must NOT have more than {} items", format_number(bound)),
                );
            }
            if let Some(bound) = limit("minItems")
                && count < bound
            {
                push(
                    found,
                    path,
                    format!("must NOT have fewer than {} items", format_number(bound)),
                );
            }
            if matches!(
                schema.get("uniqueItems"),
                Some(Node::Leaf(Value::Bool(true)))
            ) {
                'unique: for later in (0..items.len()).rev() {
                    for earlier in (0..later).rev() {
                        if equal(&items[earlier], &items[later]) {
                            push(
                                found,
                                path,
                                format!(
                                    "must NOT have duplicate items (items ## {earlier} and {later} are identical)"
                                ),
                            );
                            break 'unique;
                        }
                    }
                }
            }
            if let Some(item) = schema
                .get("items")
                .filter(|item| !matches!(item, Node::List(_)))
            {
                for (index, value) in items.iter().enumerate() {
                    check(item, value, &format!("{path}/{index}"), found);
                }
            }
        }
        "object" => {
            let object = data.as_object().cloned().unwrap_or_default();
            if let Some(required) = schema.get("required").and_then(Node::list) {
                for name in required.iter().filter_map(Node::as_str) {
                    if !object.contains_key(name) {
                        found.push(format!("must have required property '{name}'"));
                    }
                }
            }
            let properties = match schema.get("properties") {
                Some(Node::Map(entries)) => entries.as_slice(),
                _ => &[],
            };
            let known = |key: &str| properties.iter().any(|(name, _)| name == key);
            match schema.get("additionalProperties") {
                Some(Node::Leaf(Value::Bool(false))) => {
                    for key in object.keys() {
                        if !known(key) {
                            found.push(format!("must NOT have additional property '{key}'"));
                        }
                    }
                }
                Some(extra @ Node::Map(_)) => {
                    for (key, value) in &object {
                        if !known(key) {
                            check(extra, value, &format!("{path}/{}", pointer(key)), found);
                        }
                    }
                }
                _ => {}
            }
            for (key, property) in properties {
                if let Some(value) = object.get(key) {
                    check(property, value, &format!("{path}/{}", pointer(key)), found);
                }
            }
        }
        _ => {}
    }
}

fn pointer(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

fn equal(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(a), Value::Number(b)) => a.as_f64() == b.as_f64(),
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| equal(a, b))
        }
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(key, value)| b.get(key).is_some_and(|other| equal(value, other)))
        }
        (a, b) => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::{Node, errors};
    use serde_json::{Value, json};

    fn tool(name: &str) -> Node {
        let tools = Node::parse(crate::tools::CONTRACTS).unwrap();
        let Node::List(tools) = tools else { panic!() };
        tools
            .into_iter()
            .find(|tool| {
                tool.get("function")
                    .and_then(|function| function.get("name"))
                    .and_then(Node::as_str)
                    == Some(name)
            })
            .and_then(|tool| {
                tool.get("function")
                    .and_then(|function| function.get("parameters"))
                    .cloned()
            })
            .unwrap()
    }
    #[test]
    fn matches_kimi_code_messages() {
        let cases = [
            ("Read", json!({}), "must have required property 'path'"),
            (
                "Read",
                json!({"extra": 1}),
                "must have required property 'path'; must NOT have additional property 'extra'",
            ),
            (
                "Read",
                json!({"path": 5, "extra": 1}),
                "must NOT have additional property 'extra'; /path must be string",
            ),
            (
                "Read",
                json!({"path": 5, "line_offset": 0}),
                "/path must be string; /line_offset must be >= 1; /line_offset must be < 0; /line_offset must match a schema in anyOf",
            ),
            (
                "Read",
                json!({"path": "a", "line_offset": "x"}),
                "/line_offset must be integer; /line_offset must be integer; /line_offset must match a schema in anyOf",
            ),
            (
                "Read",
                json!({"path": "a", "line_offset": 1.5}),
                "/line_offset must be integer; /line_offset must be integer; /line_offset must be < 0; /line_offset must match a schema in anyOf",
            ),
            (
                "Write",
                json!({"path": "x.txt"}),
                "must have required property 'content'",
            ),
            ("Write", json!([1]), "must be object"),
            (
                "TodoList",
                json!({"todos": [{"title": "", "status": "pending"}]}),
                "/todos/0/title must NOT have fewer than 1 characters",
            ),
            ("TodoList", json!({"todos": "x"}), "/todos must be array"),
            (
                "TodoList",
                json!({"todos": [{"title": "a"}]}),
                "must have required property 'status'",
            ),
            (
                "Edit",
                json!({"path": "README.md", "old_string": "hello", "new_string": "bye", "extra": 1}),
                "must NOT have additional property 'extra'",
            ),
            (
                "Bash",
                json!({"command": "echo hi", "timeout": 0}),
                "/timeout must be > 0",
            ),
            (
                "Bash",
                json!({"command": "", "timeout": 400}),
                "/command must NOT have fewer than 1 characters",
            ),
            (
                "Grep",
                json!({"pattern": "x", "output_mode": "bad"}),
                "/output_mode must be equal to one of the allowed values",
            ),
            (
                "Glob",
                json!({"pattern": "x", "head_limit": -1}),
                "/head_limit must be >= 0",
            ),
            ("WaitFor", json!({"timeout": 91}), "/timeout must be <= 90"),
        ];
        for (name, args, expected) in cases {
            assert_eq!(
                errors(&tool(name), &args).as_deref(),
                Some(expected),
                "{name} {args}"
            );
        }
        assert_eq!(
            errors(&tool("Bash"), &json!({"command": "x", "timeout": 400})),
            None
        );
    }
}

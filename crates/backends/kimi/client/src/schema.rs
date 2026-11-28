use serde_json::{Map, Value};
use std::collections::BTreeSet;

#[derive(Clone, Copy)]
enum Slot {
    Single,
    Array,
    Map,
    SchemaOrArray,
}

const CHILD_SLOTS: [(&str, Slot, Option<&str>); 22] = [
    ("$defs", Slot::Map, None),
    ("definitions", Slot::Map, None),
    ("dependencies", Slot::Map, Some("object")),
    ("dependentSchemas", Slot::Map, Some("object")),
    ("patternProperties", Slot::Map, Some("object")),
    ("properties", Slot::Map, Some("object")),
    ("additionalItems", Slot::Single, Some("array")),
    ("additionalProperties", Slot::Single, Some("object")),
    ("contains", Slot::Single, Some("array")),
    ("contentSchema", Slot::Single, Some("string")),
    ("else", Slot::Single, None),
    ("if", Slot::Single, None),
    ("not", Slot::Single, None),
    ("propertyNames", Slot::Single, Some("object")),
    ("then", Slot::Single, None),
    ("unevaluatedItems", Slot::Single, Some("array")),
    ("unevaluatedProperties", Slot::Single, Some("object")),
    ("allOf", Slot::Array, None),
    ("anyOf", Slot::Array, None),
    ("oneOf", Slot::Array, None),
    ("prefixItems", Slot::Array, Some("array")),
    ("items", Slot::SchemaOrArray, Some("array")),
];

const SKIP_KEYS: [&str; 8] = [
    "$ref", "allOf", "anyOf", "else", "if", "not", "oneOf", "then",
];
const OBJECT_KEYS: [&str; 4] = [
    "dependentRequired",
    "maxProperties",
    "minProperties",
    "required",
];
const ARRAY_KEYS: [&str; 5] = [
    "maxContains",
    "maxItems",
    "minContains",
    "minItems",
    "uniqueItems",
];
const STRING_KEYS: [&str; 6] = [
    "contentEncoding",
    "contentMediaType",
    "format",
    "maxLength",
    "minLength",
    "pattern",
];
const NUMERIC_KEYS: [&str; 5] = [
    "exclusiveMaximum",
    "exclusiveMinimum",
    "maximum",
    "minimum",
    "multipleOf",
];
const TYPE_ORDER: [&str; 7] = [
    "string", "number", "integer", "boolean", "object", "array", "null",
];

pub fn normalize_tool_schema(schema: &Value) -> Result<Value, String> {
    let mut visited = BTreeSet::new();
    let mut resolved = resolve(schema, schema, &mut visited);
    if let Value::Object(root) = &mut resolved {
        for bucket in ["$defs", "definitions"] {
            let prefix = format!("#/{bucket}/");
            let unresolved = root
                .iter()
                .any(|(key, value)| key != bucket && has_ref(value, &prefix));
            if !unresolved {
                root.remove(bucket);
            }
        }
    }
    let Value::Object(root) = &mut resolved else {
        return Err("JSON Schema root must normalize to an object.".to_owned());
    };
    visit_children(root, &mut normalize_property)?;
    Ok(resolved)
}

fn has_ref(node: &Value, prefix: &str) -> bool {
    match node {
        Value::Array(items) => items.iter().any(|item| has_ref(item, prefix)),
        Value::Object(map) => {
            map.get("$ref")
                .and_then(Value::as_str)
                .is_some_and(|reference| reference.starts_with(prefix))
                || map.values().any(|value| has_ref(value, prefix))
        }
        _ => false,
    }
}

fn resolve(node: &Value, root: &Value, visited: &mut BTreeSet<String>) -> Value {
    match node {
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| resolve(item, root, visited))
                .collect(),
        ),
        Value::Object(map) => {
            if let Some(reference) = map.get("$ref").and_then(Value::as_str) {
                if !(reference == "#" || reference.starts_with("#/")) || visited.contains(reference)
                {
                    return node.clone();
                }
                let Some(target) = pointer(root, reference) else {
                    return node.clone();
                };
                visited.insert(reference.to_owned());
                let resolved = resolve(target, root, visited);
                visited.remove(reference);
                let Value::Object(mut merged) = resolved else {
                    return resolved;
                };
                for (key, value) in map {
                    if key != "$ref" {
                        merged.insert(key.clone(), resolve(value, root, visited));
                    }
                }
                return Value::Object(merged);
            }
            Value::Object(
                map.iter()
                    .map(|(key, value)| (key.clone(), resolve(value, root, visited)))
                    .collect(),
            )
        }
        _ => node.clone(),
    }
}

fn pointer<'a>(root: &'a Value, reference: &str) -> Option<&'a Value> {
    if reference == "#" {
        return Some(root);
    }
    let mut current = root;
    for raw in reference[2..].split('/') {
        let part = raw.replace("~1", "/").replace("~0", "~");
        current = match current {
            Value::Object(map) => map.get(&part)?,
            Value::Array(items) => {
                let canonical = part == "0" || (!part.starts_with('0') && !part.is_empty());
                let index = part
                    .parse::<usize>()
                    .ok()
                    .filter(|_| canonical && part.bytes().all(|byte| byte.is_ascii_digit()))?;
                items.get(index)?
            }
            _ => return None,
        };
    }
    Some(current)
}

fn visit_children(
    node: &mut Map<String, Value>,
    visit: &mut dyn FnMut(&mut Value) -> Result<(), String>,
) -> Result<(), String> {
    for (key, slot, _) in CHILD_SLOTS {
        let Some(value) = node.get_mut(key) else {
            continue;
        };
        match (slot, value) {
            (Slot::Single | Slot::SchemaOrArray, child @ Value::Object(_)) => visit(child)?,
            (Slot::Array | Slot::SchemaOrArray, Value::Array(items)) => {
                for item in items {
                    visit(item)?;
                }
            }
            (Slot::Map, Value::Object(map)) => {
                for item in map.values_mut() {
                    visit(item)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn structure_keys(parent: &str, extra: &[&'static str]) -> Vec<&'static str> {
    CHILD_SLOTS
        .iter()
        .filter(|(_, _, owner)| *owner == Some(parent))
        .map(|(key, _, _)| *key)
        .chain(extra.iter().copied())
        .collect()
}

fn has_any(node: &Map<String, Value>, keys: &[&str]) -> bool {
    keys.iter().any(|key| node.contains_key(*key))
}

fn normalize_property(node: &mut Value) -> Result<(), String> {
    let Value::Object(map) = node else {
        return Ok(());
    };
    let skipped = has_any(map, &SKIP_KEYS);
    let values = match (map.get("enum"), map.get("const")) {
        (Some(Value::Array(values)), _) if !values.is_empty() => Some(values.clone()),
        (_, Some(value)) => Some(vec![value.clone()]),
        _ => None,
    };
    match map.get("type") {
        None if !skipped => {
            let kind = match &values {
                Some(values) => infer_from_values(values)?,
                None => infer_from_structure(map),
            };
            map.insert("type".to_owned(), Value::String(kind.to_owned()));
        }
        Some(Value::String(current)) if !skipped => {
            if let Some(Ok(inferred)) = values.as_deref().map(infer_from_values)
                && current != inferred
            {
                map.insert("type".to_owned(), Value::String(inferred.to_owned()));
                if inferred != "object" {
                    for key in structure_keys("object", &OBJECT_KEYS) {
                        map.remove(key);
                    }
                }
                if inferred != "array" {
                    for key in structure_keys("array", &ARRAY_KEYS) {
                        map.remove(key);
                    }
                }
            }
        }
        _ => {}
    }
    visit_children(map, &mut normalize_property)
}

fn infer_from_structure(node: &Map<String, Value>) -> &'static str {
    if has_any(node, &structure_keys("object", &OBJECT_KEYS)) {
        "object"
    } else if has_any(node, &structure_keys("array", &ARRAY_KEYS)) {
        "array"
    } else if has_any(node, &structure_keys("string", &STRING_KEYS)) {
        "string"
    } else if has_any(node, &NUMERIC_KEYS) {
        "number"
    } else {
        "string"
    }
}

fn infer_from_values(values: &[Value]) -> Result<&'static str, String> {
    let mut kinds = BTreeSet::new();
    for value in values {
        kinds.insert(match value {
            Value::Null => "null",
            Value::Bool(_) => "boolean",
            Value::Number(number) => match number.as_f64() {
                Some(float) if float.fract() != 0.0 => "number",
                _ => "integer",
            },
            Value::String(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        });
    }
    if kinds.contains("number") {
        kinds.remove("integer");
    }
    let ordered = TYPE_ORDER
        .iter()
        .copied()
        .filter(|kind| kinds.contains(kind))
        .collect::<Vec<_>>();
    match ordered.as_slice() {
        [kind] => Ok(kind),
        [] => Err("Cannot infer JSON Schema type from an empty enum.".to_owned()),
        _ => Err(
            "Mixed JSON Schema enum or const types are not supported by Kimi tool schemas."
                .to_owned(),
        ),
    }
}

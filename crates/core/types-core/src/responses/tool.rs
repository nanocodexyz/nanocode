use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(remote = "Self", tag = "type", rename_all = "snake_case")]
pub enum ToolDefinition {
    Function {
        name: Box<str>,

        description: std::sync::Arc<str>,

        strict: bool,

        #[serde(default, skip_serializing_if = "Option::is_none")]
        defer_loading: Option<bool>,

        #[serde(default, rename = "async", skip_serializing_if = "is_false")]
        asynchronous: bool,

        parameters: JsonSchema,
        #[serde(default, skip_serializing)]
        output_schema: Option<JsonSchema>,
    },

    Namespace {
        name: Box<str>,

        description: std::sync::Arc<str>,

        #[serde(serialize_with = "serialize_namespace_tools")]
        tools: Vec<Self>,
    },

    Custom {
        name: Box<str>,

        description: std::sync::Arc<str>,

        #[serde(default, skip_serializing_if = "Option::is_none")]
        defer_loading: Option<bool>,

        #[serde(default, rename = "async", skip_serializing_if = "is_false")]
        asynchronous: bool,

        format: CustomToolFormat,
    },

    ToolSearch {
        execution: Box<str>,

        description: std::sync::Arc<str>,

        parameters: JsonSchema,
    },
}

impl Serialize for ToolDefinition {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        Self::serialize(self, serializer)
    }
}

impl<'de> Deserialize<'de> for ToolDefinition {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        decode_definition(Value::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Deserialize)]
#[serde(remote = "ToolDefinition", rename_all = "snake_case")]
enum TaggedDefinition {
    Function {
        name: Box<str>,
        description: std::sync::Arc<str>,
        strict: bool,
        #[serde(default)]
        defer_loading: Option<bool>,
        #[serde(default, rename = "async")]
        asynchronous: bool,
        parameters: JsonSchema,
        #[serde(default)]
        output_schema: Option<JsonSchema>,
    },
    Namespace {
        name: Box<str>,
        description: std::sync::Arc<str>,
        tools: Vec<ToolDefinition>,
    },
    Custom {
        name: Box<str>,
        description: std::sync::Arc<str>,
        #[serde(default)]
        defer_loading: Option<bool>,
        #[serde(default, rename = "async")]
        asynchronous: bool,
        format: CustomToolFormat,
    },
    ToolSearch {
        execution: Box<str>,
        description: std::sync::Arc<str>,
        parameters: JsonSchema,
    },
}

fn decode_definition(value: Value) -> Result<ToolDefinition, serde_json::Error> {
    let Value::Object(mut fields) = value else {
        return Err(serde::de::Error::custom(
            "a tool definition must be an object",
        ));
    };
    let Some(Value::String(kind)) = fields.remove("type") else {
        return Err(serde::de::Error::missing_field("type"));
    };
    TaggedDefinition::deserialize(Value::Object(serde_json::Map::from_iter([(
        kind,
        Value::Object(fields),
    )])))
}

const fn is_false(value: &bool) -> bool {
    !*value
}

fn serialize_namespace_tools<S>(tools: &[ToolDefinition], serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    if tools.iter().any(|tool| {
        !matches!(
            tool,
            ToolDefinition::Function { .. } | ToolDefinition::Custom { .. }
        )
    }) {
        return Err(serde::ser::Error::custom(
            "Responses namespaces may contain only function or custom definitions",
        ));
    }
    tools.serialize(serializer)
}

impl ToolDefinition {
    #[must_use]
    pub fn function(
        name: impl Into<Box<str>>,
        description: impl Into<std::sync::Arc<str>>,
        parameters: impl Into<JsonSchema>,
    ) -> Self {
        Self::Function {
            name: name.into(),
            description: description.into(),
            strict: false,
            defer_loading: None,
            asynchronous: false,
            parameters: parameters.into(),
            output_schema: None,
        }
    }

    #[must_use]
    pub fn custom(
        name: impl Into<Box<str>>,
        description: impl Into<std::sync::Arc<str>>,
        format: CustomToolFormat,
    ) -> Self {
        Self::Custom {
            name: name.into(),
            description: description.into(),
            defer_loading: None,
            asynchronous: false,
            format,
        }
    }

    #[must_use]
    pub fn namespace(
        name: impl Into<Box<str>>,
        description: impl Into<std::sync::Arc<str>>,
        tools: impl IntoIterator<Item = Self>,
    ) -> Self {
        Self::Namespace {
            name: name.into(),
            description: description.into(),
            tools: tools.into_iter().collect(),
        }
    }

    #[must_use]
    pub fn tool_search(
        execution: impl Into<Box<str>>,
        description: impl Into<std::sync::Arc<str>>,
        parameters: impl Into<JsonSchema>,
    ) -> Self {
        Self::ToolSearch {
            execution: execution.into(),
            description: description.into(),
            parameters: parameters.into(),
        }
    }

    #[must_use]
    pub const fn with_strict_parameters(mut self) -> Self {
        if let Self::Function { strict, .. } = &mut self {
            *strict = true;
        }
        self
    }

    #[must_use]
    pub fn with_output_schema(mut self, output_schema: impl Into<JsonSchema>) -> Self {
        if let Self::Function {
            output_schema: current,
            ..
        } = &mut self
        {
            *current = Some(output_schema.into());
        }
        self
    }

    #[must_use]
    #[allow(
        clippy::missing_const_for_fn,
        reason = "ToolDefinition owns drop types that const evaluation rejects"
    )]
    pub fn with_deferred_loading(mut self) -> Self {
        match &mut self {
            Self::Function { defer_loading, .. } | Self::Custom { defer_loading, .. } => {
                *defer_loading = Some(true);
            }
            Self::Namespace { .. } | Self::ToolSearch { .. } => {}
        }
        self
    }

    #[must_use]
    #[allow(
        clippy::missing_const_for_fn,
        reason = "ToolDefinition owns drop types that const evaluation rejects"
    )]
    pub fn with_async_execution(mut self) -> Self {
        match &mut self {
            Self::Function { asynchronous, .. } | Self::Custom { asynchronous, .. } => {
                *asynchronous = true;
            }
            Self::Namespace { .. } | Self::ToolSearch { .. } => {}
        }
        self
    }

    #[must_use]
    pub const fn is_async(&self) -> bool {
        match self {
            Self::Function { asynchronous, .. } | Self::Custom { asynchronous, .. } => {
                *asynchronous
            }
            Self::Namespace { .. } | Self::ToolSearch { .. } => false,
        }
    }

    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Function { name, .. }
            | Self::Namespace { name, .. }
            | Self::Custom { name, .. } => name,
            Self::ToolSearch { .. } => "tool_search",
        }
    }

    #[must_use]
    pub fn description(&self) -> &str {
        match self {
            Self::Function { description, .. }
            | Self::Namespace { description, .. }
            | Self::Custom { description, .. } => description,
            Self::ToolSearch { description, .. } => description,
        }
    }

    #[must_use]
    pub fn with_description(mut self, text: impl Into<std::sync::Arc<str>>) -> Self {
        match &mut self {
            Self::Function { description, .. }
            | Self::Namespace { description, .. }
            | Self::Custom { description, .. }
            | Self::ToolSearch { description, .. } => *description = text.into(),
        }
        self
    }

    #[must_use]
    pub const fn parameters(&self) -> Option<&JsonSchema> {
        match self {
            Self::Function { parameters, .. } | Self::ToolSearch { parameters, .. } => {
                Some(parameters)
            }
            Self::Namespace { .. } | Self::Custom { .. } => None,
        }
    }

    #[must_use]
    pub const fn output_schema(&self) -> Option<&JsonSchema> {
        match self {
            Self::Function { output_schema, .. } => output_schema.as_ref(),
            Self::Namespace { .. } | Self::Custom { .. } | Self::ToolSearch { .. } => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct CustomToolFormat {
    #[serde(rename = "type")]
    pub kind: Box<str>,

    pub syntax: Box<str>,

    pub definition: Box<str>,
}

impl CustomToolFormat {
    #[must_use]
    pub fn grammar(syntax: impl Into<Box<str>>, definition: impl Into<Box<str>>) -> Self {
        Self {
            kind: "grammar".into(),
            syntax: syntax.into(),
            definition: definition.into(),
        }
    }
}

#[derive(Clone)]
pub struct JsonSchema(Schema);

#[derive(Clone)]
enum Schema {
    Lazy(&'static LazySchema),
    Static(&'static StaticSchema),
    Value(std::sync::Arc<Value>),
}

pub struct LazySchema {
    build: fn() -> Value,
    json: std::sync::OnceLock<Box<serde_json::value::RawValue>>,
    value: std::sync::OnceLock<std::sync::Arc<Value>>,
}

impl LazySchema {
    #[must_use]
    pub const fn new(build: fn() -> Value) -> Self {
        Self {
            build,
            json: std::sync::OnceLock::new(),
            value: std::sync::OnceLock::new(),
        }
    }

    fn json(&self) -> &serde_json::value::RawValue {
        self.json.get_or_init(|| {
            serde_json::value::to_raw_value(&(self.build)())
                .unwrap_or_else(|error| panic!("{error}"))
        })
    }

    fn value(&self) -> &std::sync::Arc<Value> {
        self.value
            .get_or_init(|| std::sync::Arc::new((self.build)()))
    }
}

#[macro_export]
macro_rules! lazy_json_schema {
    ($build:expr) => {{
        static SCHEMA: $crate::responses::LazySchema =
            $crate::responses::LazySchema::new(|| $build);
        $crate::responses::JsonSchema::from_lazy(&SCHEMA)
    }};
}

pub struct StaticSchema {
    json: &'static str,
    value: std::sync::OnceLock<std::sync::Arc<Value>>,
}

impl StaticSchema {
    #[must_use]
    pub const fn new(json: &'static [u8]) -> Self {
        let Ok(json) = core::str::from_utf8(json) else {
            panic!("a static schema is UTF-8")
        };
        Self {
            json,
            value: std::sync::OnceLock::new(),
        }
    }

    fn value(&self) -> &std::sync::Arc<Value> {
        self.value.get_or_init(|| {
            std::sync::Arc::new(
                serde_json::from_str(self.json).unwrap_or_else(|error| panic!("{error}")),
            )
        })
    }
}

#[macro_export]
macro_rules! json_schema {
    ($($json:tt)+) => {{
        const LENGTH: usize = $crate::responses::compact_json_length(stringify!($($json)+));
        const JSON: [u8; LENGTH] = $crate::responses::compact_json::<LENGTH>(stringify!($($json)+));
        static SCHEMA: $crate::responses::StaticSchema = $crate::responses::StaticSchema::new(&JSON);
        $crate::responses::JsonSchema::from_static(&SCHEMA)
    }};
}

const fn compact(source: &[u8], json: &mut [u8]) -> usize {
    let mut index = 0;
    let mut length = 0;
    let mut string = false;
    let mut escaped = false;
    while index < source.len() {
        let byte = source[index];
        index += 1;
        if !string && byte.is_ascii_whitespace() {
            continue;
        }
        if length < json.len() {
            json[length] = byte;
        }
        length += 1;
        if escaped {
            escaped = false;
        } else if string && byte == b'\\' {
            escaped = true;
        } else if byte == b'"' {
            string = !string;
        }
    }
    length
}

#[doc(hidden)]
#[must_use]
pub const fn compact_json_length(source: &str) -> usize {
    compact(source.as_bytes(), &mut [])
}

#[doc(hidden)]
#[must_use]
pub const fn compact_json<const LENGTH: usize>(source: &str) -> [u8; LENGTH] {
    let mut json = [0; LENGTH];
    assert!(compact(source.as_bytes(), &mut json) == LENGTH);
    assert!(
        json_value(&json, 0) == LENGTH,
        "a static schema is one JSON value"
    );
    json
}

const fn json_value(json: &[u8], index: usize) -> usize {
    assert!(index < json.len(), "a static schema ends early");
    match json[index] {
        b'{' => json_members(json, index + 1, b'}'),
        b'[' => json_members(json, index + 1, b']'),
        b'"' => json_string(json, index + 1),
        b't' => json_word(json, index, b"true"),
        b'f' => json_word(json, index, b"false"),
        b'n' => json_word(json, index, b"null"),
        _ => json_number(json, index),
    }
}

const fn json_members(json: &[u8], mut index: usize, close: u8) -> usize {
    if index < json.len() && json[index] == close {
        return index + 1;
    }
    loop {
        if close == b'}' {
            assert!(index < json.len(), "a static schema ends early");
            assert!(
                json[index] == b'"',
                "a static schema object key is a string"
            );
            index = json_string(json, index + 1);
            assert!(index < json.len(), "a static schema ends early");
            assert!(
                json[index] == b':',
                "a static schema object key precedes a colon"
            );
            index += 1;
        }
        index = json_value(json, index);
        assert!(index < json.len(), "a static schema ends early");
        if json[index] == close {
            return index + 1;
        }
        assert!(
            json[index] == b',',
            "a static schema separates members with commas"
        );
        index += 1;
    }
}

const fn json_string(json: &[u8], mut index: usize) -> usize {
    while index < json.len() {
        match json[index] {
            b'"' => return index + 1,
            b'\\' => {
                assert!(index + 1 < json.len(), "a static schema ends early");
                match json[index + 1] {
                    b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => index += 2,
                    b'u' => {
                        let mut digit = index + 2;
                        while digit < index + 6 {
                            assert!(digit < json.len(), "a static schema ends early");
                            assert!(
                                json[digit].is_ascii_hexdigit(),
                                "a static schema unicode escape has four hex digits"
                            );
                            digit += 1;
                        }
                        index += 6;
                    }
                    _ => panic!("a static schema string has a JSON escape"),
                }
            }
            byte => {
                assert!(byte >= 0x20, "a static schema string has no control bytes");
                index += 1;
            }
        }
    }
    panic!("a static schema string is closed")
}

const fn json_word(json: &[u8], index: usize, word: &[u8]) -> usize {
    let mut offset = 0;
    while offset < word.len() {
        assert!(index + offset < json.len(), "a static schema ends early");
        assert!(
            json[index + offset] == word[offset],
            "a static schema literal is true, false or null"
        );
        offset += 1;
    }
    index + word.len()
}

const fn json_digits(json: &[u8], mut index: usize) -> usize {
    let start = index;
    while index < json.len() && json[index].is_ascii_digit() {
        index += 1;
    }
    assert!(index > start, "a static schema number has digits");
    index
}

const fn json_number(json: &[u8], mut index: usize) -> usize {
    if json[index] == b'-' {
        index += 1;
    }
    if index < json.len() && json[index] == b'0' {
        index += 1;
    } else {
        index = json_digits(json, index);
    }
    if index < json.len() && json[index] == b'.' {
        index = json_digits(json, index + 1);
    }
    if index < json.len() && (json[index] == b'e' || json[index] == b'E') {
        index += 1;
        if index < json.len() && (json[index] == b'+' || json[index] == b'-') {
            index += 1;
        }
        index = json_digits(json, index);
    }
    index
}

impl JsonSchema {
    #[must_use]
    pub const fn from_lazy(schema: &'static LazySchema) -> Self {
        Self(Schema::Lazy(schema))
    }

    #[must_use]
    pub const fn from_static(schema: &'static StaticSchema) -> Self {
        Self(Schema::Static(schema))
    }

    #[must_use]
    pub fn as_value(&self) -> &Value {
        match &self.0 {
            Schema::Lazy(schema) => schema.value(),
            Schema::Static(schema) => schema.value(),
            Schema::Value(value) => value,
        }
    }

    #[must_use]
    pub fn is_object(&self) -> bool {
        match &self.0 {
            Schema::Lazy(schema) => schema.json().get().starts_with('{'),
            Schema::Static(schema) => schema.json.starts_with('{'),
            Schema::Value(value) => value.is_object(),
        }
    }

    #[must_use]
    pub fn shared(&self) -> std::sync::Arc<Value> {
        match &self.0 {
            Schema::Lazy(schema) => std::sync::Arc::clone(schema.value()),
            Schema::Static(schema) => std::sync::Arc::clone(schema.value()),
            Schema::Value(value) => std::sync::Arc::clone(value),
        }
    }
}

impl std::fmt::Debug for JsonSchema {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            Schema::Lazy(schema) => formatter
                .debug_tuple("JsonSchema")
                .field(schema.value())
                .finish(),
            Schema::Static(schema) => formatter
                .debug_tuple("JsonSchema")
                .field(&schema.json)
                .finish(),
            Schema::Value(value) => formatter.debug_tuple("JsonSchema").field(value).finish(),
        }
    }
}

impl<'de> Deserialize<'de> for JsonSchema {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Value::deserialize(deserializer).map(Self::from)
    }
}

impl From<std::sync::Arc<Value>> for JsonSchema {
    fn from(value: std::sync::Arc<Value>) -> Self {
        Self(Schema::Value(value))
    }
}

impl From<Value> for JsonSchema {
    fn from(value: Value) -> Self {
        Self(Schema::Value(std::sync::Arc::new(value)))
    }
}

impl PartialEq for JsonSchema {
    fn eq(&self, other: &Self) -> bool {
        match (&self.0, &other.0) {
            (Schema::Lazy(left), Schema::Lazy(right)) if std::ptr::eq(*left, *right) => true,
            (Schema::Static(left), Schema::Static(right)) if std::ptr::eq(*left, *right) => true,
            (Schema::Value(left), Schema::Value(right)) if std::sync::Arc::ptr_eq(left, right) => {
                true
            }
            _ => self.as_value() == other.as_value(),
        }
    }
}

impl Serialize for JsonSchema {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match &self.0 {
            Schema::Lazy(schema) => schema.json().serialize(serializer),
            Schema::Static(schema) => {
                serde_json::from_str::<&serde_json::value::RawValue>(schema.json)
                    .map_err(serde::ser::Error::custom)?
                    .serialize(serializer)
            }
            Schema::Value(value) => value.serialize(serializer),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(transparent)]
pub struct JsonValue(std::sync::Arc<Value>);

impl JsonValue {
    #[must_use]
    pub fn as_value(&self) -> &Value {
        &self.0
    }
}

impl From<Value> for JsonValue {
    fn from(value: Value) -> Self {
        Self(std::sync::Arc::new(value))
    }
}

impl PartialEq for JsonValue {
    fn eq(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.0, &other.0) || self.0 == other.0
    }
}

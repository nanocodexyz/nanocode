use crate::validate::Node;
use nanocode_backend_kimi_client::normalize_tool_schema;
use nanocode_tool_host::contract::{ToolOutputBody, ToolOutputContent};
use nanocode_tool_host::embedded::{CodeModeObserver, CodeModeUpdate, OwnedToolContext};
use nanocode_tool_host::runtime::ToolRuntime;
use nanocode_tool_host::{ToolContext, ToolDefinition, ToolInput};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub struct NanoTools {
    runtime: ToolRuntime,
    routes: BTreeMap<String, Route>,
    pub entries: Vec<Entry>,
}

/// One declared tool: its name, description, the schema its arguments are validated against, and the
/// declaration sent to Kimi.
pub struct Entry {
    pub name: String,
    pub description: String,
    pub schema: Value,
    pub ordered: Node,
    pub declaration: Value,
}

/// Every tool the model sees, in the order Kimi Code declares them (by name, ignoring case), with the
/// token estimates its context accounting uses.
pub struct Catalog {
    pub declarations: Vec<Value>,
    pub schemas: BTreeMap<String, Node>,
    pub sizes: u64,
    pub listing: u64,
}

impl Catalog {
    pub fn new(mut entries: Vec<Entry>) -> Self {
        entries.sort_by_key(|entry| entry.name.to_lowercase());
        let estimate = nanocode_backend_kimi_client::estimate_text;
        let sizes = entries
            .iter()
            .map(|entry| {
                estimate(&entry.name)
                    + estimate(&entry.description)
                    + estimate(&entry.schema.to_string())
            })
            .sum();
        let listing = match entries.is_empty() {
            true => 0,
            false => estimate(
                &Value::Array(
                    entries
                        .iter()
                        .map(|entry| json!({"name": entry.name, "description": entry.description, "parameters": entry.schema}))
                        .collect(),
                )
                .to_string(),
            ),
        };
        Self {
            declarations: entries
                .iter()
                .map(|entry| entry.declaration.clone())
                .collect(),
            schemas: entries
                .into_iter()
                .map(|entry| (entry.name, entry.ordered))
                .collect(),
            sizes,
            listing,
        }
    }
}

pub fn builtin_entries(builtins: &crate::tools::Builtins) -> Vec<Entry> {
    let ordered = match Node::parse(crate::tools::CONTRACTS) {
        Ok(Node::List(tools)) => tools,
        _ => Vec::new(),
    };
    crate::tools::Builtins::declarations()
        .into_iter()
        .zip(ordered)
        .map(|(declaration, ordered)| Entry {
            ordered: ordered
                .get("function")
                .and_then(|function| function.get("parameters"))
                .cloned()
                .unwrap_or(Node::Leaf(json!({}))),
            name: declaration["function"]["name"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            description: declaration["function"]["description"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            schema: declaration["function"]["parameters"].clone(),
            declaration,
        })
        .filter(|entry| builtins.handles(&entry.name))
        .collect()
}

#[derive(Clone)]
enum Route {
    Function(String),
    Exec,
    Wait,
}

struct Quiet;

impl CodeModeObserver for Quiet {
    fn update(&mut self, _update: CodeModeUpdate<'_>) {}
}

fn exec_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "code": {"type": "string", "description": "JavaScript source to evaluate."},
            "yield_time_ms": {"type": "number", "description": "How long to wait before a script that is still running yields."},
            "max_output_tokens": {"type": "number", "description": "Token budget for this call's direct results."}
        },
        "required": ["code"]
    })
}

impl NanoTools {
    pub fn new(runtime: ToolRuntime, session: &str) -> Result<Self, String> {
        let mut routes = BTreeMap::new();
        let mut entries = Vec::new();
        for definition in flatten(runtime.model_specs(session)) {
            let (name, description, schema, text, route) = match definition {
                ToolDefinition::Function {
                    name,
                    description,
                    parameters,
                    ..
                } => {
                    let schema =
                        serde_json::to_value(&parameters).unwrap_or(json!({"type": "object"}));
                    let text = serde_json::to_string(&parameters).unwrap_or_default();
                    let route = match &*name {
                        "wait" => Route::Wait,
                        _ => Route::Function(name.to_string()),
                    };
                    (
                        name.to_string(),
                        description.to_string(),
                        schema,
                        text,
                        route,
                    )
                }
                ToolDefinition::Custom {
                    name, description, ..
                } => (
                    name.to_string(),
                    description.to_string(),
                    exec_schema(),
                    exec_schema().to_string(),
                    Route::Exec,
                ),
                _ => continue,
            };
            if routes.contains_key(&name) {
                continue;
            }
            let parameters = normalize_tool_schema(&schema)
                .map_err(|error| format!("tool {name:?} has a schema Kimi refuses: {error}"))?;
            let declaration = json!({
                "type": "function",
                "function": {"name": name, "description": description, "parameters": parameters},
            });
            routes.insert(name.clone(), route);
            let ordered = Node::parse(&text).unwrap_or(Node::Leaf(json!({})));
            entries.push(Entry {
                name,
                description,
                schema,
                ordered,
                declaration,
            });
        }
        Ok(Self {
            runtime,
            routes,
            entries,
        })
    }

    pub fn handles(&self, name: &str) -> bool {
        self.routes.contains_key(name)
    }

    pub async fn call(
        &self,
        name: &str,
        args: &Value,
        context: ToolContext<'_>,
    ) -> std::result::Result<String, String> {
        let route = self
            .routes
            .get(name)
            .cloned()
            .ok_or_else(|| format!("unknown tool {name:?}"))?;
        match route {
            Route::Exec => {
                let source = args.get("code").and_then(Value::as_str).unwrap_or_default();
                let code = nanocode_tool_host::code_mode::source_with_options(
                    source,
                    args.get("yield_time_ms"),
                    args.get("max_output_tokens"),
                )?;
                let execution = self
                    .runtime
                    .execute_code_owned_with_updates(
                        &code,
                        OwnedToolContext::from_context(context),
                        &mut Quiet,
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                let mut text = body_text(&execution.output);
                for notification in execution.notifications {
                    text.push_str("\n\n");
                    text.push_str(&notification.text);
                }
                match execution.success {
                    true => Ok(text),
                    false => Err(text),
                }
            }
            Route::Wait => {
                let execution = self
                    .runtime
                    .wait_for_code_with_updates(&args.to_string(), context, &mut Quiet)
                    .await
                    .map_err(|error| error.to_string())?;
                let text = body_text(&execution.output);
                match execution.success {
                    true => Ok(text),
                    false => Err(text),
                }
            }
            Route::Function(target) => {
                let raw =
                    serde_json::value::to_raw_value(args).map_err(|error| error.to_string())?;
                let output = self
                    .runtime
                    .execute_tool(&target, ToolInput::Function(raw), context)
                    .await
                    .map_err(|error| error.to_string())?;
                let text = body_text(&output.output);
                match output.success {
                    true => Ok(text),
                    false => Err(text),
                }
            }
        }
    }
}

fn flatten(definitions: Vec<ToolDefinition>) -> Vec<ToolDefinition> {
    let mut flat = Vec::new();
    for definition in definitions {
        match definition {
            ToolDefinition::Namespace {
                name: namespace,
                tools,
                ..
            } => {
                for mut tool in tools {
                    match &mut tool {
                        ToolDefinition::Function { name, .. }
                        | ToolDefinition::Custom { name, .. } => {
                            *name = format!("{namespace}__{name}").into();
                        }
                        _ => continue,
                    }
                    flat.push(tool);
                }
            }
            definition => flat.push(definition),
        }
    }
    flat
}

fn body_text(body: &ToolOutputBody) -> String {
    match body {
        ToolOutputBody::Text(text) => text.clone(),
        ToolOutputBody::Content(items) => items
            .iter()
            .filter_map(|item| match item {
                ToolOutputContent::InputText { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

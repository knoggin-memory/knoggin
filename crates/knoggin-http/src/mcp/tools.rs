// SPDX-License-Identifier: AGPL-3.0-only
//! The 18 tools: titles, descriptions and hints as the MCP tab gives them, the two tool
//! sets, and each tool's call into the same `StoreHost` methods REST uses.

use knoggin_core::ops::{
    BatchResult, Context, ForgetManyResult, Forgotten, Items, MemoryView, Moved, NamespaceDeleted,
    NamespaceStatsResult, NamespaceView, Page, RecallResponse, Reinforced, SimilarHit,
    SimilarResult, StoredMemory, TagCount,
};
use serde::Serialize;
use serde_json::Value;

use super::args::{self, parse};
use super::protocol::{ToolAnnotations, ToolInfo};
use crate::ctx::RequestContext;
use crate::error::{ApiError, Code};
use crate::host::{Reply, StoreHost};

/// Which endpoint a request came in on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolSet {
    /// `/mcp`: all 18 tools, access recording on by default.
    Full,
    /// `/mcp/plugin`: 10 tools, access recording always off, no `recordAccess`.
    Plugin,
}

/// One tool of the catalog.
pub(crate) struct Tool {
    pub(crate) name: &'static str,
    title: &'static str,
    description: &'static str,
    /// The description on `/mcp/plugin`, where it differs.
    plugin_description: Option<&'static str>,
    /// Needs `memory:write`; otherwise `memory:read`.
    pub(crate) write: bool,
    in_plugin: bool,
    /// `readOnlyHint` on `/mcp` and on `/mcp/plugin`.
    read_only: (bool, bool),
    destructive: bool,
    /// `idempotentHint` on `/mcp` and on `/mcp/plugin`.
    idempotent: (bool, bool),
    always_load: bool,
    input: fn() -> Value,
    output: fn() -> Value,
}

/// The one argument only `/mcp` has.
const RECORD_ACCESS: &str = "recordAccess";

macro_rules! tool {
    (@out $output:ty) => { args::output::<$output> };
    (@out $output:ty, $inner:ty) => { args::output_of::<$output, $inner> };
    ($name:literal, $title:literal, $desc:expr, $plugin_desc:expr, write: $w:literal, plugin: $p:literal,
     ro: $ro:expr, d: $d:literal, i: $i:expr, always: $a:literal, $input:ty => $output:ty $(, of $inner:ty)?) => {
        Tool {
            name: $name,
            title: $title,
            description: $desc,
            plugin_description: $plugin_desc,
            write: $w,
            in_plugin: $p,
            read_only: $ro,
            destructive: $d,
            idempotent: $i,
            always_load: $a,
            input: args::standalone::<$input>,
            output: tool!(@out $output $(, $inner)?),
        }
    };
}

/// Every tool, in the order of the MCP tab.
pub(crate) static TOOLS: [Tool; 18] = [
    tool!("get_context", "Get Memory Context",
        "Load the user's long-term memory context in one call: the default namespace, the list of namespaces, and the most relevant memories for an optional topic. Call this once at the start of a task when earlier context could matter.",
        None, write: false, plugin: true, ro: (true, true), d: false, i: (true, true), always: true,
        args::GetContextArgs => Context),
    tool!("recall_memories", "Recall Memories",
        "Search the user's long-term memories by meaning. Call before answering questions about the user's projects, preferences or past decisions. Returns up to 25 memories unless you ask for another number; the best matches come with full text.",
        None, write: false, plugin: true, ro: (false, true), d: false, i: (false, true), always: true,
        args::RecallArgs => RecallResponse),
    tool!("get_memory", "Get Memory",
        "Fetch one memory by ID, with its full text.",
        None, write: false, plugin: true, ro: (false, true), d: false, i: (false, true), always: false,
        args::GetMemoryArgs => MemoryView),
    tool!("find_similar_memories", "Find Similar Memories",
        "Find memories close in meaning to a given memory. Useful for spotting duplicates before saving or for exploring a topic.",
        None, write: false, plugin: true, ro: (false, true), d: false, i: (false, true), always: false,
        args::FindSimilarArgs => SimilarResult, of SimilarHit),
    tool!("list_memories", "List Memories",
        "Page through the memories of a namespace, newest first, optionally filtered by tag, phase or date. Does not search by meaning; use recall_memories for that.",
        None, write: false, plugin: true, ro: (true, true), d: false, i: (true, true), always: false,
        args::ListMemoriesArgs => Page<MemoryView>, of MemoryView),
    tool!("list_namespaces", "List Namespaces",
        "List the namespaces in this memory store with their memory counts. Namespaces organize memories; they are not access controls.",
        None, write: false, plugin: true, ro: (true, true), d: false, i: (true, true), always: false,
        args::NoArgs => Items<NamespaceView>, of NamespaceView),
    tool!("list_tags", "List Tags",
        "List the tags used in a namespace with how many memories carry each.",
        None, write: false, plugin: false, ro: (true, true), d: false, i: (true, true), always: false,
        args::ListTagsArgs => Page<TagCount>, of TagCount),
    tool!("namespace_stats", "Namespace Statistics",
        "Counts and decay settings for one namespace.",
        None, write: false, plugin: false, ro: (true, true), d: false, i: (true, true), always: false,
        args::NamespaceStatsArgs => NamespaceStatsResult),
    tool!("store_memory", "Store Memory",
        "Save one memory: a short summary and optional full text. Save what the user asked you to keep, or what your instructions tell you to keep, and tell the user what you saved. Never save passwords, keys or other secrets. This only adds; it never replaces an existing memory.",
        Some("Save one memory: a short summary and optional full text. Before you call this, show the user the exact text and wait for a yes; then tell the user what you saved. Never save passwords, keys or other secrets. This only adds; it never replaces an existing memory."),
        write: true, plugin: true, ro: (false, false), d: false, i: (false, false), always: true,
        args::StoreMemoryArgs => StoredMemory),
    tool!("store_memories", "Store Memories (Batch)",
        "Save up to 100 memories in one call. The same rules as store_memory apply to each.",
        None, write: true, plugin: false, ro: (false, false), d: false, i: (false, false), always: false,
        args::StoreMemoriesArgs => BatchResult<StoredMemory>, of StoredMemory),
    tool!("supersede_memory", "Supersede Memory",
        "Replace an existing memory with a corrected or newer version. The old memory is kept but no longer returned by recall.",
        Some("Replace an existing memory with a corrected or newer version. Before you call this, show the user the exact text and wait for a yes; then tell the user what you saved. The old memory is kept but no longer returned by recall."),
        write: true, plugin: true, ro: (false, false), d: true, i: (false, false), always: false,
        args::SupersedeArgs => StoredMemory),
    tool!("reinforce_memory", "Reinforce Memory",
        "Tell the store that a memory was useful (quality 3 or 4) or not (1 or 2). Useful memories fade more slowly.",
        None, write: true, plugin: true, ro: (false, false), d: false, i: (false, false), always: false,
        args::ReinforceArgs => Reinforced),
    tool!("move_memories", "Move Memories",
        "Move memories to another namespace. All of them move or none do.",
        None, write: true, plugin: false, ro: (false, false), d: false, i: (true, true), always: false,
        args::MoveArgs => Moved),
    tool!("create_namespace", "Create Namespace",
        "Create a new namespace to keep a project's or a topic's memories apart.",
        None, write: true, plugin: false, ro: (false, false), d: false, i: (false, false), always: false,
        args::CreateNamespaceArgs => NamespaceView),
    tool!("update_namespace_policy", "Update Namespace Policy",
        "Change how fast memories in a namespace fade. A faster setting can cause text to be dropped sooner; use dryRun first to see the effect.",
        None, write: true, plugin: false, ro: (false, false), d: true, i: (true, true), always: false,
        args::UpdatePolicyArgs => NamespaceView),
    tool!("forget_memory", "Forget Memory",
        "Delete one memory. This cannot be undone. Older encrypted backups that still hold it expire by themselves. Only do this when the user asks.",
        None, write: true, plugin: true, ro: (false, false), d: true, i: (true, true), always: false,
        args::ForgetArgs => Forgotten),
    tool!("forget_memories", "Forget Memories (Batch)",
        "Delete up to 100 memories. This cannot be undone. Older encrypted backups that still hold them expire by themselves. Use dryRun first. Only do this when the user asks.",
        None, write: true, plugin: false, ro: (false, false), d: true, i: (true, true), always: false,
        args::ForgetManyArgs => ForgetManyResult),
    tool!("delete_namespace", "Delete Namespace",
        "Delete a namespace. With force it also deletes every memory in it. This cannot be undone. Use dryRun first. Only do this when the user asks.",
        None, write: true, plugin: false, ro: (false, false), d: true, i: (false, false), always: false,
        args::DeleteNamespaceArgs => NamespaceDeleted),
];

impl Tool {
    /// The tool's `tools/list` entry on `set`, with its output schema when `output`.
    fn info(&self, set: ToolSet, output: bool) -> ToolInfo {
        let plugin = set == ToolSet::Plugin;
        let mut input = (self.input)();
        if plugin && let Some(props) = input.get_mut("properties").and_then(Value::as_object_mut) {
            props.remove(RECORD_ACCESS);
        }
        let description = match (plugin, self.plugin_description) {
            (true, Some(d)) => d,
            _ => self.description,
        };
        let pick = |(full, plug): (bool, bool)| if plugin { plug } else { full };
        ToolInfo {
            name: self.name,
            title: self.title,
            description: description.to_owned(),
            input_schema: input,
            output_schema: output.then(|| (self.output)()),
            annotations: ToolAnnotations {
                read_only_hint: pick(self.read_only),
                destructive_hint: self.destructive,
                idempotent_hint: pick(self.idempotent),
                open_world_hint: false,
            },
            meta: self
                .always_load
                .then(|| serde_json::json!({ "anthropic/alwaysLoad": true })),
        }
    }
}

/// The tools of `set`.
pub(crate) fn tools_of(set: ToolSet) -> impl Iterator<Item = &'static Tool> {
    TOOLS
        .iter()
        .filter(move |t| set == ToolSet::Full || t.in_plugin)
}

/// `tools/list`: the tools of `set` that the credential may call. Output schemas are
/// published on the current revision only.
pub(crate) fn list(set: ToolSet, ctx: &RequestContext, output: bool) -> Vec<ToolInfo> {
    tools_of(set)
        .filter(|t| ctx.permissions.write || !t.write)
        .map(|t| t.info(set, output))
        .collect()
}

/// The answer of one call: the JSON text, the same value parsed, and whether it failed.
pub(crate) struct Outcome {
    pub(crate) text: String,
    pub(crate) value: Value,
    pub(crate) is_error: bool,
    /// The tool's name when it is one of ours (safe to log), and the error code.
    pub(crate) logged: Option<(&'static str, &'static str)>,
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: ErrorFields<'a>,
}

#[derive(Serialize)]
struct ErrorFields<'a> {
    code: Code,
    message: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    field: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
}

fn failure(e: &ApiError) -> (String, Value) {
    let body = ErrorBody {
        error: ErrorFields {
            code: e.code,
            message: if e.code == Code::Internal {
                crate::error::INTERNAL_MESSAGE
            } else {
                &e.message
            },
            field: e.field.as_deref(),
            reason: e.reason,
        },
    };
    let value = serde_json::to_value(&body).unwrap_or_default();
    (value.to_string(), value)
}

/// Bytes of one JSON answer, the same for a fresh and a replayed write, so that a tool
/// result and the REST body of the same call match.
fn fresh<T: Serialize>(value: &T) -> Result<Vec<u8>, ApiError> {
    serde_json::to_vec(value).map_err(|_| ApiError::internal())
}

fn replied<T: Serialize>(reply: Result<Reply<T>, ApiError>) -> Result<Vec<u8>, ApiError> {
    reply?.into_json()
}

/// Runs one `tools/call`.
pub(crate) async fn call(
    host: &StoreHost,
    ctx: &RequestContext,
    set: ToolSet,
    name: &str,
    arguments: Value,
) -> Outcome {
    let tool = tools_of(set).find(|t| t.name == name);
    let result = match tool {
        None => Err(ApiError::new(
            Code::NotFound,
            "There is no tool of that name here",
        )),
        Some(tool) => run(host, ctx, set, tool, arguments).await,
    };
    let logged = tool.map(|t| t.name);
    match result {
        Ok(bytes) => {
            let value: Value = serde_json::from_slice(&bytes).unwrap_or_default();
            // A batch save fails as a whole only when every item failed.
            let is_error = name == "store_memories"
                && value["stored"] == 0
                && value["errors"].as_u64().is_some_and(|n| n > 0);
            Outcome {
                text: String::from_utf8(bytes).unwrap_or_default(),
                value,
                is_error,
                logged: logged.map(|n| (n, "ok")),
            }
        }
        Err(e) => {
            let (text, value) = failure(&e);
            Outcome {
                text,
                value,
                is_error: true,
                logged: logged.map(|n| (n, e.code.as_str())),
            }
        }
    }
}

async fn run(
    host: &StoreHost,
    ctx: &RequestContext,
    set: ToolSet,
    tool: &Tool,
    args: Value,
) -> Result<Vec<u8>, ApiError> {
    if tool.write {
        ctx.require_write()?;
    } else {
        ctx.require_read()?;
    }
    let Value::Object(map) = &args else {
        return Err(ApiError::invalid_argument(
            "arguments",
            "arguments must be an object",
        ));
    };
    let plugin = set == ToolSet::Plugin;
    if plugin && map.contains_key(RECORD_ACCESS) {
        return Err(ApiError::invalid_argument(
            RECORD_ACCESS,
            "unknown field `recordAccess`",
        ));
    }
    match tool.name {
        "get_context" => {
            let a: args::GetContextArgs = parse(args)?;
            fresh(&host.context(ctx, a.into()).await?)
        }
        "recall_memories" => {
            let mut req: knoggin_core::ops::Recall = parse::<args::RecallArgs>(args)?.into();
            req.record_access &= !plugin;
            fresh(&host.recall(ctx, req).await?)
        }
        "get_memory" => {
            let mut req: knoggin_core::ops::GetMemory = parse::<args::GetMemoryArgs>(args)?.into();
            req.record_access &= !plugin;
            fresh(&host.get_memory(ctx, req).await?)
        }
        "find_similar_memories" => {
            let mut req: knoggin_core::ops::FindSimilar =
                parse::<args::FindSimilarArgs>(args)?.into();
            req.record_access &= !plugin;
            fresh(&host.find_similar(ctx, req).await?)
        }
        "list_memories" => {
            let a: args::ListMemoriesArgs = parse(args)?;
            fresh(&host.list_memories(ctx, a.into()).await?)
        }
        "list_namespaces" => {
            let args::NoArgs {} = parse(args)?;
            fresh(&host.list_namespaces(ctx).await?)
        }
        "list_tags" => {
            let a: args::ListTagsArgs = parse(args)?;
            fresh(&host.list_tags(ctx, a.into()).await?)
        }
        "namespace_stats" => {
            let a: args::NamespaceStatsArgs = parse(args)?;
            fresh(&host.namespace_stats(ctx, a.into()).await?)
        }
        "store_memory" => {
            let (req, key) = parse::<args::StoreMemoryArgs>(args)?.split();
            replied(host.store_memory(ctx, req, key).await)
        }
        "store_memories" => {
            let (req, key) = parse::<args::StoreMemoriesArgs>(args)?.split();
            replied(host.store_memories(ctx, req, key).await)
        }
        "supersede_memory" => {
            let (req, key) = parse::<args::SupersedeArgs>(args)?.split();
            replied(host.supersede_memory(ctx, req, key).await)
        }
        "reinforce_memory" => {
            let (req, key) = parse::<args::ReinforceArgs>(args)?.split();
            replied(host.reinforce_memory(ctx, req, key).await)
        }
        "move_memories" => {
            let (req, key) = parse::<args::MoveArgs>(args)?.split();
            replied(host.move_memories(ctx, req, key).await)
        }
        "create_namespace" => {
            let (req, key) = parse::<args::CreateNamespaceArgs>(args)?.split();
            replied(host.create_namespace(ctx, req, key).await)
        }
        "update_namespace_policy" => {
            let (req, key) = parse::<args::UpdatePolicyArgs>(args)?.split();
            replied(host.update_namespace_policy(ctx, req, key).await)
        }
        "forget_memory" => {
            let (req, key) = parse::<args::ForgetArgs>(args)?.split();
            replied(host.forget_memory(ctx, req, key).await)
        }
        "forget_memories" => {
            let (req, key) = parse::<args::ForgetManyArgs>(args)?.split();
            replied(host.forget_memories(ctx, req, key).await)
        }
        "delete_namespace" => {
            let (req, key) = parse::<args::DeleteNamespaceArgs>(args)?.split();
            replied(host.delete_namespace(ctx, req, key).await)
        }
        _ => Err(ApiError::internal()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLUGIN_SAVE: &str = "Before you call this, show the user the exact text and wait for a yes; then tell the user what you saved.";

    fn walk(v: &Value, f: &mut impl FnMut(&serde_json::Map<String, Value>)) {
        match v {
            Value::Object(m) => {
                f(m);
                m.values().for_each(|x| walk(x, f));
            }
            Value::Array(a) => a.iter().for_each(|x| walk(x, f)),
            _ => {}
        }
    }

    #[test]
    fn every_schema_stands_alone() {
        for tool in &TOOLS {
            for schema in [(tool.input)(), (tool.output)()] {
                assert_eq!(schema["type"], "object", "{}: {schema}", tool.name);
                walk(&schema, &mut |m| {
                    assert!(!m.contains_key("$ref"), "{}: {schema}", tool.name);
                });
            }
        }
    }

    #[test]
    fn plugin_set_is_the_ten_named_tools() {
        let names: Vec<_> = tools_of(ToolSet::Plugin).map(|t| t.name).collect();
        assert_eq!(
            names,
            [
                "get_context",
                "recall_memories",
                "get_memory",
                "find_similar_memories",
                "list_memories",
                "list_namespaces",
                "store_memory",
                "supersede_memory",
                "reinforce_memory",
                "forget_memory"
            ]
        );
        assert_eq!(tools_of(ToolSet::Full).count(), 18);
        assert!(PLUGIN_SAVE.len() > 10);
        for t in &TOOLS {
            if let Some(d) = t.plugin_description {
                assert!(d.contains(PLUGIN_SAVE), "{}", t.name);
            }
        }
    }
}

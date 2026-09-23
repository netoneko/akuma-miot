//! What a cat thinks with.
//!
//! A thin seam over [`genai`], kept as our own trait-shaped surface for two
//! reasons: `miot-agent` can be tested against a fake with no network and no
//! provider, and swapping providers never reaches the agent loop.
//!
//! # One dialect, not two
//!
//! An earlier version of this crate detected whether an endpoint spoke ollama's
//! `/api/chat` or the OpenAI-compatible `/v1/chat/completions`, and normalised
//! the two shapes of `arguments` (ollama sends an object, OpenAI-compat sends a
//! JSON *string*). None of that is here, because **both ollama and
//! `llama-server` serve `/v1/`** — pointing a `ServiceTargetResolver` at the
//! endpoint collapses the whole problem to one code path.
//!
//! # Tool calls, not prose
//!
//! The surface is deliberately the tool-call one. The litter's finding is that
//! a small model picks a *value* far more reliably than it spells a bracket
//! syntax, and a tool call arrives already parsed with its arguments in fields.

use genai::adapter::AdapterKind;
pub use genai::chat::Tool;
use genai::chat::{ChatMessage, ChatRequest};
use genai::resolver::{AuthData, Endpoint, ServiceTargetResolver};
use genai::{Client, ModelIden, ServiceTarget};
use std::time::Instant;

/// One tool call the model asked for, arguments already parsed.
#[derive(Debug, Clone)]
pub struct Call {
    pub name: String,
    pub args: serde_json::Value,
}

impl Call {
    pub fn str(&self, k: &str) -> Option<String> {
        match self.args.get(k)? {
            serde_json::Value::String(s) => Some(s.clone()),
            other => Some(other.to_string()),
        }
    }
}

/// What one turn produced.
#[derive(Debug, Clone, Default)]
pub struct Turn {
    pub text: String,
    pub calls: Vec<Call>,
    pub tokens: u32,
    pub ms: u64,
}

/// A cat's endpoint.
pub struct Llm {
    client: Client,
    model: String,
    label: String,
}

impl Llm {
    /// `base_url` is a server root — `http://127.0.0.1:8081` for a
    /// `llama-server`, `http://localhost:11434` for ollama. Both get `/v1/`
    /// appended and are driven through the OpenAI-compatible adapter.
    ///
    /// The resolver is what makes one endpoint per cat possible, which is the
    /// point: four cats against one server serialize their turns, and the whole
    /// design rests on turns running concurrently while the chain ticks
    /// straight through them.
    pub fn local(base_url: &str, model: &str) -> Self {
        let url = format!("{}/v1/", base_url.trim_end_matches('/'));
        let endpoint = Endpoint::from_owned(url);
        let resolver = ServiceTargetResolver::from_resolver_fn(
            move |target: ServiceTarget| -> Result<ServiceTarget, genai::resolver::Error> {
                Ok(ServiceTarget {
                    endpoint: endpoint.clone(),
                    // A local server wants no key, but the adapter wants the
                    // field present.
                    auth: AuthData::from_single("local"),
                    model: ModelIden::new(AdapterKind::OpenAI, target.model.model_name),
                })
            },
        );
        Llm {
            client: Client::builder().with_service_target_resolver(resolver).build(),
            model: model.to_string(),
            label: format!("{model} @ {}", base_url.rsplit('/').next().unwrap_or(base_url)),
        }
    }

    /// A hosted provider, resolved by `genai` from the model name and the usual
    /// environment variables — `zai::glm-4.6`, `claude-…`, `gpt-…`, and so on.
    pub fn hosted(model: &str) -> Self {
        Llm {
            client: Client::default(),
            model: model.to_string(),
            label: model.to_string(),
        }
    }

    /// GLM on z.ai, with the key handed in rather than read from
    /// `ZAI_API_KEY` — `kot run --glm` reads it from a token file
    /// (`~/.akuma/z.ai/token` by default) so a service unit never has to
    /// carry the secret in its environment. A bare model name goes to the
    /// **coding-plan** endpoint (`zai-coding::`), because that's the kind of
    /// key this project has (checked 2026-09-22: the per-token `paas/v4` API
    /// answers it with "insufficient balance"). Spell `zai::glm-…` to use
    /// the per-token API instead.
    pub fn glm(token: &str, model: &str) -> Self {
        let token = token.trim().to_string();
        let client = Client::builder()
            .with_auth_resolver_fn(move |_: genai::ModelIden| -> Result<Option<AuthData>, genai::resolver::Error> {
                Ok(Some(AuthData::from_single(token.clone())))
            })
            .build();
        let model = if model.contains("::") { model.to_string() } else { format!("zai-coding::{model}") };
        Llm { client, label: model.clone(), model }
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub async fn turn(&self, system: &str, user: &str, tools: Vec<Tool>) -> Result<Turn, String> {
        let started = Instant::now();
        let req = ChatRequest::new(vec![ChatMessage::system(system), ChatMessage::user(user)])
            .with_tools(tools);
        let res = self
            .client
            .exec_chat(&self.model, req, None)
            .await
            .map_err(|e| format!("{}: {e}", self.label))?;

        let tokens = res.usage.completion_tokens.unwrap_or(0).max(0) as u32;
        let text = res.first_text().unwrap_or_default().to_string();
        let calls = res
            .into_tool_calls()
            .into_iter()
            .map(|c| Call { name: c.fn_name, args: c.fn_arguments })
            .collect();
        Ok(Turn { text, calls, tokens, ms: started.elapsed().as_millis() as u64 })
    }
}

/// The tool a cat uses to talk.
///
/// Separate from [`task_tools`] because a cat that is being *spoken to* has no
/// task to act on, and offering it the task-state verbs it cannot use is how a
/// small model ends up calling one of them anyway. [`local_tools`] carries no
/// task state, so those are fine here — a DM is exactly how an operator hands
/// a cat a one-off job outside the formal task lifecycle.
pub fn chat_tools() -> Vec<Tool> {
    let mut tools = vec![Tool::new("SendMessage")
        .with_description("Say something. Use this to reply.")
        .with_schema(serde_json::json!({
            "type": "object",
            "properties": {
                "to": {"type": "string",
                       "description": "a cat's name, or 'litter' for everyone"},
                "body": {"type": "string"}
            },
            "required": ["body"]
        }))];
    tools.extend(local_tools());
    tools.extend(note_tools());
    tools
}

/// Standalone notes: a markdown artifact with no task behind it, and no
/// clearance ceremony first. The chain-durable equivalent of "leaving a
/// sticky note for the litter" — publish one, or read what others left.
/// Offered everywhere ([`chat_tools`] and [`task_tools`]) because unlike
/// [`task_tools`]'s other verbs, none of these act on task state, so there is
/// no wake reason that makes them unsafe to offer.
fn note_tools() -> Vec<Tool> {
    vec![
        Tool::new("Artifact")
            .with_description(
                "Publish a standalone artifact to the chain, visible to everyone — markdown, no \
                 task required and no clearance needed first. NOT the same as TaskUpdate's \
                 status=artifact, which closes a specific task's own report: use THIS one for a \
                 finding, a hello, or a report that isn't the result of a task you were assigned.",
            )
            .with_schema(serde_json::json!({
                "type": "object",
                "properties": {"text": {"type": "string", "description": "markdown; the first '# ' line becomes its title"}},
                "required": ["text"]
            })),
        Tool::new("ArtifactList")
            .with_description(
                "List every artifact that exists right now — a closed task's report (id like \
                 't1') and every standalone one published with the Artifact tool (a bare id like \
                 '3') alike, one list, id/title/author each.",
            )
            .with_schema(serde_json::json!({"type": "object", "properties": {}})),
        Tool::new("ArtifactRead")
            .with_description("Read one artifact's full text by id, exactly as ArtifactList showed it — a task's ('t1') or a standalone one ('3').")
            .with_schema(serde_json::json!({
                "type": "object",
                "properties": {"id": {"type": "string", "description": "the artifact's id, from ArtifactList — keep its 't' prefix if it has one"}},
                "required": ["id"]
            })),
    ]
}

/// Stubs: local to this cat's own host, no sandbox. A turn is one LLM call
/// in, tool calls out — there is no loop that feeds a result back for a
/// further reply, so these don't help decide what to do next; use them to do
/// work, then a separate `SendMessage`/`TaskUpdate` to report it.
fn local_tools() -> Vec<Tool> {
    vec![
        Tool::new("Bash")
            .with_description("Run one shell command on this cat's own host (/bin/sh -c).")
            .with_schema(serde_json::json!({
                "type": "object",
                "properties": {"command": {"type": "string"}},
                "required": ["command"]
            })),
        Tool::new("ReadFile")
            .with_description("Read one text file from this cat's own host.")
            .with_schema(serde_json::json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            })),
        Tool::new("WriteFile")
            .with_description("Write text to a file on this cat's own host, overwriting it.")
            .with_schema(serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"}
                },
                "required": ["path", "content"]
            })),
    ]
}

/// The public tool surface, as the model sees it.
///
/// Three tools, not eight. `TaskUpdate` carries a `status` enum rather than
/// being split into claim/done/failed/clear/reopen/artifact, because a small
/// model picks a *value* more reliably than it picks among near-identical tool
/// names — and a new act then costs a value instead of new surface.
/// `TaskPlan` and `TaskReassign` are separate because they are leader acts that
/// take another cat's name rather than text.
pub fn task_tools() -> Vec<Tool> {
    let mut tools = vec![
        Tool::new("TaskUpdate")
            .with_description("Act on one task. Use the status you were told to use.")
            .with_schema(serde_json::json!({
                "type": "object",
                "properties": {
                    "task": {"type": "string",
                             "description": "the exact id you were given: a PARENT id like t1 \
                                              (only for status=artifact) or a SUB-TASK id like \
                                              t1.2 (for claim/done/failed/clear/reopen) — never \
                                              the parent id where a sub-task id is asked for"},
                    "status": {"type": "string",
                               "enum": ["claim","done","failed","clear","reopen","artifact"]},
                    "text": {"type": "string",
                             "description": "your result, or the report for status=artifact"}
                },
                "required": ["task", "status"]
            })),
        Tool::new("TaskPlan")
            .with_description(
                "Leader only. Split a parent task into directed sub-tasks, all in ONE call.",
            )
            .with_schema(serde_json::json!({
                "type": "object",
                "properties": {
                    "task": {"type": "string"},
                    "assignments": {"type": "array", "items": {
                        "type": "object",
                        "properties": {
                            "who": {"type": "string", "description": "the cat's name"},
                            "what": {"type": "string"}
                        },
                        "required": ["who", "what"]
                    }}
                },
                "required": ["task", "assignments"]
            })),
        Tool::new("TaskReassign")
            .with_description(
                "Leader only. Move a sub-task to a different cat when its current one \
                 cannot do it — it went silent, or it reported failed.",
            )
            .with_schema(serde_json::json!({
                "type": "object",
                "properties": {
                    "task": {"type": "string", "description": "the sub-task id, e.g. t1.1"},
                    "to": {"type": "string", "description": "the cat to move it to"}
                },
                "required": ["task", "to"]
            })),
    ];
    tools.extend(local_tools());
    tools.extend(note_tools());
    tools
}

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

/// Above this, a session force-compacts rather than waiting for the model
/// to call [`compact_tool`] itself — past this point a turn risks not
/// fitting the window at all.
pub const FORCE_COMPACT_PCT: u32 = 98;

/// The next checkpoint strictly above `last_warned`, at or below `used_pct`
/// — `None` if nothing new was crossed since the last check. 25% first,
/// then every 10 up to 80%, then every 2% (the last stretch is where a
/// session actually runs out, so it gets finer warning).
pub fn budget_checkpoint(used_pct: u32, last_warned: u32) -> Option<u32> {
    let cps = std::iter::once(25).chain((30..=80).step_by(10)).chain((82..=100).step_by(2));
    cps.filter(|&c| c > last_warned && c <= used_pct).max()
}

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
    /// Completion tokens only — kept for existing callers' `{tokens} tok`
    /// logging.
    pub tokens: u32,
    /// The size of what was actually sent this call (prompt) — meaningful
    /// for [`Llm::converse`], where the prompt grows to hold the whole
    /// history every turn, so this doubles as "context used so far."
    pub prompt_tokens: u32,
    /// `prompt_tokens + tokens`, or the provider's own total if it reports
    /// one directly.
    pub total_tokens: u32,
    pub ms: u64,
}

/// A cat's endpoint.
pub struct Llm {
    client: Client,
    model: String,
    label: String,
    // Only `Llm::local` can answer this (a raw `/v1/models` GET; no
    // provider-agnostic way to ask a hosted model its context length), and
    // only ever needs answering once — a chat/model pair's window doesn't
    // change mid-session.
    base_url: Option<String>,
    http: reqwest::Client,
    context_window: tokio::sync::OnceCell<Option<u32>>,
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
            base_url: Some(base_url.trim_end_matches('/').to_string()),
            http: reqwest::Client::new(),
            context_window: tokio::sync::OnceCell::new(),
        }
    }

    /// A hosted provider, resolved by `genai` from the model name and the usual
    /// environment variables — `zai::glm-4.6`, `claude-…`, `gpt-…`, and so on.
    pub fn hosted(model: &str) -> Self {
        Llm {
            client: Client::default(),
            model: model.to_string(),
            label: model.to_string(),
            base_url: None,
            http: reqwest::Client::new(),
            context_window: tokio::sync::OnceCell::new(),
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
        Llm { client, label: model.clone(), model, base_url: None, http: reqwest::Client::new(), context_window: tokio::sync::OnceCell::new() }
    }

    /// This model's context window, if it can be determined at all — only
    /// `Llm::local` can (one cached `GET {base}/v1/models`, reading
    /// `data[0].meta.n_ctx`); a hosted provider has no such endpoint here,
    /// so this stays `None` rather than guessing a number that isn't
    /// verified for the specific model in use.
    pub async fn context_window(&self) -> Option<u32> {
        *self
            .context_window
            .get_or_init(|| async {
                let base = self.base_url.as_ref()?;
                let v: serde_json::Value = self.http.get(format!("{base}/v1/models")).send().await.ok()?.json().await.ok()?;
                v.get("data")?.as_array()?.first()?.get("meta")?.get("n_ctx")?.as_u64().map(|n| n as u32)
            })
            .await
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub async fn turn(&self, system: &str, user: &str, tools: Vec<Tool>) -> Result<Turn, String> {
        self.converse(system, &[(Speaker::User, user.to_string())], tools).await
    }

    /// Like [`Llm::turn`], but with prior turns folded in as real
    /// assistant/user messages instead of flattened into one `user` string —
    /// for a plain back-and-forth (`kot chat`) where there is no chain to
    /// carry the question between turns the way the agent loop does.
    pub async fn converse(&self, system: &str, history: &[(Speaker, String)], tools: Vec<Tool>) -> Result<Turn, String> {
        let started = Instant::now();
        let mut messages = vec![ChatMessage::system(system)];
        messages.extend(history.iter().map(|(who, text)| match who {
            Speaker::User => ChatMessage::user(text.clone()),
            Speaker::Assistant => ChatMessage::assistant(text.clone()),
        }));
        let req = ChatRequest::new(messages).with_tools(tools);
        let res = self
            .client
            .exec_chat(&self.model, req, None)
            .await
            .map_err(|e| format!("{}: {e}", self.label))?;

        let tokens = res.usage.completion_tokens.unwrap_or(0).max(0) as u32;
        let prompt_tokens = res.usage.prompt_tokens.unwrap_or(0).max(0) as u32;
        let total_tokens = res.usage.total_tokens.map(|t| t.max(0) as u32).unwrap_or(prompt_tokens + tokens);
        let text = res.first_text().unwrap_or_default().to_string();
        let calls = res
            .into_tool_calls()
            .into_iter()
            .map(|c| Call { name: c.fn_name, args: c.fn_arguments })
            .collect();
        Ok(Turn { text, calls, tokens, prompt_tokens, total_tokens, ms: started.elapsed().as_millis() as u64 })
    }
}

/// Who said a turn, for [`Llm::converse`]'s history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Speaker {
    User,
    Assistant,
}

/// The tool a cat uses to talk.
///
/// Separate from [`task_tools`] because a cat that is being *spoken to* has no
/// task to act on, and offering it the task-state verbs it cannot use is how a
/// small model ends up calling one of them anyway. [`local_tools`] carries no
/// task state, so those are fine here — a DM is exactly how an operator hands
/// a cat a one-off job outside the formal task lifecycle.
pub fn chat_tools() -> Vec<Tool> {
    let mut tools = vec![send_message_tool()];
    tools.extend(local_tools());
    tools.extend(note_tools());
    tools
}

/// Factored out of [`chat_tools`] so `kot chat` (no node, so no [`note_tools`])
/// can still offer the same reply channel a cat gets when spoken to.
pub fn send_message_tool() -> Tool {
    Tool::new("SendMessage")
        .with_description("Say something. Use this to reply.")
        .with_schema(serde_json::json!({
            "type": "object",
            "properties": {
                "to": {"type": "string",
                       "description": "a cat's name, or 'litter' for everyone"},
                "body": {"type": "string"},
                "no_ack": {"type": "boolean",
                           "description": "true if this is a closing remark or acknowledgment \
                                            that doesn't itself need a reply — set it on your \
                                            own closing messages to stop a back-and-forth from \
                                            looping forever. Defaults to false."},
                "off_record": {"type": "boolean",
                                "description": "true to keep this message out of the chain's \
                                                 block log entirely — it still reaches whoever \
                                                 it's addressed to live, it just never gets \
                                                 committed. If you were woken by a message marked \
                                                 off_record, set this to true on your reply too, \
                                                 or your reply commits even though the message you're \
                                                 answering never did. Defaults to false."}
            },
            "required": ["body"]
        }))
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
        // Root only, chain-side (`pallet_litter::Call::request_compaction`
        // rejects anyone else with NotAuthorized) — offered everywhere
        // anyway, same reasoning as the rest of this list: it never touches
        // task state, so there is no wake reason to withhold it, and the
        // chain itself is the real gate, not which tools a cat is shown.
        Tool::new("RequestCompaction")
            .with_description(
                "Ask the node to snapshot state and shrink the block log now, instead of \
                 waiting for the next /clear. Root only — any other caller is refused. Touches \
                 no task; nothing to report back beyond whether it was accepted.",
            )
            .with_schema(serde_json::json!({"type": "object", "properties": {}})),
        // Found live 2026-09-23: a cat asked to address "your littermate"
        // with no name given invented a plausible-sounding one and tried
        // to SendMessage it. The roster is now stated in every `said`
        // prompt (`Cat::prompt`) so that specific guess never has to
        // happen again, but who's actually *live* right now — as opposed
        // to just genesis-configured — still isn't something a cat could
        // ask before this.
        Tool::new("Peers")
            .with_description(
                "Who else is in this litter, by name, and which of them is the mesh's current \
                 primary (the node that actually seals blocks — separate from the litter's \
                 planning leader).",
            )
            .with_schema(serde_json::json!({"type": "object", "properties": {}})),
        Tool::new("Stats")
            .with_description(
                "Everyone's latest self-reported work stats this session — turns taken, tool \
                 calls made, tokens spent, milliseconds spent thinking — by name, yours \
                 included.",
            )
            .with_schema(serde_json::json!({"type": "object", "properties": {}})),
    ]
}

/// Stubs: local to this cat's own host, no sandbox. A turn is one LLM call
/// in, tool calls out — there is no loop that feeds a result back for a
/// further reply, so these don't help decide what to do next; use them to do
/// work, then a separate `SendMessage`/`TaskUpdate` to report it.
pub fn local_tools() -> Vec<Tool> {
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

/// Session/context-window management — offered only where a session
/// actually accumulates history across turns (`kot chat`; not `agent.rs`,
/// whose turns are stateless per wake and so have nothing to compact).
/// `TokenBudget`, `BrowseTools` and `Inspect` are the one exception in this
/// project to "a local tool's result is never fed back to the model":
/// their entire purpose is to put something back in front of the model on
/// request, so the caller feeds their result into the next turn's history
/// rather than only printing it. `BrowseTools`/`Inspect` is the same
/// list-then-read shape as `ArtifactList`/`ArtifactRead`, for the same
/// reason: naming an id without first being able to see what it is would
/// just be guessing blind.
pub fn budget_tools() -> Vec<Tool> {
    vec![
        Tool::new("TokenBudget")
            .with_description("Check how much of your context window this session has used.")
            .with_schema(serde_json::json!({"type": "object", "properties": {}})),
        Tool::new("BrowseTools")
            .with_description(
                "List every tool-call result stored this session — id, tool name, one-line \
                 preview — so you can pick one to Inspect. Survives Compact; the conversation \
                 doesn't.",
            )
            .with_schema(serde_json::json!({"type": "object", "properties": {}})),
        Tool::new("Compact")
            .with_description(
                "Replace your own conversation history with a summary you write, to free up \
                 context. Past tool-call results are NOT cleared by this — BrowseTools and \
                 Inspect one back if you need it after compacting.",
            )
            .with_schema(serde_json::json!({
                "type": "object",
                "properties": {"summary": {"type": "string", "description": "everything about the conversation so far worth remembering"}},
                "required": ["summary"]
            })),
        Tool::new("Inspect")
            .with_description("Pull one of your own past tool-call results (by id, from BrowseTools) back into view.")
            .with_schema(serde_json::json!({
                "type": "object",
                "properties": {"id": {"type": "integer", "description": "an id BrowseTools listed"}},
                "required": ["id"]
            })),
    ]
}

/// Self-identity — persona, model, platform, build version — for a model
/// that has no other way to introspect its own system prompt as data.
/// Same feedback exception as the rest of [`budget_tools`]: the caller
/// feeds the answer into history rather than only printing it.
pub fn about_me_tool() -> Tool {
    Tool::new("AboutMe")
        .with_description("Your own persona, model, and what host/build you're running on — check this if you're unsure who you are.")
        .with_schema(serde_json::json!({"type": "object", "properties": {}}))
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

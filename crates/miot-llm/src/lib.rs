//! What a cat thinks with.
//!
//! One provider so far — Ollama's `/api/chat`, which speaks native tool calls
//! and needs no key. The shape here is deliberately the *tool-call* shape and
//! not a text-completion one: the litter's finding is that a small model picks
//! a **value** far more reliably than it spells a bracket syntax, and a tool
//! call arrives already parsed with its arguments in fields.
//!
//! No streaming yet. A turn is seconds and nothing downstream can use a
//! partial one — the agent submits an extrinsic or it does not.

use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone, Serialize)]
pub struct Msg {
    pub role: &'static str,
    pub content: String,
}

impl Msg {
    pub fn system(c: impl Into<String>) -> Self {
        Msg { role: "system", content: c.into() }
    }
    pub fn user(c: impl Into<String>) -> Self {
        Msg { role: "user", content: c.into() }
    }
}

/// One tool call the model asked for, arguments already parsed.
#[derive(Debug, Clone)]
pub struct Call {
    pub name: String,
    pub args: serde_json::Value,
}

impl Call {
    pub fn str(&self, k: &str) -> Option<String> {
        self.args.get(k)?.as_str().map(str::to_string)
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

#[derive(Deserialize)]
struct RawResp {
    message: RawMsg,
    #[serde(default)]
    eval_count: u32,
    #[serde(default)]
    total_duration: u64,
}

#[derive(Deserialize)]
struct RawMsg {
    #[serde(default)]
    content: String,
    #[serde(default)]
    tool_calls: Vec<RawCall>,
}

#[derive(Deserialize)]
struct RawCall {
    function: RawFn,
}

#[derive(Deserialize)]
struct RawFn {
    name: String,
    #[serde(default)]
    arguments: serde_json::Value,
}

pub struct Ollama {
    client: reqwest::Client,
    url: String,
    model: String,
}

impl Ollama {
    pub fn new(host: &str, model: &str) -> Self {
        Ollama {
            client: reqwest::Client::builder()
                // A turn is the loose loop: minutes is normal, and nothing
                // upstream is waiting on it. The chain ticks straight through.
                .timeout(Duration::from_secs(600))
                .build()
                .expect("http client"),
            url: format!("{host}/api/chat"),
            model: model.to_string(),
        }
    }

    pub async fn turn(
        &self,
        msgs: &[Msg],
        tools: &serde_json::Value,
    ) -> Result<Turn, String> {
        let body = serde_json::json!({
            "model": self.model,
            "stream": false,
            "messages": msgs,
            "tools": tools,
            // Low temperature: we want the model to pick the verb it was told
            // to pick, not to be interesting about it.
            "options": { "temperature": 0.2 },
        });
        let resp = self
            .client
            .post(&self.url)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("ollama unreachable: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("ollama {}: {}", resp.status(), resp.text().await.unwrap_or_default()));
        }
        let raw: RawResp = resp.json().await.map_err(|e| format!("bad ollama json: {e}"))?;
        Ok(Turn {
            text: raw.message.content,
            calls: raw
                .message
                .tool_calls
                .into_iter()
                .map(|c| Call { name: c.function.name, args: c.function.arguments })
                .collect(),
            tokens: raw.eval_count,
            ms: raw.total_duration / 1_000_000,
        })
    }
}

/// The public tool surface, as the model sees it.
///
/// Two tools, not six. `TaskUpdate` carries a `status` enum rather than being
/// split into claim/done/failed/clear/artifact, because a small model picks a
/// *value* more reliably than it picks among near-identical tool names — and a
/// new act then costs a value instead of new surface.
pub fn task_tools() -> serde_json::Value {
    serde_json::json!([
      {"type":"function","function":{
        "name":"TaskUpdate",
        "description":"Act on one task. Use the status you were told to use.",
        "parameters":{"type":"object","properties":{
          "task":{"type":"string","description":"task id, e.g. t1 or t1.2"},
          "status":{"type":"string","enum":["claim","done","failed","clear","reopen","artifact"]},
          "text":{"type":"string","description":"your result, or the report for status=artifact"}
        },"required":["task","status"]}}},
      {"type":"function","function":{
        "name":"TaskPlan",
        "description":"Leader only. Split a parent task into directed sub-tasks, all in ONE call.",
        "parameters":{"type":"object","properties":{
          "task":{"type":"string"},
          "assignments":{"type":"array","items":{"type":"object","properties":{
            "who":{"type":"string","description":"the cat's name"},
            "what":{"type":"string"}
          },"required":["who","what"]}}
        },"required":["task","assignments"]}}}
    ])
}

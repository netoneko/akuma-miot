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

use serde::Serialize;
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

/// Which chat dialect an endpoint speaks.
///
/// `llama-server` and ollama both do native tool calls; they disagree only on
/// the envelope. Detected from the URL rather than configured, because getting
/// it wrong is a 404 and there is nothing to decide.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dialect {
    /// ollama `/api/chat`
    Ollama,
    /// OpenAI-compatible `/v1/chat/completions` — what `llama-server --jinja`
    /// serves.
    OpenAi,
}

pub struct Ollama {
    client: reqwest::Client,
    url: String,
    model: String,
    dialect: Dialect,
}

impl Ollama {
    /// `host` is a base URL. A `:11434` default port means ollama; anything
    /// else is assumed to be a `llama-server`, which is the common case for a
    /// swarm of one server per cat.
    pub fn new(host: &str, model: &str) -> Self {
        let dialect =
            if host.contains(":11434") { Dialect::Ollama } else { Dialect::OpenAi };
        Self::with_dialect(host, model, dialect)
    }

    pub fn with_dialect(host: &str, model: &str, dialect: Dialect) -> Self {
        Ollama {
            client: reqwest::Client::builder()
                // A turn is the loose loop: minutes is normal, and nothing
                // upstream is waiting on it. The chain ticks straight through.
                .timeout(Duration::from_secs(600))
                .build()
                .expect("http client"),
            url: match dialect {
                Dialect::Ollama => format!("{host}/api/chat"),
                Dialect::OpenAi => format!("{host}/v1/chat/completions"),
            },
            model: model.to_string(),
            dialect,
        }
    }

    pub fn dialect(&self) -> Dialect {
        self.dialect
    }

    pub async fn turn(
        &self,
        msgs: &[Msg],
        tools: &serde_json::Value,
    ) -> Result<Turn, String> {
        // Low temperature: we want the model to pick the verb it was told to
        // pick, not to be interesting about it. The two dialects spell that —
        // and the cap that stops a reasoning model spending its whole budget
        // without emitting a call — in different places.
        let body = match self.dialect {
            Dialect::Ollama => serde_json::json!({
                "model": self.model,
                "stream": false,
                "messages": msgs,
                "tools": tools,
                "options": { "temperature": 0.2, "num_predict": 2048 },
            }),
            Dialect::OpenAi => serde_json::json!({
                "model": self.model,
                "stream": false,
                "messages": msgs,
                "tools": tools,
                "temperature": 0.2,
                "max_tokens": 2048,
            }),
        };
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
        let started = std::time::Instant::now();
        let v: serde_json::Value =
            resp.json().await.map_err(|e| format!("bad json: {e}"))?;
        let msg = match self.dialect {
            Dialect::Ollama => v.get("message").cloned().unwrap_or_default(),
            Dialect::OpenAi => v
                .pointer("/choices/0/message")
                .cloned()
                .ok_or_else(|| format!("no choices in response: {v}"))?,
        };
        let text = msg.get("content").and_then(|c| c.as_str()).unwrap_or_default().to_string();
        let calls = msg
            .get("tool_calls")
            .and_then(|t| t.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|c| {
                        let f = c.get("function")?;
                        let name = f.get("name")?.as_str()?.to_string();
                        // ollama gives arguments as an object; the
                        // OpenAI-compatible shape gives a JSON *string*.
                        let args = match f.get("arguments") {
                            Some(serde_json::Value::String(s)) => {
                                serde_json::from_str(s).unwrap_or(serde_json::Value::Null)
                            }
                            Some(other) => other.clone(),
                            None => serde_json::Value::Null,
                        };
                        Some(Call { name, args })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let tokens = v
            .get("eval_count")
            .or_else(|| v.pointer("/usage/completion_tokens"))
            .and_then(|n| n.as_u64())
            .unwrap_or(0) as u32;
        let ms = v
            .get("total_duration")
            .and_then(|n| n.as_u64())
            .map(|ns| ns / 1_000_000)
            .unwrap_or_else(|| started.elapsed().as_millis() as u64);
        Ok(Turn { text, calls, tokens, ms })
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
        "name":"TaskReassign",
        "description":"Leader only. Move a sub-task to a different cat when its current one cannot do it — it went silent, or it reported failed.",
        "parameters":{"type":"object","properties":{
          "task":{"type":"string","description":"the sub-task id, e.g. t1.1"},
          "to":{"type":"string","description":"the cat to move it to"}
        },"required":["task","to"]}}},
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

//! `kot chat` — talk to a model directly, in this process. No node, no
//! chain, no signing: just [`miot_llm::Llm::converse`] and a terminal.
//!
//! Exists for the case `agent.rs` doesn't cover: trying a model, a persona,
//! or its tool-calling out loud before wiring it into the litter at all —
//! so it offers the same [`miot_llm::local_tools`] a cat gets (`Bash`,
//! `ReadFile`, `WriteFile`) and runs them right here, rather than routing
//! through a node that isn't running. `Artifact`/`ArtifactList`/
//! `ArtifactRead` are left out: those publish to the chain, and there is
//! none here to publish to. Unlike a cat's turn (stateless, the chain
//! carries the question) this keeps real conversation history in memory for
//! as long as the process runs — there is nothing else here to carry it.

use crate::common::{DIM, OFF};
use miot_llm::{Call, Llm, Speaker};
use std::io::Write;

pub struct ChatConfig {
    pub llm: Llm,
    pub persona: String,
}

/// Appended to the persona so the model actually reaches for a tool instead
/// of just talking — `agent.rs`'s `AGENT_RULES` exists for the same reason.
const CHAT_RULES: &str = "\n\nRules:\n\
- You may call Bash, ReadFile, or WriteFile to actually do something on this \
machine before replying — their output is shown to the operator, not fed \
back to you, so make each call self-contained.\n\
- Always call SendMessage with your reply, even if you called nothing else. \
Never answer in plain text alone.";

fn tools() -> Vec<miot_llm::Tool> {
    let mut t = vec![miot_llm::send_message_tool()];
    t.extend(miot_llm::local_tools());
    t
}

/// Same three stubs as `agent::Cat::act_local`, minus the node-backed
/// artifact tools and the `[name]` log prefix a multi-cat litter needs to
/// tell turns apart — there's only one of us here.
async fn act_local(c: &Call) -> String {
    match c.name.as_str() {
        "Bash" => {
            let command = c.str("command").unwrap_or_default();
            let run = tokio::process::Command::new("/bin/sh").arg("-c").arg(&command).output();
            match tokio::time::timeout(std::time::Duration::from_secs(30), run).await {
                Ok(Ok(out)) => format!(
                    "$ {command}  (exit {:?})\n{}{}",
                    out.status.code(),
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr),
                ),
                Ok(Err(e)) => format!("$ {command}  failed to spawn: {e}"),
                Err(_) => format!("$ {command}  timed out after 30s"),
            }
        }
        "ReadFile" => {
            let path = c.str("path").unwrap_or_default();
            match tokio::fs::read_to_string(&path).await {
                Ok(s) => format!("read {path} ({} bytes):\n{s}", s.len()),
                Err(e) => format!("read {path} failed: {e}"),
            }
        }
        "WriteFile" => {
            let path = c.str("path").unwrap_or_default();
            let content = c.str("content").unwrap_or_default();
            match tokio::fs::write(&path, &content).await {
                Ok(()) => format!("wrote {path} ({} bytes)", content.len()),
                Err(e) => format!("write {path} failed: {e}"),
            }
        }
        other => format!("(no such tool: {other})"),
    }
}

pub async fn run(cfg: ChatConfig) {
    println!("talking to {} — {}", cfg.llm.label(), DIM.to_string() + "/quit or Ctrl-D to leave" + OFF);
    println!();

    let system = format!("{}{CHAT_RULES}", cfg.persona);
    let mut history: Vec<(Speaker, String)> = Vec::new();
    let stdin = std::io::stdin();

    loop {
        print!("> ");
        let _ = std::io::stdout().flush();

        let mut line = String::new();
        if stdin.read_line(&mut line).unwrap_or(0) == 0 {
            println!();
            break;
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line == "/quit" || line == "/exit" {
            break;
        }

        history.push((Speaker::User, line.to_string()));
        match cfg.llm.converse(&system, &history, tools()).await {
            Ok(turn) => {
                let mut said: Option<String> = None;
                let mut ran: Vec<&str> = Vec::new();
                for call in &turn.calls {
                    if call.name == "SendMessage" {
                        said = call.str("body");
                        continue;
                    }
                    ran.push(&call.name);
                    println!("{DIM}  {}{OFF}", act_local(call).await.replace('\n', "\n  "));
                }
                // Always answer — a model that only reached for Bash, or
                // that replied in plain text instead of calling
                // SendMessage, still gets treated as having said something.
                let reply = said
                    .or_else(|| Some(turn.text.trim()).filter(|s| !s.is_empty()).map(str::to_string))
                    .unwrap_or_else(|| if ran.is_empty() { "(no reply)".to_string() } else { format!("(ran {} — no further reply)", ran.join(", ")) });
                println!("{reply}\n{DIM}({} tok, {:.1}s){OFF}\n", turn.tokens, turn.ms as f64 / 1000.0);
                history.push((Speaker::Assistant, reply));
            }
            Err(e) => {
                println!("{DIM}error: {e}{OFF}\n");
                // A failed turn never happened, as far as history is
                // concerned — leaving the user line in would have the model
                // "answer" it a second time, out of order, next turn.
                history.pop();
            }
        }
    }
}

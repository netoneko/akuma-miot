//! `kot chat` — talk to a model directly, in this process. No node, no
//! chain, no signing: just [`miot_llm::Llm::converse`] and a terminal.
//!
//! Exists for the case `agent.rs` doesn't cover: trying a model, a persona,
//! or its tool-calling out loud before wiring it into the litter at all —
//! so it offers the same [`miot_llm::local_tools`] a cat gets (`Bash`,
//! `ReadFile`, `WriteFile`) and runs them right here, rather than routing
//! through a node that isn't running. `Artifact`/`ArtifactList`/
//! `ArtifactRead` are left out: those publish to the chain, and there is
//! none here to publish to.
//!
//! Unlike a cat's turn (stateless, the chain carries the question) this
//! keeps real conversation history in memory for as long as the process
//! runs — the one place in this project where a session actually
//! accumulates context and can run out of room, so it's the one place
//! that tracks a token budget and can compact ([`miot_llm::budget_tools`]).

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
Never answer in plain text alone.\n\
- TokenBudget tells you how much context you have left. BrowseTools lists \
past tool results (id, name, preview); Inspect one back by id for the full \
thing. Compact writes a summary of the conversation so far and replaces it, \
to free up room — past tool results survive a Compact. AboutMe reminds you \
of your own persona, model, and what you're running on.";

fn tools() -> Vec<miot_llm::Tool> {
    let mut t = vec![miot_llm::send_message_tool(), miot_llm::about_me_tool()];
    t.extend(miot_llm::local_tools());
    t.extend(miot_llm::budget_tools());
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

/// `used_pct` against `context_window`, if either is known — `None` means
/// "can't tell" (a hosted provider with no queryable window), not zero.
fn pct_used(total_tokens: u32, context_window: Option<u32>) -> Option<u32> {
    let window = context_window?;
    if window == 0 {
        return None;
    }
    Some(((total_tokens as u64 * 100) / window as u64) as u32)
}

fn budget_report(pct: Option<u32>, context_window: Option<u32>, total_tokens: u32, tool_log_len: usize) -> String {
    let usage = match (pct, context_window) {
        (Some(p), Some(w)) => format!("{total_tokens}/{w} tokens ({p}%) used last turn"),
        _ => format!("{total_tokens} tokens used last turn (context window unknown for this model)"),
    };
    format!("{usage}. {} tool result(s) stored — BrowseTools to list them.", tool_log_len)
}

/// One line per stored result — name and a one-line preview, not just a
/// bare count — so the model can pick which id to `Inspect` instead of
/// guessing blind. The chain-side equivalent is `ArtifactList` before
/// `ArtifactRead`; `tool_log` needed the same two-step shape.
fn browse_tools_report(tool_log: &[(String, String)]) -> String {
    if tool_log.is_empty() {
        return "No tool results stored yet.".to_string();
    }
    let lines: Vec<String> = tool_log
        .iter()
        .enumerate()
        .map(|(id, (name, out))| {
            let preview: String = out.lines().next().unwrap_or("").chars().take(60).collect();
            format!("  {id}: {name} — {preview}")
        })
        .collect();
    format!("Tool results stored (Inspect{{id}} for the full one):\n{}", lines.join("\n"))
}

/// A model has no other way to see its own system prompt as data — this is
/// that, plus what host and build it's actually running on, since a small
/// model asked "what are you" otherwise has to guess from training data
/// instead of its actual instructions.
fn about_me_report(llm: &Llm, persona: &str) -> String {
    format!(
        "Model: {}\nPlatform: {} {}\nBuild: kot {}\nPersona:\n{persona}",
        llm.label(),
        std::env::consts::OS,
        std::env::consts::ARCH,
        crate::version::VERSION,
    )
}

/// One extra call, no tools, asking the model to summarize itself — used
/// both when it calls `Compact` and when a session is force-compacted at
/// `miot_llm::FORCE_COMPACT_PCT` without waiting for that.
async fn summarize(llm: &Llm, system: &str, history: &[(Speaker, String)]) -> String {
    let ask = "Summarize this conversation so far for your own future reference — what was asked, \
               what you found or did, what's still open. Plain text, no tools, as concise as it can \
               be while staying useful.";
    let mut h = history.to_vec();
    h.push((Speaker::User, ask.to_string()));
    match llm.converse(system, &h, Vec::new()).await {
        Ok(turn) if !turn.text.trim().is_empty() => turn.text.trim().to_string(),
        _ => "(compaction summary unavailable — history cleared anyway)".to_string(),
    }
}

pub async fn run(cfg: ChatConfig) {
    println!("talking to {} — {}", cfg.llm.label(), DIM.to_string() + "/quit or Ctrl-D to leave" + OFF);
    let context_window = cfg.llm.context_window().await;
    match context_window {
        Some(w) => println!("{DIM}context window: {w} tokens{OFF}"),
        None => println!("{DIM}context window: unknown for this model — budget warnings won't fire{OFF}"),
    }
    println!();

    let system = format!("{}{CHAT_RULES}", cfg.persona);
    let mut history: Vec<(Speaker, String)> = Vec::new();
    let mut tool_log: Vec<(String, String)> = Vec::new();
    let mut warned_tier: u32 = 0;
    let mut pending_warning: Option<String> = None;
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
        let system_now = match pending_warning.take() {
            Some(w) => format!("{system}\n\n{w}"),
            None => system.clone(),
        };
        match cfg.llm.converse(&system_now, &history, tools()).await {
            Ok(turn) => {
                let mut said: Option<String> = None;
                let mut ran: Vec<String> = Vec::new();
                let mut compacted = false;

                for call in &turn.calls {
                    match call.name.as_str() {
                        "SendMessage" => said = call.str("body"),
                        "Compact" => {
                            let summary = call.str("summary").unwrap_or_default();
                            println!("{DIM}  compacted: history replaced with a {}-char summary the model wrote. Tool results kept ({} stored).{OFF}", summary.len(), tool_log.len());
                            history = vec![(Speaker::Assistant, summary)];
                            warned_tier = 0;
                            compacted = true;
                        }
                        "TokenBudget" => {
                            let report = budget_report(pct_used(turn.total_tokens, context_window), context_window, turn.total_tokens, tool_log.len());
                            println!("{DIM}  {report}{OFF}");
                            history.push((Speaker::User, format!("[TokenBudget] {report}")));
                        }
                        "BrowseTools" => {
                            let report = browse_tools_report(&tool_log);
                            println!("{DIM}  {}{OFF}", report.replace('\n', "\n  "));
                            history.push((Speaker::User, format!("[BrowseTools] {report}")));
                        }
                        "AboutMe" => {
                            let report = about_me_report(&cfg.llm, &cfg.persona);
                            println!("{DIM}  {}{OFF}", report.replace('\n', "\n  "));
                            history.push((Speaker::User, format!("[AboutMe] {report}")));
                        }
                        "Inspect" => {
                            let id = call.args.get("id").and_then(|v| v.as_u64()).map(|n| n as usize);
                            let text = match id.and_then(|i| tool_log.get(i)) {
                                Some((name, out)) => format!("[Inspect #{} — {name}]\n{out}", id.unwrap()),
                                None => format!("[Inspect] no stored result with that id ({} stored: 0..{})", tool_log.len(), tool_log.len().saturating_sub(1)),
                            };
                            println!("{DIM}  {}{OFF}", text.replace('\n', "\n  "));
                            history.push((Speaker::User, text));
                        }
                        other => {
                            ran.push(other.to_string());
                            let out = act_local(call).await;
                            println!("{DIM}  {}{OFF}", out.replace('\n', "\n  "));
                            tool_log.push((other.to_string(), out));
                        }
                    }
                }

                // Always answer — a model that only reached for Bash, or
                // that replied in plain text instead of calling
                // SendMessage, still gets treated as having said something.
                let reply = said
                    .or_else(|| Some(turn.text.trim()).filter(|s| !s.is_empty()).map(str::to_string))
                    .unwrap_or_else(|| if ran.is_empty() { "(no reply)".to_string() } else { format!("(ran {} — no further reply)", ran.join(", ")) });
                println!("{reply}\n{DIM}({} tok, {:.1}s){OFF}\n", turn.tokens, turn.ms as f64 / 1000.0);
                history.push((Speaker::Assistant, reply));

                // Budget check, against what this turn actually cost — not
                // affected by whatever got pushed into `history` above,
                // since that's for the *next* turn to pay for.
                if let Some(pct) = pct_used(turn.total_tokens, context_window) {
                    if pct >= miot_llm::FORCE_COMPACT_PCT && !compacted {
                        println!("{DIM}  {pct}% of context window used — force-compacting before continuing.{OFF}");
                        let summary = summarize(&cfg.llm, &system, &history).await;
                        println!("{DIM}  compacted: {} tool results kept, Inspect to pull one back.{OFF}\n", tool_log.len());
                        history = vec![(Speaker::Assistant, summary)];
                        warned_tier = 0;
                    } else if let Some(tier) = miot_llm::budget_checkpoint(pct, warned_tier) {
                        warned_tier = tier;
                        pending_warning = Some(format!(
                            "⚠ context budget: {pct}% of your window used. Consider calling Compact soon \
                             (past tool results survive it) — this session force-compacts at {}%.",
                            miot_llm::FORCE_COMPACT_PCT
                        ));
                    }
                }
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

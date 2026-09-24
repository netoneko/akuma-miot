//! `kot chat` — talk to a model directly, in this process. No node, no
//! chain, no signing: a terminal hosting [`crate::agent_state_machine`], the same loop a
//! cat runs under `kot run`.
//!
//! Exists for trying a model, a persona, or its tool-calling out loud
//! before wiring it into the litter at all — so everything but where input
//! comes from and where a reply goes is the cat's own logic: the same
//! shared tools (`Bash`/`ReadFile`/`WriteFile`, `AboutMe`, the budget
//! tools), results fed back the same way, the same compaction.
//! `Artifact*`/`Peers`/`Stats` and the task verbs are left out: those talk
//! to a node, and there is none here.

use crate::agent_state_machine::{self, Dispatch, Host, Inbound};
use crate::ui;
use miot_llm::{Call, Llm, Tool};
use std::io::Write;
use std::sync::Arc;

pub struct ChatConfig {
    pub name: String,
    pub llm: Llm,
    pub persona: String,
}

struct Terminal {
    name: String,
}

const CHAT_RULES: &str = "\n\nYou are talking to your operator directly, in a terminal:\n\
- Always answer with SendMessage, even if you called nothing else. Never answer in \
plain text alone.\n\
- If you need a tool's output to answer, call it now and answer with SendMessage once \
its result comes back — don't guess at what it will say.";

impl Host for Terminal {
    fn name(&self) -> &str {
        &self.name
    }
    fn tools(&self, _kind: &'static str) -> Vec<Tool> {
        vec![miot_llm::send_message_tool()]
    }
    fn rules(&self) -> &'static str {
        CHAT_RULES
    }
    fn dispatch(&self, c: &Call) -> Dispatch {
        match c.name.as_str() {
            "SendMessage" => {
                let line = ui::reply(&self.name, &c.str("body").unwrap_or_default());
                Dispatch::Record(Box::pin(async move {
                    println!("{line}");
                    None
                }))
            }
            _ => Dispatch::Unknown,
        }
    }
    fn about(&self) -> String {
        "Where: kot chat — a terminal, no node, no chain".to_string()
    }
    /// A plain-text answer is still an answer — shown as one.
    fn spoke(&self, text: &str, _ctx: &serde_json::Value) {
        println!("{}", ui::reply(&self.name, text));
    }
    fn idle(&self) {
        print!("\n> ");
        let _ = std::io::stdout().flush();
    }
    /// The operator is right here; a stall is one "go on" away.
    fn check_before_idle(&self) -> bool {
        false
    }
}

pub async fn run(cfg: ChatConfig) {
    println!("talking to {} as {} — {}", cfg.llm.label(), ui::sealed(&cfg.name), ui::dim("/clear to start over · /quit or Ctrl-D to leave"));
    match cfg.llm.context_window().await {
        Some(w) => println!("{}", ui::dim(&format!("context window: {w} tokens"))),
        None => println!("{}", ui::dim("context window: unknown for this model — budget warnings won't fire")),
    }

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    // stdin is blocking; it gets its own thread and just feeds the inbox.
    // Dropping `tx` (EOF, /quit) closes it, and the loop ends once nothing
    // is left in flight.
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        loop {
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
            // Forget the conversation — the same reset a cat gets when the
            // chain's checkpoint moves, so it also drops anything still in
            // flight from before it.
            if line == "/clear" {
                if tx.send(Inbound::Reset("operator /clear".into())).is_err() {
                    break;
                }
                continue;
            }
            if tx.send(Inbound::Wake { text: line.to_string(), kind: "chat", ctx: serde_json::Value::Null }).is_err() {
                break;
            }
        }
    });

    agent_state_machine::run(Arc::new(Terminal { name: cfg.name }), cfg.llm, cfg.persona, rx).await;
}

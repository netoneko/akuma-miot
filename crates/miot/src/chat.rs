//! Talk to the litter.
//!
//! The operator types a line; it becomes a `say` extrinsic signed as root;
//! every cat it wakes takes a turn and replies with `SendMessage`, which is
//! another extrinsic. Everything on screen went through the chain.
//!
//! Output is written and flushed as it happens rather than collected, because
//! a turn is tens of seconds and a litter that prints nothing for two minutes
//! looks identical to a litter that has died.

use miot_llm::{chat_tools, Llm};
use miot_primitives::Effect;
use miot_runtime::{AccountId, Litter, RuntimeOrigin, System};
use polkadot_sdk::*;

use frame_support::traits::OnInitialize;
use std::io::{BufRead, Write};

use crate::live::{account, cat_name, personas, Bench, CATS, ROOT};
use crate::{colour, drain, name, new_ext, DIM, OFF};

fn out(s: &str) {
    print!("{s}");
    let _ = std::io::stdout().flush();
}

const BROADCAST_ALIASES: [&str; 3] = ["all", "cats", "litter"];

/// `@name` tags in a line → who to `say` to. Explicit broadcast
/// (`@all`/`@cats`/`@litter`) short-circuits to an empty target list (the
/// caller's "no targets" case already broadcasts); otherwise every resolved
/// `@name` becomes a target, in the order it appears, deduplicated. Unknown
/// tags are returned separately — a soft warning, not a refusal (see
/// docs/CLI.md §2).
fn parse_targets(line: &str) -> (Vec<AccountId>, Vec<String>) {
    let mut targets = Vec::new();
    let mut unknown = Vec::new();
    for word in line.split_whitespace() {
        let Some(tag) = word.strip_prefix('@') else { continue };
        let lower = tag.trim_end_matches(|c: char| !c.is_alphanumeric()).to_ascii_lowercase();
        if BROADCAST_ALIASES.contains(&lower.as_str()) {
            return (Vec::new(), Vec::new());
        }
        match account(&lower) {
            Some(a) => {
                if !targets.contains(&a) {
                    targets.push(a);
                }
            }
            None => unknown.push(lower),
        }
    }
    (targets, unknown)
}

pub async fn run(host: &str, model: &str, models: &str) {
    let bench = Bench::new(host, model, models);
    println!("  {DIM}chat — type to the litter, /clear to fail every open task, blank line or /quit to leave{OFF}");
    for who in CATS {
        println!(
            "    {}{:>5}{OFF} {DIM}{}{OFF}",
            colour(who.clone()),
            name(who.clone()),
            bench.for_cat(&who).label()
        );
    }
    println!("{DIM}──────────────────────────────────────────────────────────────{OFF}");

    let mut ext = new_ext();
    let mut block = 1u64;
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();

    loop {
        out(&format!("\n{}root{OFF} ▸ ", colour(ROOT)));
        let Some(Ok(line)) = lines.next() else { break };
        let line = line.trim().to_string();
        if line.is_empty() || line == "/quit" {
            break;
        }
        if line == "/clear" {
            block += 1;
            let effects = ext.execute_with(|| {
                System::set_block_number(block);
                Litter::on_initialize(block);
                Litter::clear_all(RuntimeOrigin::signed(ROOT)).expect("root may clear");
                drain()
            });
            let n = effects.iter().filter(|e| matches!(e, Effect::Failed { .. })).count();
            println!("{DIM}  cleared {n} open task(s) — starting fresh on this chain{OFF}");
            continue;
        }

        // Targets: `@all`/`@cats`/`@litter` (or no tag at all) means
        // broadcast; one or more `@name` tags means one `say` per addressee,
        // in the order they appear. See docs/CLI.md §2/§8.
        let (targets, unknown) = parse_targets(&line);
        for bad in &unknown {
            println!("{DIM}  no such cat: @{bad}{OFF}");
        }

        let mut effects = Vec::new();
        if targets.is_empty() {
            block += 1;
            effects.extend(ext.execute_with(|| {
                System::set_block_number(block);
                Litter::on_initialize(block);
                Litter::say(RuntimeOrigin::signed(ROOT), None, line.clone())
                    .expect("root may speak");
                drain()
            }));
        } else {
            for t in &targets {
                block += 1;
                effects.extend(ext.execute_with(|| {
                    System::set_block_number(block);
                    Litter::on_initialize(block);
                    Litter::say(RuntimeOrigin::signed(ROOT), Some(t.clone()), line.clone())
                        .expect("root may speak");
                    drain()
                }));
            }
        }

        // Anything the chain says woke somebody gets a turn. The waking rule
        // lives in `Effect::wakes`, not here — this loop only obeys it.
        let mut woke: Vec<AccountId> = Vec::new();
        for e in &effects {
            if let Effect::Said { to, from_root, body, .. } = e {
                if !e.wakes() {
                    continue;
                }
                let targets: Vec<AccountId> = match to {
                    Some(t) => vec![t.clone()],
                    None if *from_root => CATS.to_vec(),
                    None => vec![],
                };
                for t in targets {
                    if !woke.contains(&t) {
                        woke.push(t);
                    }
                }
                let _ = body;
            }
        }

        for who in woke {
            out(&format!("{DIM}  …{}{OFF}\r", name(who.clone())));
            let prompt = format!(
                "The operator said to the litter:\n\"{line}\"\n\n\
                 Reply with SendMessage. Keep it to a couple of sentences. \
                 Say who you are and what you are for."
            );
            let turn = match bench
                .for_cat(&who)
                .turn(&personas(who.clone(), who == CATS[0], ""), &prompt, chat_tools())
                .await
            {
                Ok(t) => t,
                Err(e) => {
                    println!("{DIM}  {} unreachable: {e}{OFF}", name(who.clone()));
                    continue;
                }
            };

            // What it said, and — because a reply is a public act — the
            // extrinsic that carries it.
            let body = turn
                .calls
                .iter()
                .find(|c| c.name == "SendMessage")
                .and_then(|c| c.str("body"))
                .unwrap_or_else(|| turn.text.trim().to_string());
            if body.is_empty() {
                println!("{DIM}  {} said nothing{OFF}", name(who.clone()));
                continue;
            }
            block += 1;
            let _ = ext.execute_with(|| {
                System::set_block_number(block);
                Litter::say(RuntimeOrigin::signed(who.clone()), None, body.clone())
            });

            println!(
                "{}{:>5}{OFF}  {body}  {DIM}({} tok, {:.0}s){OFF}",
                colour(who.clone()),
                cat_name(who),
                turn.tokens,
                turn.ms as f64 / 1000.0
            );
        }
    }
    println!("\n{DIM}  {block} blocks. bye.{OFF}");
}

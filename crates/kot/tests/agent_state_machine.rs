//! `agent_state_machine` against a scripted model: a fake OpenAI-compatible
//! server (the same `/v1/chat/completions` a `llama-server` answers) that
//! replays canned replies and records every request, so each test can see
//! exactly what the model was shown — which is the whole point: tool
//! results must come back, records must not, wakes queued during a turn
//! must be folded into the next one, and nothing may loop forever.

use kot::activity::Activity;
use kot::agent_state_machine::{self, Dispatch, Host, Inbound, CHECK_IN, MAX_FOLLOWUPS};
use kot::ui::ToolOut;
use miot_llm::{Call, Llm, Tool};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

// ── the fake model ──────────────────────────────────────────────────────

/// One scripted reply: tool calls, or text, after an optional delay.
#[derive(Clone)]
struct Reply {
    calls: Vec<(&'static str, Value)>,
    text: Option<&'static str>,
    delay_ms: u64,
    /// Sent apart, as `reasoning_content` — GLM's way.
    reasoning: Option<&'static str>,
}

fn calls(c: Vec<(&'static str, Value)>) -> Reply {
    Reply { calls: c, text: None, delay_ms: 0, reasoning: None }
}
fn text(t: &'static str) -> Reply {
    Reply { calls: vec![], text: Some(t), delay_ms: 0, reasoning: None }
}
fn thinking(r: Reply, reasoning: &'static str) -> Reply {
    Reply { reasoning: Some(reasoning), ..r }
}
fn say(body: &'static str) -> Reply {
    calls(vec![("SendMessage", json!({"body": body}))])
}

#[derive(Default)]
struct Fake {
    requests: Mutex<Vec<Value>>,
    script: Mutex<VecDeque<Reply>>,
    /// Replayed forever once `script` runs out, if set.
    forever: Mutex<Option<Reply>>,
}

impl Fake {
    fn requests(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }
    /// The last user message of request `n` — what that turn was fed.
    fn fed(&self, n: usize) -> String {
        let r = &self.requests()[n];
        r["messages"].as_array().unwrap().iter().rev().find(|m| m["role"] == "user").map(|m| m["content"].as_str().unwrap_or("").to_string()).unwrap_or_default()
    }
    /// Every message's content in request `n`, joined.
    fn all(&self, n: usize) -> String {
        let r = &self.requests()[n];
        r["messages"].as_array().unwrap().iter().map(|m| m["content"].as_str().unwrap_or("").to_string()).collect::<Vec<_>>().join("\n---\n")
    }
    fn tool_names(&self, n: usize) -> Vec<String> {
        self.requests()[n]["tools"].as_array().map(|t| t.iter().filter_map(|t| t["function"]["name"].as_str().map(str::to_string)).collect()).unwrap_or_default()
    }
}

async fn serve(fake: Arc<Fake>) -> String {
    use axum::routing::{get, post};
    let f = fake.clone();
    let app = axum::Router::new()
        .route("/v1/models", get(|| async { axum::Json(json!({"data": [{"id": "fake", "meta": {"n_ctx": 100000}}]})) }))
        .route(
            "/v1/chat/completions",
            post(move |axum::Json(body): axum::Json<Value>| {
                let f = f.clone();
                async move {
                    f.requests.lock().unwrap().push(body);
                    let next = f.script.lock().unwrap().pop_front().or_else(|| f.forever.lock().unwrap().clone()).unwrap_or_else(|| text("(script ran out)"));
                    if next.delay_ms > 0 {
                        tokio::time::sleep(Duration::from_millis(next.delay_ms)).await;
                    }
                    let tool_calls: Vec<Value> = next
                        .calls
                        .iter()
                        .enumerate()
                        .map(|(i, (name, args))| json!({"id": format!("call_{i}"), "type": "function", "function": {"name": name, "arguments": args.to_string()}}))
                        .collect();
                    let mut message = json!({"role": "assistant", "content": next.text});
                    if let Some(r) = next.reasoning {
                        message["reasoning_content"] = json!(r);
                    }
                    if !tool_calls.is_empty() {
                        message["tool_calls"] = json!(tool_calls);
                    }
                    axum::Json(json!({
                        "id": "x", "object": "chat.completion", "created": 0, "model": "fake",
                        "choices": [{"index": 0, "message": message, "finish_reason": if tool_calls.is_empty() { "stop" } else { "tool_calls" }}],
                        "usage": {"prompt_tokens": 100, "completion_tokens": 10, "total_tokens": 110}
                    }))
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

// ── the fake host ───────────────────────────────────────────────────────

#[derive(Default)]
struct Seen {
    sent: Vec<String>,
    spoke: Vec<String>,
    shown: Vec<String>,
    turns: usize,
    /// (tools, messages) per turn, as reported to `after_turn`.
    costs: Vec<(usize, usize)>,
    /// `Host::check_before_idle` — off unless a test is about it, so every
    /// other test's request count means what it says.
    check: bool,
    /// Every live record handed to `Host::activity`, in order.
    activity: Vec<Activity>,
    transcript: Option<std::path::PathBuf>,
    reminder: Option<String>,
    /// `Host::stall_after` — the default (minutes) unless a test is about it.
    stall: Option<Duration>,
    /// `Host::local_nag_after` — the default (minutes) unless a test is about it.
    local_nag: Option<Duration>,
    /// `Host::history_path` — none unless a test is about restarts.
    history: Option<std::path::PathBuf>,
    /// `Host::restart_note`.
    restart: Option<String>,
}

struct TestHost(Arc<Mutex<Seen>>);

impl Host for TestHost {
    fn name(&self) -> &str {
        "tama"
    }
    fn tools(&self, kind: &'static str) -> Vec<Tool> {
        let mut t = vec![miot_llm::send_message_tool()];
        if kind == "task" {
            t.push(Tool::new("TaskOnly"));
        }
        t
    }
    fn rules(&self) -> &'static str {
        ""
    }
    fn dispatch(&self, c: &Call) -> Dispatch {
        match c.name.as_str() {
            "SendMessage" => {
                let seen = self.0.clone();
                let body = c.str("body").unwrap_or_default();
                Dispatch::Record(Box::pin(async move {
                    seen.lock().unwrap().sent.push(body.clone());
                    Some(ToolOut::new(body, true).meta("submitted"))
                }))
            }
            // A chain write that takes a while to land.
            "SlowSay" => {
                let ms = c.args.get("ms").and_then(|m| m.as_u64()).unwrap_or(0);
                Dispatch::Record(Box::pin(async move {
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                    Some(ToolOut::new("slow", true).meta("submitted"))
                }))
            }
            "Echo" => {
                let v = c.str("v").unwrap_or_default();
                let ms = c.args.get("ms").and_then(|m| m.as_u64()).unwrap_or(0);
                Dispatch::Query(Box::pin(async move {
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                    ToolOut::new("", true).body(format!("echo:{v}"))
                }))
            }
            _ => Dispatch::Unknown,
        }
    }
    fn about(&self) -> String {
        "Where: a test".into()
    }
    fn spoke(&self, text: &str, _ctx: &Value) {
        self.0.lock().unwrap().spoke.push(text.to_string());
    }
    fn after_turn(&self, cost: &kot::ui::TurnCost) {
        let mut s = self.0.lock().unwrap();
        s.turns += 1;
        s.costs.push((cost.tools, cost.messages));
    }
    fn show(&self, s: String) {
        self.0.lock().unwrap().shown.push(s);
    }
    fn check_before_idle(&self) -> bool {
        self.0.lock().unwrap().check
    }
    fn activity(&self, a: &Activity) {
        self.0.lock().unwrap().activity.push(a.clone());
    }
    fn transcript(&self) -> Option<std::path::PathBuf> {
        self.0.lock().unwrap().transcript.clone()
    }
    fn reminder(&self) -> Option<String> {
        self.0.lock().unwrap().reminder.clone()
    }
    fn history_path(&self) -> Option<std::path::PathBuf> {
        self.0.lock().unwrap().history.clone()
    }
    fn restart_note(&self) -> Option<String> {
        self.0.lock().unwrap().restart.clone()
    }
    fn stall_after(&self) -> Duration {
        self.0.lock().unwrap().stall.unwrap_or(agent_state_machine::STALL_AFTER)
    }
    fn local_nag_after(&self) -> Duration {
        self.0.lock().unwrap().local_nag.unwrap_or(agent_state_machine::LOCAL_TASK_NAG_AFTER)
    }
}

struct Rig {
    fake: Arc<Fake>,
    seen: Arc<Mutex<Seen>>,
    tx: Option<mpsc::UnboundedSender<Inbound>>,
    done: tokio::task::JoinHandle<()>,
}

async fn rig(script: Vec<Reply>) -> Rig {
    rig_with(script, false).await
}

async fn rig_with(script: Vec<Reply>, check: bool) -> Rig {
    rig_seen(script, Seen { check, ..Seen::default() }).await
}

async fn rig_seen(script: Vec<Reply>, seen: Seen) -> Rig {
    let fake = Arc::new(Fake::default());
    *fake.script.lock().unwrap() = script.into();
    let url = serve(fake.clone()).await;
    let seen = Arc::new(Mutex::new(seen));
    let (tx, rx) = mpsc::unbounded_channel();
    let host = Arc::new(TestHost(seen.clone()));
    let done = tokio::spawn(agent_state_machine::run(host, Llm::local(&url, "fake"), "You are tama.".into(), rx));
    Rig { fake, seen, tx: Some(tx), done }
}

impl Rig {
    fn wake(&self, text: &str) {
        self.wake_kind(text, "chat");
    }
    fn wake_kind(&self, text: &str, kind: &'static str) {
        self.tx.as_ref().unwrap().send(Inbound::Wake { text: text.into(), kind, ctx: json!({"t": "said"}) }).unwrap();
    }
    /// Close the inbox and wait for the loop to drain and return — which
    /// it must, or the test times out.
    async fn finish(mut self) -> (Arc<Fake>, Arc<Mutex<Seen>>) {
        self.tx.take();
        tokio::time::timeout(Duration::from_secs(30), self.done).await.expect("loop never finished").unwrap();
        (self.fake, self.seen)
    }
    async fn until(&self, what: &str, f: impl Fn(&Fake, &Seen) -> bool) {
        let t0 = std::time::Instant::now();
        while !f(&self.fake, &self.seen.lock().unwrap()) {
            assert!(t0.elapsed() < Duration::from_secs(30), "timed out waiting for: {what}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

// ── tests ───────────────────────────────────────────────────────────────

/// The bug this module exists for: a query's result reaches the model on
/// the next turn, labelled, and the model can then answer with it.
#[tokio::test]
async fn query_result_is_fed_back_and_answered() {
    let r = rig(vec![calls(vec![("Bash", json!({"command": "echo meow-from-bash"}))]), say("bash said meow")]).await;
    r.wake("run echo for me");
    r.until("reply", |_, s| !s.sent.is_empty()).await;
    let (fake, seen) = r.finish().await;

    assert_eq!(fake.requests().len(), 2, "one turn for the wake, one for the result");
    let fed = fake.fed(1);
    assert!(fed.contains("[#0 Bash]"), "result labelled by id and tool: {fed}");
    assert!(fed.contains("meow-from-bash"), "the actual output: {fed}");
    // The result names the call that asked for it...
    assert!(fed.contains("$ echo meow-from-bash"), "{fed}");
    // ...and the model's own turns never carry a call written as text —
    // a small model copies that pattern instead of calling the tool.
    assert!(!fake.all(1).contains("[called:"), "{}", fake.all(1));
    assert_eq!(seen.lock().unwrap().sent, vec!["bash said meow"]);
}

/// A result isn't a one-turn flash: it enters the conversation history, so
/// a later turn, woken by something else entirely, still has it — in order,
/// between the wake that led to it and the one after.
#[tokio::test]
async fn results_stay_in_history() {
    let r = rig(vec![calls(vec![("Bash", json!({"command": "echo kept-in-history"}))]), say("noted"), say("still know it")]).await;
    r.wake("remember this");
    r.until("first reply", |_, s| s.sent.len() == 1).await;
    r.wake("what did bash say earlier?");
    r.until("second reply", |_, s| s.sent.len() == 2).await;
    let (fake, _) = r.finish().await;

    assert_eq!(fake.requests().len(), 3);
    assert!(!fake.fed(2).contains("kept-in-history"), "turn 3 was fed only the new wake");
    let msgs = fake.requests()[2]["messages"].as_array().unwrap().clone();
    let pos = |needle: &str, role: &str| msgs.iter().position(|m| m["role"] == role && m["content"].as_str().unwrap_or("").contains(needle));
    let asked = pos("remember this", "user").expect("the first wake is in history");
    let result = pos("[#0 Bash] $ echo kept-in-history", "user").expect("the result, with its call, is in history");
    let next = pos("what did bash say", "user").expect("the new wake");
    assert!(asked < result && result < next, "wake, then result, then the new wake: {msgs:#?}");
}

/// A host query (not a shared tool) comes back the same way.
#[tokio::test]
async fn host_query_is_fed_back() {
    let r = rig(vec![calls(vec![("Echo", json!({"v": "purr"}))]), say("ok")]).await;
    r.wake("go");
    r.until("reply", |_, s| !s.sent.is_empty()).await;
    let (fake, _) = r.finish().await;
    assert!(fake.fed(1).contains("[#0 Echo]") && fake.fed(1).contains("echo:purr"), "{}", fake.fed(1));
}

/// A record is fire-and-forget: it happens, it's shown, and it does not
/// buy another turn.
#[tokio::test]
async fn record_is_not_fed_back() {
    let r = rig(vec![say("hello")]).await;
    r.wake("hi");
    r.until("reply", |_, s| !s.sent.is_empty()).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (fake, seen) = r.finish().await;
    assert_eq!(fake.requests().len(), 1);
    assert!(seen.lock().unwrap().shown.iter().any(|l| l.contains("SendMessage") && l.contains("hello")));
}

/// Several queries from one turn come back together, in one follow-up —
/// the batch waits for its slowest member rather than turning per result.
#[tokio::test]
async fn results_of_one_turn_are_aggregated() {
    let r = rig(vec![
        calls(vec![("Echo", json!({"v": "fast", "ms": 10})), ("Echo", json!({"v": "slow", "ms": 400})), ("Bash", json!({"command": "echo third"}))]),
        say("got all three"),
    ])
    .await;
    r.wake("do three things");
    r.until("reply", |_, s| !s.sent.is_empty()).await;
    let (fake, _) = r.finish().await;
    assert_eq!(fake.requests().len(), 2, "one follow-up, not one per result");
    let fed = fake.fed(1);
    for want in ["echo:fast", "echo:slow", "third"] {
        assert!(fed.contains(want), "missing {want}: {fed}");
    }
}

/// A model that only ever calls tools, with nobody speaking to it, gets
/// MAX_FOLLOWUPS follow-ups and then its results are held, not fed.
#[tokio::test]
async fn followups_are_capped() {
    let r = rig(vec![]).await;
    *r.fake.forever.lock().unwrap() = Some(calls(vec![("Bash", json!({"command": "true"}))]));
    r.wake("loop forever");
    r.until("cap", |_, s| s.shown.iter().any(|l| l.contains("held back"))).await;
    let (fake, _) = r.finish().await;
    assert_eq!(fake.requests().len(), 1 + MAX_FOLLOWUPS as usize);
}

/// A new wake resets the follow-up budget — the cap is about nobody
/// speaking, not about a long conversation.
#[tokio::test]
async fn wake_resets_followup_budget() {
    let r = rig(vec![]).await;
    *r.fake.forever.lock().unwrap() = Some(calls(vec![("Bash", json!({"command": "true"}))]));
    r.wake("first");
    r.until("cap", |_, s| s.shown.iter().any(|l| l.contains("held back"))).await;
    r.wake("second");
    r.until("second cap", |_, s| s.shown.iter().filter(|l| l.contains("held back")).count() == 2).await;
    let (fake, _) = r.finish().await;
    assert_eq!(fake.requests().len(), 2 * (1 + MAX_FOLLOWUPS as usize));
}

/// Wakes that queue up are folded into one turn, not one turn each.
#[tokio::test]
async fn queued_wakes_fold_into_one_turn() {
    let r = rig(vec![say("both")]).await;
    // Both in the inbox before the loop gets to them.
    r.wake("first wake");
    r.wake("second wake");
    r.until("reply", |_, s| !s.sent.is_empty()).await;
    let (fake, _) = r.finish().await;
    assert_eq!(fake.requests().len(), 1);
    assert!(fake.fed(0).contains("first wake") && fake.fed(0).contains("second wake"), "{}", fake.fed(0));
}

/// A wake arriving mid-turn is neither lost nor allowed to cancel the turn
/// in flight: it's the next turn.
#[tokio::test]
async fn wake_during_a_turn_waits_for_the_next() {
    let mut slow = say("one");
    slow.delay_ms = 500;
    let r = rig(vec![slow, say("two")]).await;
    r.wake("first");
    r.until("first request", |f, _| f.requests().len() == 1).await;
    r.wake("second");
    r.until("both replies", |_, s| s.sent.len() == 2).await;
    let (fake, seen) = r.finish().await;
    assert_eq!(seen.lock().unwrap().sent, vec!["one", "two"]);
    assert!(fake.fed(1).contains("second"));
    // And the second turn still has the first in its history.
    assert!(fake.all(1).contains("first"));
}

/// A result that lands while a wake is also waiting rides along with it.
#[tokio::test]
async fn result_rides_along_with_a_wake() {
    let mut slow = calls(vec![("Echo", json!({"v": "late", "ms": 50}))]);
    slow.delay_ms = 0;
    let mut hold = say("ack");
    hold.delay_ms = 0;
    let r = rig(vec![slow, hold]).await;
    r.wake("start");
    r.until("first request", |f, _| f.requests().len() == 1).await;
    r.wake("meanwhile");
    r.until("reply", |_, s| !s.sent.is_empty()).await;
    let (fake, _) = r.finish().await;
    let n = fake.requests().len();
    let joined: String = (1..n).map(|i| fake.fed(i)).collect();
    assert!(joined.contains("echo:late") && joined.contains("meanwhile"), "{joined}");
}

/// AboutMe answers with who this is — the thing cats claimed not to have.
#[tokio::test]
async fn about_me_is_offered_and_answered() {
    let r = rig(vec![calls(vec![("AboutMe", json!({}))]), say("I am tama")]).await;
    r.wake("who are you");
    r.until("reply", |_, s| !s.sent.is_empty()).await;
    let (fake, _) = r.finish().await;
    assert!(fake.tool_names(0).contains(&"AboutMe".to_string()), "{:?}", fake.tool_names(0));
    let fed = fake.fed(1);
    assert!(fed.contains("Name: tama") && fed.contains("Model: fake") && fed.contains("Where: a test") && fed.contains("You are tama."), "{fed}");
}

/// Every host gets the shared tools, plus its own for the wake's kind.
#[tokio::test]
async fn tools_are_shared_plus_host_kind() {
    let r = rig(vec![text("x"), text("y")]).await;
    r.wake("chatting");
    r.until("first", |f, _| f.requests().len() == 1).await;
    r.wake_kind("a task", "task");
    r.until("second", |f, _| f.requests().len() == 2).await;
    let (fake, _) = r.finish().await;
    let first = fake.tool_names(0);
    for t in ["AboutMe", "Bash", "ReadFile", "WriteFile", "TokenBudget", "BrowseTools", "Inspect", "Compact", "SendMessage"] {
        assert!(first.contains(&t.to_string()), "missing {t}: {first:?}");
    }
    assert!(!first.contains(&"TaskOnly".to_string()));
    assert!(fake.tool_names(1).contains(&"TaskOnly".to_string()));
}

/// Calling a tool that doesn't exist here is fed back as such, not
/// silently dropped.
#[tokio::test]
async fn unknown_tool_is_fed_back() {
    let r = rig(vec![calls(vec![("Teleport", json!({}))]), say("oh")]).await;
    r.wake("go");
    r.until("reply", |_, s| !s.sent.is_empty()).await;
    let (fake, _) = r.finish().await;
    assert!(fake.fed(1).contains("[#0 Teleport]") && fake.fed(1).contains("no such tool"), "{}", fake.fed(1));
}

/// Plain text with no tool call goes to the host's `spoke`.
#[tokio::test]
async fn plain_text_goes_to_spoke() {
    let r = rig(vec![text("just words")]).await;
    r.wake("hi");
    r.until("spoke", |_, s| !s.spoke.is_empty()).await;
    let (_, seen) = r.finish().await;
    assert_eq!(seen.lock().unwrap().spoke, vec!["just words"]);
}

/// BrowseTools / Inspect see past results by id.
#[tokio::test]
async fn inspect_pulls_back_a_past_result() {
    let r = rig(vec![calls(vec![("Echo", json!({"v": "remember-me"}))]), calls(vec![("Inspect", json!({"id": 0})), ("BrowseTools", json!({}))]), say("found it")]).await;
    r.wake("go");
    r.until("reply", |_, s| !s.sent.is_empty()).await;
    let (fake, _) = r.finish().await;
    let fed = fake.fed(2);
    assert!(fed.contains("#0 Echo") && fed.contains("echo:remember-me"), "{fed}");
    assert!(fed.contains("0: Echo"), "BrowseTools lists it: {fed}");
}

/// Reset forgets the conversation.
#[tokio::test]
async fn reset_forgets_history() {
    let r = rig(vec![text("a"), text("b")]).await;
    r.wake("secret-before-reset");
    r.until("first", |f, _| f.requests().len() == 1).await;
    r.tx.as_ref().unwrap().send(Inbound::Reset("test".into())).unwrap();
    r.wake("after");
    r.until("second", |f, _| f.requests().len() == 2).await;
    let (fake, _) = r.finish().await;
    assert!(!fake.all(1).contains("secret-before-reset"), "{}", fake.all(1));
}

/// Compact replaces history with the model's summary.
#[tokio::test]
async fn compact_replaces_history() {
    let r = rig(vec![calls(vec![("Compact", json!({"summary": "SUMMARY-TEXT"}))]), text("after")]).await;
    r.wake("old-stuff");
    r.until("first", |f, _| f.requests().len() == 1).await;
    r.wake("new");
    r.until("second", |f, _| f.requests().len() == 2).await;
    let (fake, _) = r.finish().await;
    let all = fake.all(1);
    assert!(all.contains("SUMMARY-TEXT") && !all.contains("old-stuff"), "{all}");
}

/// Every turn is reported to the host, and closing the inbox with a query
/// still in flight waits for it rather than dropping it.
#[tokio::test]
async fn close_waits_for_in_flight_work() {
    let r = rig(vec![calls(vec![("Echo", json!({"v": "slowpoke", "ms": 300}))]), say("done")]).await;
    r.wake("go");
    r.until("first", |f, _| f.requests().len() == 1).await;
    let (fake, seen) = r.finish().await;
    assert_eq!(fake.requests().len(), 2, "the in-flight result still got its turn");
    assert_eq!(seen.lock().unwrap().turns, 2);
}

/// A `SendMessage` is talking, not a tool call: a turn that ran `Bash` and
/// replied reports one of each, not two tool calls.
#[tokio::test]
async fn messages_are_counted_apart_from_tools() {
    let r = rig(vec![calls(vec![("Bash", json!({"command": "true"})), ("SendMessage", json!({"body": "done"}))])]).await;
    r.wake("go");
    r.until("reply", |_, s| !s.sent.is_empty()).await;
    let (_, seen) = r.finish().await;
    assert_eq!(seen.lock().unwrap().costs[0], (1, 1));
}

/// kuro's case, live 2026-09-24: a turn still thinking when the session
/// resets must not act — its reply belongs to a conversation that's gone.
#[tokio::test]
async fn reset_during_a_turn_drops_its_calls() {
    let mut slow = say("stale reply");
    slow.delay_ms = 500;
    let r = rig(vec![slow, say("fresh reply")]).await;
    r.wake("before the clear");
    r.until("thinking", |f, _| f.requests().len() == 1).await;
    r.tx.as_ref().unwrap().send(Inbound::Reset("clear".into())).unwrap();
    r.wake("after the clear");
    r.until("fresh reply", |_, s| !s.sent.is_empty()).await;
    let (fake, seen) = r.finish().await;
    assert_eq!(seen.lock().unwrap().sent, vec!["fresh reply"], "the stale turn's SendMessage must not go out");
    assert!(!fake.all(1).contains("before the clear"), "and the new session doesn't remember it: {}", fake.all(1));
}

/// Wakes queued ahead of a reset in the same batch are the old session's.
#[tokio::test]
async fn wakes_queued_before_a_reset_are_dropped() {
    let r = rig(vec![say("only the new one")]).await;
    r.wake("old wake");
    r.tx.as_ref().unwrap().send(Inbound::Reset("clear".into())).unwrap();
    r.wake("new wake");
    r.until("reply", |_, s| !s.sent.is_empty()).await;
    let (fake, _) = r.finish().await;
    assert_eq!(fake.requests().len(), 1);
    let fed = fake.fed(0);
    assert!(fed.contains("new wake") && !fed.contains("old wake"), "{fed}");
}

/// A query still running at the reset finishes, is shown, and is not fed
/// into the new session.
#[tokio::test]
async fn a_query_from_before_a_reset_is_not_fed_back() {
    let r = rig(vec![calls(vec![("Echo", json!({"v": "stale", "ms": 400}))]), say("fresh")]).await;
    r.wake("start something slow");
    r.until("first turn done", |_, s| s.turns == 1).await;
    r.tx.as_ref().unwrap().send(Inbound::Reset("clear".into())).unwrap();
    r.wake("new question");
    r.until("reply", |_, s| !s.sent.is_empty()).await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    let (fake, seen) = r.finish().await;
    let all: String = (0..fake.requests().len()).map(|i| fake.all(i)).collect();
    assert!(!all.contains("echo:stale"), "stale result reached the model: {all}");
    assert!(seen.lock().unwrap().shown.iter().any(|l| l.contains("before the session reset")));
}

// ── stalls, held results, long output (meow, 2026-09-24) ────────────────

/// meow's case: asked to build, it read docs, then messaged "next I'll
/// build" and called nothing — and nothing ever woke it again. Now a turn
/// that worked on results and only wrote things gets one check-in, where
/// it can actually start what it promised.
#[tokio::test]
async fn promise_without_a_call_gets_a_check_in() {
    let r = rig_with(
        vec![
            calls(vec![("Bash", json!({"command": "echo recon"}))]),
            say("next I'll run the build"),
            calls(vec![("Bash", json!({"command": "echo building"}))]),
            say("built"),
            text(""),
        ],
        true,
    )
    .await;
    r.wake("build the kernel");
    r.until("second check-in answered", |_, s| s.shown.iter().any(|l| l.contains("check-in: nothing more to do"))).await;
    let (fake, seen) = r.finish().await;
    assert_eq!(fake.requests().len(), 5);
    assert!(fake.fed(2).contains(CHECK_IN), "the promise turn is followed by a check-in: {}", fake.fed(2));
    assert!(fake.fed(3).contains("$ echo building"), "the promised work ran and came back: {}", fake.fed(3));
    assert!(fake.fed(4).contains(CHECK_IN));
    assert_eq!(seen.lock().unwrap().sent, vec!["next I'll run the build", "built"]);
}

/// A check-in answered with nothing leaves no trace: the next turn's
/// history has only the check-in that led somewhere.
#[tokio::test]
async fn an_empty_check_in_leaves_no_trace() {
    let r = rig_with(vec![calls(vec![("Bash", json!({"command": "echo x"}))]), say("done"), text(""), say("hi again")], true).await;
    r.wake("go");
    r.until("check-in", |_, s| s.shown.iter().any(|l| l.contains("check-in: nothing more to do"))).await;
    r.wake("next");
    r.until("second reply", |_, s| s.sent.len() == 2).await;
    let (fake, _) = r.finish().await;
    assert_eq!(fake.requests().len(), 4);
    assert!(!fake.all(3).contains("(Check-in:"), "{}", fake.all(3));
}

/// The other half of meow's case: the check-in itself gets nothing back
/// (`an_empty_check_in_leaves_no_trace`), but this time there's an open
/// `LocalTask` — so, like a chain task's `nudge`, the cat gets woken again
/// on its own, without an operator's line, instead of sitting idle forever.
#[tokio::test]
async fn an_ignored_check_in_with_open_local_tasks_gets_nudged() {
    let r = rig_seen(
        vec![calls(vec![("Bash", json!({"command": "echo x"}))]), say("done"), text(""), say("continuing")],
        Seen { check: true, local_nag: Some(Duration::from_millis(80)), reminder: Some("(Your open local tasks — LocalTask to update them:\nL2 [doing] read the runbook)".into()), ..Seen::default() },
    )
    .await;
    r.wake("build the kernel");
    r.until("check-in answered with nothing", |_, s| s.shown.iter().any(|l| l.contains("check-in: nothing more to do"))).await;
    r.until("nudged back to life", |_, s| s.sent.len() == 2).await;
    let (fake, seen) = r.finish().await;
    assert_eq!(fake.requests().len(), 4, "wake, result, check-in, nudge — no fifth turn piles on");
    let fed = fake.fed(3);
    assert!(fed.contains("open local tasks") && fed.contains("L2") && fed.contains("read the runbook"), "the nudge carries the open list: {fed}");
    assert!(!fed.contains(CHECK_IN), "a real wake, not another check-in: {fed}");
    assert_eq!(seen.lock().unwrap().sent, vec!["done", "continuing"]);
}

/// A cat with nothing open on its local list is left alone — the nudge only
/// fires when there's actually something to remind it about.
#[tokio::test]
async fn no_open_local_tasks_means_no_nudge() {
    let r = rig_seen(
        vec![calls(vec![("Bash", json!({"command": "echo x"}))]), say("done"), text("")],
        Seen { check: true, local_nag: Some(Duration::from_millis(80)), reminder: None, ..Seen::default() },
    )
    .await;
    r.wake("build the kernel");
    r.until("check-in answered with nothing", |_, s| s.shown.iter().any(|l| l.contains("check-in: nothing more to do"))).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let (fake, _) = r.finish().await;
    assert_eq!(fake.requests().len(), 3, "no fourth, nudged turn — nothing to nudge about");
}

/// Plain chatter — a wake answered with a message, no tools involved —
/// never costs a check-in.
#[tokio::test]
async fn a_reply_to_a_wake_is_not_checked() {
    let r = rig_with(vec![say("hello")], true).await;
    r.wake("hi");
    r.until("reply", |_, s| !s.sent.is_empty()).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (fake, _) = r.finish().await;
    assert_eq!(fake.requests().len(), 1);
}

/// Nor does a turn that started more work — it isn't idle.
#[tokio::test]
async fn a_turn_that_starts_a_query_is_not_checked() {
    let r = rig_with(vec![calls(vec![("Bash", json!({"command": "echo a"}))]), calls(vec![("Bash", json!({"command": "echo b"})), ("SendMessage", json!({"body": "working"}))]), text("")], true).await;
    r.wake("go");
    r.until("three turns", |_, s| s.turns == 3).await;
    let (fake, _) = r.finish().await;
    assert_eq!(fake.requests().len(), 3);
    assert!(!fake.fed(2).contains(CHECK_IN), "results back, not a check-in: {}", fake.fed(2));
}

/// Results past the cap aren't dropped: they ride along with the next wake.
#[tokio::test]
async fn held_results_are_fed_with_the_next_wake() {
    let r = rig(vec![]).await;
    *r.fake.forever.lock().unwrap() = Some(calls(vec![("Bash", json!({"command": "echo held-one"}))]));
    r.wake("loop");
    r.until("cap", |_, s| s.shown.iter().any(|l| l.contains("held back"))).await;
    *r.fake.forever.lock().unwrap() = Some(say("caught up"));
    r.wake("status?");
    r.until("reply", |_, s| !s.sent.is_empty()).await;
    let (fake, _) = r.finish().await;
    let n = 1 + MAX_FOLLOWUPS as usize;
    let fed = fake.fed(n);
    assert!(fed.contains("status?") && fed.contains(&format!("[#{} Bash]", MAX_FOLLOWUPS)) && fed.contains("held-one"), "{fed}");
}

/// The last allowed follow-up says so, and only that one.
#[tokio::test]
async fn last_followup_is_announced() {
    let r = rig(vec![]).await;
    *r.fake.forever.lock().unwrap() = Some(calls(vec![("Bash", json!({"command": "true"}))]));
    r.wake("loop");
    r.until("cap", |_, s| s.shown.iter().any(|l| l.contains("held back"))).await;
    let (fake, _) = r.finish().await;
    let last = MAX_FOLLOWUPS as usize;
    assert!(fake.fed(last).contains("last turn without someone writing"), "{}", fake.fed(last));
    assert!(!fake.fed(last - 1).contains("last turn without someone writing"));
}

/// A long result is fed as head and tail, with a pointer; Inspect with an
/// offset reads the middle a page at a time.
#[tokio::test]
async fn long_results_feed_head_and_tail_and_page() {
    let cmd = "i=0; while [ $i -lt 500 ]; do echo line$i-xxxxxxxxxx; i=$((i+1)); done";
    let r = rig(vec![calls(vec![("Bash", json!({"command": cmd}))]), calls(vec![("Inspect", json!({"id": 0, "offset": 1000}))]), say("read it")]).await;
    r.wake("go");
    r.until("reply", |_, s| !s.sent.is_empty()).await;
    let (fake, _) = r.finish().await;
    let fed = fake.fed(1);
    assert!(fed.contains("line0-") && fed.contains("line499-"), "head and tail: {fed}");
    assert!(!fed.contains("line250-"), "not the middle");
    assert!(fed.contains(r#"Inspect {"id": 0, "offset": 1000}"#), "{fed}");
    let page = fake.fed(2);
    assert!(page.contains("line250-") || page.contains("line60-"), "{page}");
    assert!(page.contains("for the next part"), "{page}");
    assert!(!page.contains("chars cut here"), "a page is never cut again: {page}");
}

/// Bash honours its timeout, and says how to get more.
#[tokio::test]
async fn bash_timeout_is_honoured() {
    let r = rig(vec![calls(vec![("Bash", json!({"command": "sleep 5", "timeout": 1}))]), say("ok")]).await;
    r.wake("go");
    r.until("reply", |_, s| !s.sent.is_empty()).await;
    let (fake, _) = r.finish().await;
    let fed = fake.fed(1);
    assert!(fed.contains("timed out after 1s, killed") && fed.contains("larger timeout"), "{fed}");
}

// ── watching it work: reasoning, activity, transcript ───────────────────

/// Reasoning sent apart (`reasoning_content`) is shown and kept as the
/// activity's last thought — and never goes back to the model: the next
/// request's history has the turn's text, not what it thought.
#[tokio::test]
async fn reasoning_is_shown_and_never_fed_back() {
    let r = rig(vec![
        thinking(calls(vec![("Bash", json!({"command": "echo hi"}))]), "the user wants echo; SECRET-THOUGHT"),
        thinking(say("done"), "it printed hi, so report it"),
    ])
    .await;
    r.wake("echo hi please");
    r.until("reply", |_, s| !s.sent.is_empty()).await;
    let (fake, seen) = r.finish().await;
    let seen = seen.lock().unwrap();

    let shown = seen.shown.join("\n");
    assert!(shown.contains("reasoning") && shown.contains("SECRET-THOUGHT"), "{shown}");
    assert!(!fake.all(1).contains("SECRET-THOUGHT"), "reasoning leaked into history: {}", fake.all(1));
    assert_eq!(seen.activity.last().unwrap().thought, "it printed hi, so report it");
}

/// qwen3 on llama-server thinks inline, `<think>…</think>` in the content.
/// That's cut out of what the model "said" — a reply must not carry it.
#[tokio::test]
async fn inline_think_tags_are_cut_out_of_the_reply() {
    let r = rig(vec![text("<think>pondering the question</think>The answer is 4.")]).await;
    r.wake("2+2?");
    r.until("spoke", |_, s| !s.spoke.is_empty()).await;
    let (_, seen) = r.finish().await;
    let seen = seen.lock().unwrap();
    assert_eq!(seen.spoke, vec!["The answer is 4."]);
    assert!(seen.shown.join("\n").contains("pondering the question"), "shown as reasoning instead");
    assert_eq!(seen.activity.last().unwrap().thought, "pondering the question");
}

/// Text written beside tool calls used to vanish from the log; now it's
/// shown (and it was always in history).
#[tokio::test]
async fn text_beside_calls_is_shown() {
    let r = rig(vec![Reply { text: Some("Checking the build first."), ..calls(vec![("Bash", json!({"command": "true"}))]) }, say("ok")]).await;
    r.wake("build it");
    r.until("reply", |_, s| !s.sent.is_empty()).await;
    let (_, seen) = r.finish().await;
    let shown = seen.lock().unwrap().shown.join("\n");
    assert!(shown.contains("wrote, beside its calls") && shown.contains("Checking the build first."), "{shown}");
}

/// The live record walks the loop: thinking while the model call is out,
/// waiting while its tools run (both in flight at once, each named), then
/// idle — with every call landed, tallied by outcome, failures included.
#[tokio::test]
async fn activity_follows_the_loop_and_tallies_calls() {
    let r = rig(vec![
        calls(vec![("Bash", json!({"command": "sleep 0.3; echo slow"})), ("Bash", json!({"command": "exit 3"})), ("Nope", json!({}))]),
        say("one worked, one failed"),
    ])
    .await;
    r.wake("try both");
    r.until("reply", |_, s| !s.sent.is_empty()).await;
    let (_, seen) = r.finish().await;
    let seen = seen.lock().unwrap();
    let acts = &seen.activity;

    let phases: Vec<&str> = acts.iter().map(|a| a.phase.as_str()).collect();
    let first = |p: &str| phases.iter().position(|x| *x == p).unwrap_or_else(|| panic!("never {p}: {phases:?}"));
    assert!(first("thinking") < first("waiting") && first("waiting") < phases.iter().rposition(|x| *x == "idle").unwrap(), "{phases:?}");

    let both = acts.iter().find(|a| a.running.len() == 2).expect("both Bash calls in flight at once");
    assert!(both.running.iter().any(|f| f.arg == "$ sleep 0.3; echo slow"), "{:?}", both.running);
    assert!(both.running.iter().any(|f| f.arg == "$ exit 3"), "{:?}", both.running);
    assert!(acts.iter().all(|a| a.running.iter().all(|f| f.tool != "Nope")), "an unknown tool is never in flight");

    let last = acts.last().unwrap();
    assert!(last.running.is_empty(), "{:?}", last.running);
    assert_eq!(last.phase, "idle");
    assert_eq!((last.ok, last.failed), (2, 2), "slow Bash + SendMessage ok; exit 3 + the unknown tool failed: {:?}", last.recent);
    let failed = last.recent.iter().find(|f| f.arg == "$ exit 3").unwrap();
    assert!(!failed.ok && failed.meta.contains("exit 3"), "{failed:?}");
    let slow = last.recent.iter().find(|f| f.arg.contains("echo slow")).unwrap();
    assert!(slow.ok && slow.ms >= 250, "timed from dispatch: {slow:?}");
    assert_eq!(last.turns, 2);
    assert!(last.why.contains("result(s) back"), "the last turn was fed results: {}", last.why);
    assert_eq!(last.window, Some(100000));

    let shown = seen.shown.join("\n");
    assert!(shown.contains("started · 2 in flight"), "the second call's start line counts both: {shown}");
}

/// The phase clock restarts only when the phase changes — a call landing
/// mid-wait doesn't reset "waiting for 1m03s".
#[tokio::test]
async fn phase_clock_moves_only_on_a_change() {
    let r = rig(vec![calls(vec![("Echo", json!({"v": "a", "ms": 200})), ("Echo", json!({"v": "b", "ms": 400}))]), say("ok")]).await;
    r.wake("two echoes");
    r.until("reply", |_, s| !s.sent.is_empty()).await;
    let (_, seen) = r.finish().await;
    let acts = seen.lock().unwrap().activity.clone();
    // The first turn's wait: from its settle until the second echo lands.
    // (The second turn's SendMessage is a wait of its own.)
    let waiting: Vec<&Activity> = acts.iter().filter(|a| a.phase == "waiting" && a.turns == 1).collect();
    assert!(waiting.len() >= 2, "a record at the settle and one per landed call: {}", waiting.len());
    assert!(waiting.windows(2).all(|w| w[0].since == w[1].since), "still the same wait: {:?}", waiting.iter().map(|a| a.since).collect::<Vec<_>>());
    assert!(waiting.iter().any(|a| a.running.len() == 1), "one echo landed, one still out");
}

/// The transcript has everything, in order: the system prompt once, each
/// turn's prompt/reasoning/text/calls, each result in full, each record.
#[tokio::test]
async fn transcript_records_the_whole_session() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t/tama.transcript.jsonl");
    let r = rig_seen(
        vec![thinking(calls(vec![("Bash", json!({"command": "echo in-the-transcript"}))]), "run it"), say("all done")],
        Seen { transcript: Some(path.clone()), ..Seen::default() },
    )
    .await;
    r.wake("echo something");
    r.until("reply", |_, s| !s.sent.is_empty()).await;
    r.finish().await;

    let lines: Vec<Value> = std::fs::read_to_string(&path).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    let kinds: Vec<&str> = lines.iter().map(|l| l["t"].as_str().unwrap()).collect();
    assert_eq!(kinds, vec!["start", "turn", "result", "turn", "record"], "{kinds:?}");
    assert!(lines[0]["system"].as_str().unwrap().starts_with("You are tama."));
    assert_eq!(lines[1]["prompt"], "echo something");
    assert_eq!(lines[1]["reasoning"], "run it");
    assert_eq!(lines[1]["calls"][0]["name"], "Bash");
    assert_eq!(lines[1]["total_tokens"], 110);
    assert_eq!(lines[2]["tool"], "Bash");
    assert!(lines[2]["text"].as_str().unwrap().contains("in-the-transcript"));
    assert_eq!(lines[2]["ok"], true);
    assert!(lines[3]["prompt"].as_str().unwrap().contains("[#0 Bash]"));
    assert_eq!(lines[4]["tool"], "SendMessage");
    assert!(lines.iter().all(|l| l["at"].as_u64().is_some() && l.get("session").is_some() || l["t"] == "start"));

    // A second run appends — the file is a log, not a snapshot.
    let r = rig_seen(vec![say("again")], Seen { transcript: Some(path.clone()), ..Seen::default() }).await;
    r.wake("hello again");
    r.until("reply", |_, s| !s.sent.is_empty()).await;
    r.finish().await;
    let n = std::fs::read_to_string(&path).unwrap().lines().count();
    assert_eq!(n, 5 + 3, "start, turn, record appended");
}

/// The host's reminder (a cat's open local tasks) rides every turn with a
/// wake in it — and not a result-only turn, which is still mid-thought.
#[tokio::test]
async fn reminder_rides_wakes_not_result_turns() {
    let r = rig_seen(
        vec![calls(vec![("Bash", json!({"command": "true"}))]), say("ok")],
        Seen { reminder: Some("(Your open local tasks — L1 [doing] build it)".into()), ..Seen::default() },
    )
    .await;
    r.wake("go on");
    r.until("reply", |_, s| !s.sent.is_empty()).await;
    let (fake, _) = r.finish().await;
    assert!(fake.fed(0).contains("L1 [doing] build it"), "{}", fake.fed(0));
    assert!(!fake.fed(1).contains("L1 [doing]"), "{}", fake.fed(1));
}

// ── calls in flight, as the model sees them ─────────────────────────────

/// While a Bash runs, any turn lists it ("still running"); `Running` shows
/// what it has printed so far; `Cancel` kills it, and its result comes back
/// marked cancelled with that output — long before its own 30 s were up.
#[tokio::test]
async fn running_shows_live_output_and_cancel_kills() {
    let r = rig(vec![
        calls(vec![("Bash", json!({"command": "echo started-the-build; sleep 30", "timeout": 60}))]),
        calls(vec![("Running", json!({})), ("Cancel", json!({"id": "r0"}))]),
        say("stopped it"),
    ])
    .await;
    let t0 = std::time::Instant::now();
    r.wake("build it");
    r.until("the build printing", |_, s| s.activity.last().is_some_and(|a| a.phase == "waiting")).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    r.wake("how is it going?");
    r.until("reply", |_, s| !s.sent.is_empty()).await;
    let (fake, seen) = r.finish().await;
    assert!(t0.elapsed() < Duration::from_secs(15), "cancel didn't kill it: {:?}", t0.elapsed());

    let woke = fake.fed(1);
    assert!(woke.contains("Still running") && woke.contains("r0 Bash $ echo started-the-build; sleep 30"), "{woke}");
    let fed = fake.fed(2);
    assert!(fed.contains("Running]") && fed.contains("latest output:\nstarted-the-build"), "Running shows the output so far: {fed}");
    assert!(fed.contains("Cancelling r0"), "{fed}");
    assert!(fed.contains("Bash] $ echo started-the-build; sleep 30  (cancelled"), "the cancelled result itself: {fed}");
    let last = seen.lock().unwrap().activity.last().unwrap().clone();
    assert!(last.running.is_empty());
    assert!(last.recent.iter().any(|f| f.tool == "Bash" && !f.ok && f.meta.contains("cancelled")), "{:?}", last.recent);
}

/// `Running` and `Cancel` with nothing in flight, or a wrong id, say so.
#[tokio::test]
async fn running_and_cancel_with_nothing_to_show() {
    let r = rig(vec![calls(vec![("Running", json!({})), ("Cancel", json!({"id": "r7"}))]), say("ok")]).await;
    r.wake("anything running?");
    r.until("reply", |_, s| !s.sent.is_empty()).await;
    let (fake, _) = r.finish().await;
    let fed = fake.fed(1);
    assert!(fed.contains("Nothing is running."), "{fed}");
    assert!(fed.contains("r7  (not running"), "{fed}");
}

/// A call silent past `stall_after` gets the model one turn to hear about
/// it — once for that silence, not every tick — and the result still lands
/// when it finishes.
#[tokio::test]
async fn a_quiet_call_gets_one_stall_notice() {
    let r = rig_seen(
        vec![calls(vec![("Bash", json!({"command": "sleep 1.5; echo finally", "timeout": 10}))]), text("I'll leave it running."), say("it finished")],
        Seen { stall: Some(Duration::from_millis(300)), ..Seen::default() },
    )
    .await;
    r.wake("run the slow thing");
    r.until("reply", |_, s| !s.sent.is_empty()).await;
    let (fake, seen) = r.finish().await;

    let n = fake.requests().len();
    let notices: usize = (0..n).map(|i| fake.fed(i).matches("still running]").count()).sum();
    assert_eq!(notices, 1, "exactly one notice: {:?}", (0..n).map(|i| fake.fed(i)).collect::<Vec<_>>());
    let notice = (0..n).map(|i| fake.fed(i)).find(|f| f.contains("still running]")).unwrap();
    assert!(notice.contains("[r0 Bash still running] $ sleep 1.5; echo finally") && notice.contains("no output at all"), "{notice}");
    assert!(fake.fed(n - 1).contains("finally"), "the result still comes back: {}", fake.fed(n - 1));
    assert!(seen.lock().unwrap().shown.join("\n").contains("telling the model"));
}

/// New output re-arms the notice: a call that prints, goes quiet, prints,
/// goes quiet again is noticed twice.
#[tokio::test]
async fn output_rearms_the_stall_notice() {
    let r = rig_seen(
        vec![calls(vec![("Bash", json!({"command": "sleep 0.6; echo tick; sleep 0.8; echo tock", "timeout": 10}))]), text("waiting"), text("still waiting"), say("done")],
        Seen { stall: Some(Duration::from_millis(400)), ..Seen::default() },
    )
    .await;
    r.wake("go");
    r.until("reply", |_, s| !s.sent.is_empty()).await;
    let (fake, _) = r.finish().await;
    let n = fake.requests().len();
    let notices: Vec<String> = (0..n).map(|i| fake.fed(i)).filter(|f| f.contains("still running]")).collect();
    assert_eq!(notices.len(), 2, "{notices:?}");
    assert!(notices[0].contains("no output at all"), "{}", notices[0]);
    assert!(notices[1].contains("no new output for"), "{}", notices[1]);
}

/// A Bash that times out still hands back what it printed before it died —
/// the useful part of a build that ran out of time.
#[tokio::test]
async fn a_timed_out_bash_keeps_its_output() {
    let r = rig(vec![calls(vec![("Bash", json!({"command": "echo got-this-far; sleep 5", "timeout": 1}))]), say("ok")]).await;
    r.wake("go");
    r.until("reply", |_, s| !s.sent.is_empty()).await;
    let (fake, _) = r.finish().await;
    let fed = fake.fed(1);
    assert!(fed.contains("got-this-far") && fed.contains("timed out after 1s, killed"), "{fed}");
}

/// A chain write still landing is never shown to the model as "running":
/// it was told writes just happen. Found live: listing one had GLM
/// deliberating whether to resend its summary.
#[tokio::test]
async fn a_write_in_flight_is_not_listed_to_the_model() {
    let r = rig(vec![calls(vec![("SlowSay", json!({"ms": 800})), ("Echo", json!({"v": "a", "ms": 30}))]), calls(vec![("Running", json!({}))]), say("ok")]).await;
    r.wake("go");
    r.until("reply", |_, s| !s.sent.is_empty()).await;
    let (fake, seen) = r.finish().await;
    let fed = fake.fed(1);
    assert!(fed.contains("echo:a") && !fed.contains("SlowSay") && !fed.contains("Still running"), "{fed}");
    assert!(fake.fed(2).contains("Nothing is running."), "{}", fake.fed(2));
    // The operator's live view still had it in flight.
    assert!(seen.lock().unwrap().activity.iter().any(|a| a.running.iter().any(|f| f.tool == "SlowSay")));
}

// ── restarts (meow, 2026-09-25: no memory, rebooted in a loop) ──────────

fn with_history(path: &std::path::Path) -> Seen {
    Seen { history: Some(path.to_path_buf()), restart: Some("RESTART-NOTE: you were restarted".into()), ..Seen::default() }
}

/// What a cat learned before its process restarted is in the first request
/// after, the restart is said once, and only in that first turn.
#[tokio::test]
async fn a_restart_picks_the_conversation_back_up_and_says_so_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tama.history.7.json");

    let r = rig_seen(vec![text("noted: the repo is /src/akuma, reboot next")], with_history(&path)).await;
    r.wake("the repo is at /src/akuma; build, then reboot");
    r.until("first life", |f, _| f.requests().len() == 1).await;
    r.finish().await;
    assert!(path.exists(), "the conversation was saved");

    // The same cat, restarted: a fresh machine, the same path.
    let r = rig_seen(vec![text("the reboot already happened"), text("ok")], with_history(&path)).await;
    r.wake("did the reboot happen?");
    r.until("second life", |f, _| f.requests().len() == 1).await;
    r.wake("anything else?");
    r.until("second turn", |f, _| f.requests().len() == 2).await;
    let (fake, seen) = r.finish().await;
    let first = fake.all(0);
    assert!(first.contains("the repo is at /src/akuma"), "what it was told before: {first}");
    assert!(first.contains("noted: the repo is /src/akuma"), "what it said before: {first}");
    assert!(fake.fed(0).contains("RESTART-NOTE"), "the restart is said in the first turn: {}", fake.fed(0));
    assert!(!fake.fed(1).contains("RESTART-NOTE"), "and only there: {}", fake.fed(1));
    assert!(seen.lock().unwrap().shown.iter().any(|l| l.contains("restored")), "the operator sees it too");
}

/// A reset (the chain's checkpoint moved) forgets the saved conversation as
/// well as the live one: a restart after it starts clean, with no note.
#[tokio::test]
async fn a_reset_clears_the_saved_conversation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tama.history.7.json");
    let r = rig_seen(vec![text("a")], with_history(&path)).await;
    r.wake("secret-before-reset");
    r.until("first", |f, _| f.requests().len() == 1).await;
    r.tx.as_ref().unwrap().send(Inbound::Reset("checkpoint moved".into())).unwrap();
    r.finish().await;

    let r = rig_seen(vec![text("b")], with_history(&path)).await;
    r.wake("fresh");
    r.until("after", |f, _| f.requests().len() == 1).await;
    let (fake, _) = r.finish().await;
    assert!(!fake.all(0).contains("secret-before-reset"), "{}", fake.all(0));
    assert!(!fake.fed(0).contains("RESTART-NOTE"), "nothing was restored, so nothing to say");
}

/// No path, no persistence: `kot chat`, and every host that doesn't opt in.
#[tokio::test]
async fn no_history_path_keeps_nothing() {
    let r = rig(vec![text("a")]).await;
    r.wake("hello");
    r.until("first", |f, _| f.requests().len() == 1).await;
    let (_, seen) = r.finish().await;
    assert!(!seen.lock().unwrap().shown.iter().any(|l| l.contains("restored")));
}

// ── old results age out (meow, 2026-09-25: ~75k tokens re-sent a turn) ──

/// A long result is in the conversation in full for `RESULT_TURNS` turns,
/// then only as a one-line stub that says how to get it back.
#[tokio::test]
async fn an_old_result_shrinks_to_a_stub() {
    use kot::agent_state_machine::RESULT_TURNS;
    let mut script = vec![calls(vec![("Bash", json!({"command": "head -c 900 /dev/zero | tr '\\0' Q"}))])];
    script.extend((0..RESULT_TURNS + 1).map(|_| say("ok")));
    let r = rig(script).await;
    r.wake("make a long line");
    r.until("result fed", |f, _| f.requests().len() == 2).await;
    // Turn 2 was fed it; each wake after that is one more turn.
    for i in 0..RESULT_TURNS {
        r.wake(&format!("wake {i}"));
        let n = 3 + i as usize;
        r.until("turn", move |f, _| f.requests().len() == n).await;
    }
    let (fake, _) = r.finish().await;
    let payload = "Q".repeat(900);
    let last = fake.requests().len() - 1;
    assert!(fake.all(last - 1).contains(&payload), "still in full {} turns after it was fed", RESULT_TURNS - 1);
    let aged = fake.all(last);
    assert!(!aged.contains(&payload), "aged out: {aged}");
    assert!(aged.contains("[#0 Bash] $ head -c 900") && aged.contains("Inspect {\"id\": 0}"), "a stub in its place: {aged}");
}

/// Result ids carry on across a restart, so a restored `[#0 …]` never
/// names a new, different result — and the old one says it's gone.
#[tokio::test]
async fn result_ids_carry_on_across_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tama.history.7.json");
    let r = rig_seen(vec![calls(vec![("Bash", json!({"command": "echo first-life"}))]), text("ok")], with_history(&path)).await;
    r.wake("go");
    r.until("result", |f, _| f.requests().len() == 2).await;
    r.finish().await;

    let r = rig_seen(vec![calls(vec![("Inspect", json!({"id": 0}))]), text("ok")], with_history(&path)).await;
    r.wake("again");
    r.until("result", |f, _| f.requests().len() == 2).await;
    let (fake, _) = r.finish().await;
    let fed = fake.fed(1);
    assert!(fed.contains("[#1 Inspect]"), "the first result of the new life is #1, not #0 again: {fed}");
    assert!(fed.contains("from before a restart"), "Inspect on an id from the last life: {fed}");
}

/// A history file from before aging (a bare array) has its results cut to
/// each row's first line, and new ids start above the highest one in it.
#[test]
fn an_old_format_history_is_trimmed_on_load() {
    use kot::agent_state_machine::load_history;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("meow.history.1.json");
    let body = "x".repeat(5000);
    let old = json!([
        ["user", "root said: build it"],
        ["user", format!("Results of tools you called:\n[#4 Bash] $ make  (exit 0, 2s)\n{body}\n\n[#9 ReadFile] /etc/hosts  (1 KB)\n127.0.0.1 localhost")],
        ["assistant", "built"]
    ]);
    std::fs::write(&path, old.to_string()).unwrap();
    let saved = load_history(&path);
    assert_eq!(saved.history.len(), 3);
    assert_eq!(saved.trimmed, 1);
    assert_eq!(saved.next_id, 10);
    let said = &saved.history[1].1;
    assert!(said.contains("[#4 Bash] $ make  (exit 0, 2s)") && said.contains("[#9 ReadFile] /etc/hosts"), "{said}");
    assert!(!said.contains(&body) && said.len() < 400, "{said}");
    assert_eq!(saved.history[0].1, "root said: build it");
}

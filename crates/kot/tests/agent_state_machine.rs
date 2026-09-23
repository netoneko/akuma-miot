//! `agent_state_machine` against a scripted model: a fake OpenAI-compatible
//! server (the same `/v1/chat/completions` a `llama-server` answers) that
//! replays canned replies and records every request, so each test can see
//! exactly what the model was shown — which is the whole point: tool
//! results must come back, records must not, wakes queued during a turn
//! must be folded into the next one, and nothing may loop forever.

use kot::agent_state_machine::{self, Dispatch, Host, Inbound, MAX_FOLLOWUPS};
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
}

fn calls(c: Vec<(&'static str, Value)>) -> Reply {
    Reply { calls: c, text: None, delay_ms: 0 }
}
fn text(t: &'static str) -> Reply {
    Reply { calls: vec![], text: Some(t), delay_ms: 0 }
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
    fn after_turn(&self, _cost: &kot::ui::TurnCost) {
        self.0.lock().unwrap().turns += 1;
    }
    fn show(&self, s: String) {
        self.0.lock().unwrap().shown.push(s);
    }
}

struct Rig {
    fake: Arc<Fake>,
    seen: Arc<Mutex<Seen>>,
    tx: Option<mpsc::UnboundedSender<Inbound>>,
    done: tokio::task::JoinHandle<()>,
}

async fn rig(script: Vec<Reply>) -> Rig {
    let fake = Arc::new(Fake::default());
    *fake.script.lock().unwrap() = script.into();
    let url = serve(fake.clone()).await;
    let seen = Arc::new(Mutex::new(Seen::default()));
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

//! A disk-only log shaped like Langfuse's ingestion API
//! (<https://langfuse.com/docs/api-and-data-platform/features/ingestion-api>,
//! Langfuse itself is MIT-licensed and open source — nothing here copies
//! its code, only the JSON event shape its `POST /api/public/ingestion`
//! expects), so that whenever we actually decide what to do with it, the
//! bytes on disk don't need reshaping first.
//!
//! Nothing here calls out to a Langfuse server. This just appends one
//! ingestion event per line — a `trace-create` once per session, a
//! `generation-create` per model turn (with real prompt/completion/cached
//! token counts, which is the whole reason this exists: `docs/
//! AGENT_STATE_MACHINE.md`'s cache-hit numbers came from hand-parsing the
//! plain transcript, and a real observability tool would just show them),
//! and a `span-create` per tool call. Each line is already the exact shape
//! one entry of the ingestion API's `batch` array wants; turning a run of
//! them into a real POST later is just wrapping chunks of up to 3.5 MB in
//! `{"batch": [...]}` and sending them — not reshaping the data.
//!
//! One caveat, noted rather than resolved: Langfuse's `usageDetails`
//! buckets are additive (`input + output + cache_read_input_tokens =
//! total`, Anthropic's convention), but the OpenAI-compatible wire format
//! `miot_llm` actually reads reports `cached_tokens` as a *subset* of
//! `prompt_tokens`, not additive on top of it. This log accounts for that
//! (`input` here is `prompt_tokens - cached_tokens`) so the arithmetic
//! lines up, but that mapping is untested against a live Langfuse
//! instance — treat it as "shaped like", not "verified to ingest cleanly".

use serde_json::{json, Value};
use std::path::PathBuf;

/// A transcript past this is moved aside to `<name>.1` (one generation
/// kept) and started again — same bound and reason as
/// `agent_state_machine::Transcript`.
const MAX_BYTES: u64 = 64 * 1024 * 1024;

pub struct LangfuseLog {
    path: PathBuf,
    file: Option<std::fs::File>,
    written: u64,
}

impl LangfuseLog {
    pub fn open(path: PathBuf) -> Self {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let file = std::fs::OpenOptions::new().create(true).append(true).open(&path).ok();
        let written = file.as_ref().and_then(|f| f.metadata().ok()).map(|m| m.len()).unwrap_or(0);
        LangfuseLog { path, file, written }
    }

    pub fn is_open(&self) -> bool {
        self.file.is_some()
    }

    /// Append one ingestion event (`{id, timestamp, type, body}`). Best
    /// effort, same as the plain transcript: a write that fails is
    /// dropped, never allowed to stop the agent loop.
    fn write(&mut self, event_type: &str, body: Value) {
        if self.written > MAX_BYTES {
            let mut old = self.path.clone().into_os_string();
            old.push(".1");
            let _ = std::fs::rename(&self.path, old);
            *self = LangfuseLog::open(self.path.clone());
        }
        let line = json!({
            "id": format!("evt-{}", ulid_like()),
            "timestamp": now_iso(),
            "type": event_type,
            "body": body,
        });
        let mut s = line.to_string();
        s.push('\n');
        if let Some(f) = &mut self.file {
            use std::io::Write as _;
            if f.write_all(s.as_bytes()).is_ok() {
                self.written += s.len() as u64;
            }
        }
    }

    pub fn trace_create(&mut self, trace_id: &str, name: &str, model: &str, window: Option<u32>) {
        self.write(
            "trace-create",
            json!({
                "id": trace_id,
                "name": name,
                "timestamp": now_iso(),
                "metadata": {"model": model, "context_window": window},
            }),
        );
    }

    /// One LLM call. `prompt_tokens`/`cached_tokens`/`out_tokens` are as
    /// `miot_llm::Turn` reports them (`cached_tokens` a subset of
    /// `prompt_tokens`, not additive — see the module note on why
    /// `usageDetails.input` below isn't just `prompt_tokens`).
    #[allow(clippy::too_many_arguments)]
    pub fn generation_create(
        &mut self,
        gen_id: &str,
        trace_id: &str,
        model: &str,
        started_ms_ago: u64,
        input: &str,
        output: &str,
        prompt_tokens: u32,
        cached_tokens: u32,
        out_tokens: u32,
        total_tokens: u32,
    ) {
        let start = now_iso_minus_ms(started_ms_ago);
        let end = now_iso();
        self.write(
            "generation-create",
            json!({
                "id": gen_id,
                "traceId": trace_id,
                "name": "turn",
                "model": model,
                "startTime": start,
                "endTime": end,
                "input": input,
                "output": output,
                "usageDetails": {
                    "input": prompt_tokens.saturating_sub(cached_tokens),
                    "output": out_tokens,
                    "cache_read_input_tokens": cached_tokens,
                    "total": total_tokens,
                },
            }),
        );
    }

    /// One tool call (query or record) as a span under the same trace.
    pub fn span_create(&mut self, span_id: &str, trace_id: &str, name: &str, started_ms_ago: u64, input: &str, output: &str, ok: bool) {
        self.write(
            "span-create",
            json!({
                "id": span_id,
                "traceId": trace_id,
                "name": name,
                "startTime": now_iso_minus_ms(started_ms_ago),
                "endTime": now_iso(),
                "input": input,
                "output": output,
                "level": if ok { "DEFAULT" } else { "ERROR" },
            }),
        );
    }
}

/// Not a real ULID (no external dependency pulled in for this) — just
/// unique enough for a disk log nothing else reads yet: wall-clock
/// milliseconds plus a per-process counter.
fn ulid_like() -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{}-{n}", crate::activity::unix_ms())
}

/// `chrono`'s `clock` feature (`Utc::now()`) is deliberately not enabled in
/// this workspace — `crate::ui` formats timestamps the same way, from an
/// epoch-ms `crate::activity::unix_ms()` rather than asking chrono for the
/// time itself.
fn iso_from_ms(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map(|t| t.to_rfc3339()).unwrap_or_default()
}

fn now_iso() -> String {
    iso_from_ms(crate::activity::unix_ms() as i64)
}

fn now_iso_minus_ms(ms_ago: u64) -> String {
    iso_from_ms(crate::activity::unix_ms() as i64 - ms_ago as i64)
}

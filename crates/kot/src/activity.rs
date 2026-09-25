//! What a cat is doing *right now* — live, never on chain.
//!
//! The chain says what a cat did once it did it (a `said`, a `stats_reported`
//! after the turn). Nothing said what it was doing in between: a GLM turn is
//! a minute of silence, a kernel build is ten. This is that in-between.
//!
//! One record per cat, rewritten by its agent loop at every step
//! ([`crate::agent_state_machine`]): which part of the loop it's in, what
//! woke it, which tool calls are in flight, how the finished ones went, and
//! the tail of its last reasoning. The cat POSTs it to its own node
//! (`POST /activity`); each node carries its own cat's record on the mesh
//! status exchange it already does every poll (`node.rs`), in both
//! directions, so whichever node a client connects to knows every cat it can
//! hear — push-only ones included. `GET /activity` serves them all.
//!
//! **Times, across machines.** Every instant here is the *cat's* clock (unix
//! ms), and `at` is when the record was made, on that same clock — so
//! `at - since` is skew-free. How old the record itself is travels
//! separately ([`Seen::age_ms`]), added up hop by hop from local clocks.
//! A reader never compares a cat's clock with its own.

use serde::{Deserialize, Serialize};

/// Most finished calls kept in [`Activity::recent`].
pub const RECENT: usize = 6;
/// Most of the last reasoning kept in [`Activity::thought`] — its tail,
/// where the model has got to.
pub const THOUGHT_CHARS: usize = 320;
/// Most of one call's argument kept.
pub const ARG_CHARS: usize = 80;
/// Most local tasks carried in one record, and how much of each one's text
/// and note.
pub const TASKS: usize = 24;
pub const TASK_CHARS: usize = 120;

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Activity {
    pub name: String,
    pub model: String,
    /// `thinking` (a model call in flight), `compacting` (the same, for a
    /// summary), `waiting` (the model is done, tools still running), or
    /// `idle`.
    pub phase: String,
    /// When `phase` began.
    pub since: u64,
    /// What the current (or last) turn is about: the wake's first line,
    /// `3 result(s) back`, `check-in`.
    pub why: String,
    /// Model turns this process has taken.
    pub turns: u32,
    /// Result-only turns in a row (`MAX_FOLLOWUPS` caps them).
    pub followups: u32,
    /// Results held back past the follow-up cap, waiting for a wake.
    pub held: u32,
    pub running: Vec<Flight>,
    /// Finished calls this process, by outcome.
    pub ok: u32,
    pub failed: u32,
    /// The last [`RECENT`] finished calls, oldest first.
    pub recent: Vec<Finished>,
    /// The tail of the last turn's reasoning, if the model sent any.
    pub thought: String,
    /// The cat's own local task list (`LocalTask`), in id order — every open
    /// one and the newest finished, at most [`TASKS`] (it rides every status
    /// poll), text cut to [`TASK_CHARS`].
    pub tasks: Vec<TaskLine>,
    /// Finished (`done`/`failed`/`dropped`) and all, over the whole list,
    /// not just what's in `tasks` — the progress figure.
    pub tasks_finished: u32,
    pub tasks_total: u32,
    /// The last turn's context use, and the window if known.
    pub tokens: u32,
    pub window: Option<u32>,
    /// When this record was made.
    pub at: u64,
}

/// One local task, as the operator sees it.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct TaskLine {
    pub id: String,
    /// `todo`, `doing`, `done`, `failed`, `dropped`.
    pub status: String,
    pub text: String,
    /// What came of it, for `done`/`failed`.
    pub note: String,
}

/// A tool call in flight.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Flight {
    /// The model's handle on it: `r3` in `Running`/`Cancel`.
    pub id: u64,
    pub tool: String,
    pub arg: String,
    pub since: u64,
    /// Bytes of output so far (a `Bash` streams it), refreshed every few
    /// seconds.
    pub output: u64,
    /// When it last printed, unix ms — 0 if it never has.
    pub last_output: u64,
}

/// A tool call that came back.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Finished {
    pub tool: String,
    pub arg: String,
    pub ok: bool,
    pub ms: u64,
    /// `exit 1`, `timed out after 30s, killed`, `submitted`.
    pub meta: String,
    pub at: u64,
}

/// An [`Activity`] as a node holds it: the record, and how old it was when
/// this node got it — `GET /activity` adds the time since.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Seen {
    /// The cat's account, hex — who signed it to us, not what it claims.
    pub account: String,
    pub age_ms: u64,
    pub activity: Activity,
}

impl Activity {
    /// How long it's been in `phase`, as of `age_ms` after the record was
    /// made.
    pub fn in_phase_ms(&self, age_ms: u64) -> u64 {
        self.at.saturating_sub(self.since) + age_ms
    }
}

pub fn unix_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// One line, at most `n` chars, `…` if cut.
pub fn short(s: &str, n: usize) -> String {
    let line = s.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    if line.chars().count() > n {
        format!("{}…", line.chars().take(n.saturating_sub(1)).collect::<String>())
    } else {
        line.to_string()
    }
}

/// The last `n` chars of `s`, whitespace folded, `…` in front if cut.
pub fn tail(s: &str, n: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let len = flat.chars().count();
    if len > n {
        format!("…{}", flat.chars().skip(len - n + 1).collect::<String>().trim_start())
    } else {
        flat
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_time_uses_only_the_cats_clock_plus_age() {
        let a = Activity { since: 1_000, at: 4_000, ..Default::default() };
        assert_eq!(a.in_phase_ms(0), 3_000);
        assert_eq!(a.in_phase_ms(500), 3_500);
        // A record whose `since` is somehow after `at` never underflows.
        let b = Activity { since: 5_000, at: 4_000, ..Default::default() };
        assert_eq!(b.in_phase_ms(10), 10);
    }

    #[test]
    fn short_and_tail_cut_with_an_ellipsis() {
        assert_eq!(short("\n  cargo build --release\nmore", 8), "cargo b…");
        assert_eq!(short("ls", 8), "ls");
        assert_eq!(tail("a  b\n c", 10), "a b c");
        assert_eq!(tail("one two three four", 6), "…four");
    }

    #[test]
    fn an_older_reader_ignores_fields_it_doesnt_know() {
        // Forward compatibility is what lets this ride the mesh status
        // exchange across a mixed-version fleet.
        let json = r#"{"name":"meow","model":"m","phase":"idle","since":1,"why":"","turns":0,"followups":0,"held":0,
            "running":[],"ok":0,"failed":0,"recent":[],"thought":"","tokens":0,"window":null,"at":2,"new_field":7}"#;
        let a: Activity = serde_json::from_str(json).unwrap();
        assert_eq!(a.name, "meow");
    }
}

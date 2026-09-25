//! A cat's own to-do list — the `LocalTask` tool
//! ([`miot_llm::local_task_tool`]).
//!
//! Not the litter's tasks: nothing here is on chain, nobody else can see or
//! assign one, and there is no lifecycle beyond what the cat says. It's
//! somewhere for a cat to write down the steps of a job it took on (a DM's
//! "build the kernel") and where it's got to — which, unlike its
//! conversation, survives its process restarting.
//!
//! Kept next to the session file (`~/.akuma/kot/<name>.tasks.json`) and
//! keyed by the same epoch (`last_checkpoint`, `docs/AGENT_SESSION_EPOCH.md`):
//! a list saved under another epoch is not loaded, and a checkpoint move
//! while running empties it — the steps of a job from a session the chain
//! has moved past aren't work any more.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Most open ones — a list that long is a model looping, not planning.
pub const MAX_OPEN: usize = 50;
/// Most finished ones kept, oldest dropped first.
pub const MAX_CLOSED: usize = 20;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct LocalTask {
    pub id: String,
    pub text: String,
    /// `todo`, `doing`, `done`, `failed`, `dropped`.
    pub status: String,
    /// What came of it, for `done`/`failed`.
    #[serde(default)]
    pub note: String,
}

impl LocalTask {
    pub fn is_open(&self) -> bool {
        matches!(self.status.as_str(), "todo" | "doing")
    }
}

#[derive(Serialize, Deserialize, Debug, Default, PartialEq)]
pub struct LocalTasks {
    epoch: u64,
    next: u64,
    pub tasks: Vec<LocalTask>,
    #[serde(skip)]
    path: Option<PathBuf>,
}

impl LocalTasks {
    /// `name`'s list, if it was saved under `epoch`; an empty one otherwise.
    /// `path: None` keeps it in memory only.
    pub fn load(path: Option<PathBuf>, epoch: u64) -> Self {
        let had = path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str::<LocalTasks>(&s).ok())
            .filter(|t| t.epoch == epoch);
        let mut t = had.unwrap_or(LocalTasks { epoch, next: 1, ..Default::default() });
        t.path = path;
        t
    }

    /// The chain moved to `epoch`: a new session, an empty list.
    pub fn reset(&mut self, epoch: u64) {
        if epoch != self.epoch {
            *self = LocalTasks { epoch, next: 1, tasks: Vec::new(), path: self.path.take() };
            self.save();
        }
    }

    fn save(&self) {
        let Some(path) = &self.path else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(s) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, s);
        }
    }

    pub fn open(&self) -> impl Iterator<Item = &LocalTask> {
        self.tasks.iter().filter(|t| t.is_open())
    }

    /// One `LocalTask` call. `Ok` and `Err` both carry the text the model
    /// gets back — the whole list, after a line saying what happened.
    pub fn apply(&mut self, action: &str, id: &str, text: &str) -> Result<String, String> {
        let id = id.trim().to_ascii_uppercase();
        let text = text.trim();
        let what = match action {
            "list" => String::new(),
            "add" => {
                if text.is_empty() {
                    return Err(self.render("add needs text: what to do."));
                }
                if self.open().count() >= MAX_OPEN {
                    return Err(self.render(&format!("{MAX_OPEN} open already — finish or drop some first.")));
                }
                let id = format!("L{}", self.next);
                self.next += 1;
                self.tasks.push(LocalTask { id: id.clone(), text: text.to_string(), status: "todo".into(), note: String::new() });
                format!("added {id}.")
            }
            "start" | "done" | "failed" | "drop" => {
                let Some(t) = self.tasks.iter_mut().find(|t| t.id == id) else {
                    return Err(self.render(&format!("no local task {id:?} — use an id from this list.")));
                };
                t.status = match action {
                    "start" => "doing",
                    "done" => "done",
                    "failed" => "failed",
                    _ => "dropped",
                }
                .into();
                if matches!(action, "done" | "failed") && !text.is_empty() {
                    t.note = text.to_string();
                }
                format!("{id} is {}.", t.status)
            }
            other => return Err(self.render(&format!("unknown action {other:?} — add, start, done, failed, drop or list."))),
        };
        self.trim_closed();
        self.save();
        Ok(self.render(&what))
    }

    fn trim_closed(&mut self) {
        let closed = self.tasks.iter().filter(|t| !t.is_open()).count();
        let mut excess = closed.saturating_sub(MAX_CLOSED);
        self.tasks.retain(|t| {
            if excess > 0 && !t.is_open() {
                excess -= 1;
                return false;
            }
            true
        });
    }

    /// The list as the model reads it: open ones first, then what's done.
    pub fn render(&self, lead: &str) -> String {
        let mut out = Vec::new();
        if !lead.is_empty() {
            out.push(lead.to_string());
        }
        if self.tasks.is_empty() {
            out.push("Your local task list is empty.".into());
            return out.join("\n");
        }
        let row = |t: &LocalTask| {
            let note = if t.note.is_empty() { String::new() } else { format!(" — {}", t.note) };
            format!("{} [{}] {}{note}", t.id, t.status, t.text)
        };
        let open: Vec<String> = self.open().map(row).collect();
        let closed: Vec<String> = self.tasks.iter().filter(|t| !t.is_open()).map(row).collect();
        out.push(if open.is_empty() { "Open: none.".into() } else { format!("Open:\n{}", open.join("\n")) });
        if !closed.is_empty() {
            out.push(format!("Finished:\n{}", closed.join("\n")));
        }
        out.join("\n")
    }

    /// The list for the live record (`Activity::tasks`): every open one and
    /// the newest finished, `max` in all, back in id order — with how many
    /// are finished and how many there are, over the whole list.
    pub fn progress(&self, max: usize) -> (Vec<crate::activity::TaskLine>, u32, u32) {
        use crate::activity::{short, TaskLine, TASK_CHARS};
        let open: Vec<&LocalTask> = self.open().collect();
        let room = max.saturating_sub(open.len());
        let closed: Vec<&LocalTask> = self.tasks.iter().filter(|t| !t.is_open()).collect();
        let mut pick: Vec<&LocalTask> = open.into_iter().take(max).chain(closed[closed.len().saturating_sub(room)..].iter().copied()).collect();
        pick.sort_by_key(|t| t.id.trim_start_matches('L').parse::<u64>().unwrap_or(u64::MAX));
        let lines = pick
            .into_iter()
            .map(|t| TaskLine { id: t.id.clone(), status: t.status.clone(), text: short(&t.text, TASK_CHARS), note: short(&t.note, TASK_CHARS) })
            .collect();
        (lines, closed.len() as u32, self.tasks.len() as u32)
    }

    /// For a wake's prompt: the open ones, one line each — `None` if there
    /// are none, so an idle cat's prompts don't grow.
    pub fn reminder(&self) -> Option<String> {
        let open: Vec<String> = self.open().map(|t| format!("{} [{}] {}", t.id, t.status, t.text)).collect();
        if open.is_empty() {
            return None;
        }
        Some(format!("(Your open local tasks — LocalTask to update them:\n{})", open.join("\n")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_start_done_and_the_list_says_so() {
        let mut t = LocalTasks::load(None, 7);
        assert!(t.apply("add", "", "configure the kernel").unwrap().contains("added L1."));
        t.apply("add", "", "build it").unwrap();
        let out = t.apply("start", "l1", "").unwrap();
        assert!(out.starts_with("L1 is doing."), "{out}");
        let out = t.apply("done", "L1", "used defconfig").unwrap();
        assert!(out.contains("L1 [done] configure the kernel — used defconfig"), "{out}");
        assert!(out.contains("Open:\nL2 [todo] build it"), "{out}");
        assert_eq!(t.reminder().unwrap(), "(Your open local tasks — LocalTask to update them:\nL2 [todo] build it)");
    }

    #[test]
    fn mistakes_come_back_with_the_list() {
        let mut t = LocalTasks::load(None, 0);
        t.apply("add", "", "one").unwrap();
        let e = t.apply("done", "t1.2", "").unwrap_err();
        assert!(e.contains("no local task \"T1.2\"") && e.contains("L1 [todo] one"), "{e}");
        assert!(t.apply("add", "", "  ").is_err());
        assert!(t.apply("frobnicate", "", "").is_err());
    }

    #[test]
    fn it_survives_a_restart_but_not_a_new_epoch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("meow.tasks.json");
        let mut t = LocalTasks::load(Some(path.clone()), 3);
        t.apply("add", "", "rebuild /tmp/meow-greet").unwrap();

        // Same epoch: a restarted process picks it back up, ids continuing.
        let mut again = LocalTasks::load(Some(path.clone()), 3);
        assert_eq!(again.tasks.len(), 1);
        assert!(again.apply("add", "", "send the output").unwrap().contains("added L2."));

        // Saved under another epoch: not loaded.
        assert!(LocalTasks::load(Some(path.clone()), 4).tasks.is_empty());

        // The checkpoint moving while running empties it, on disk too.
        again.reset(4);
        assert!(again.tasks.is_empty());
        assert!(LocalTasks::load(Some(path), 4).tasks.is_empty());
    }

    #[test]
    fn progress_keeps_every_open_one_and_the_newest_finished_in_order() {
        let mut t = LocalTasks::load(None, 0);
        for i in 1..=6 {
            t.apply("add", "", &format!("step {i}")).unwrap();
        }
        for id in ["L1", "L2", "L3", "L5"] {
            t.apply("done", id, "ok").unwrap();
        }
        t.apply("start", "L4", "").unwrap();
        // Room for 4: both open ones (L4, L6), then the two newest finished (L3, L5).
        let (lines, finished, total) = t.progress(4);
        assert_eq!(lines.iter().map(|l| l.id.as_str()).collect::<Vec<_>>(), vec!["L3", "L4", "L5", "L6"]);
        assert_eq!((finished, total), (4, 6));
        assert_eq!(lines[1].status, "doing");
        assert_eq!(lines[0].note, "ok");
    }

    #[test]
    fn finished_ones_are_capped_open_ones_never_trimmed() {
        let mut t = LocalTasks::load(None, 0);
        t.apply("add", "", "keep me").unwrap();
        for i in 0..(MAX_CLOSED + 5) {
            t.apply("add", "", &format!("step {i}")).unwrap();
            t.apply("done", &format!("L{}", i + 2), "").unwrap();
        }
        assert_eq!(t.tasks.iter().filter(|x| !x.is_open()).count(), MAX_CLOSED);
        assert!(t.tasks.iter().any(|x| x.text == "keep me" && x.is_open()));
    }
}

//! `Grep`, `Glob`, `MultiEdit`, `LS` — schemas only, same rule as
//! [`crate::edit_tool`]: dispatch is kot's own
//! (`crates/kot/src/agent_state_machine.rs`, the `local_tool` match arm).
//!
//! A different footing than `Edit`'s, worth being honest about: `Edit`'s
//! shape traces to Anthropic's *officially published* API tool
//! (`str_replace_based_edit_tool`). These four aren't from a published API
//! at all — they're Claude Code's own product tool set, observed and
//! documented by the wider community rather than specified by Anthropic for
//! third-party reuse, and by now widely copied by other coding agents (the
//! same convergence discussed in `docs/TOOLING.md`). Still just names and
//! JSON parameter shapes — functional API surface, not creative
//! expression — and nothing here is Claude Code's source, only kot's own
//! Rust behind the same names. Ported now because meow's own transcript
//! showed 127 of 552 `Bash` calls were plain read-only navigation (`ls`,
//! `find`, `grep`, ...) — the read-side twin of the write-side gap `Edit`
//! closed.
//!
//! Matched as closely as reasonable to the real Claude Code shape — the
//! whole point is that GLM has seen exactly this many times, so the closer
//! the copy, the fewer malformed calls. One deliberate split kept from the
//! original: a tool that names one specific file (`Edit`, `MultiEdit`) uses
//! `file_path`; a tool that names a directory or search scope (`Grep`,
//! `Glob`, `LS`) uses `path`. Not implemented here: `multiline` and `type`
//! on `Grep` (ripgrep-specific matching kot's plain `grep`/`find` backing
//! can't cheaply reproduce) — everything else real Grep takes is.

use genai::chat::Tool;

pub fn grep_tool() -> Tool {
    Tool::new("Grep")
        .with_description(
            "Search file contents for a pattern (regex) under a directory or in one file. \
             output_mode picks the shape: \"content\" (matching lines, only mode -n/-A/-B/-C \
             apply to), \"files_with_matches\" (default — just the paths), \"count\" (match \
             counts per file). No matches is still a successful search, not a failure.",
        )
        .with_schema(serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string"},
                "path": {"type": "string", "description": "file or directory to search — default: this cat's working directory"},
                "glob": {"type": "string", "description": "only search files matching this name pattern, e.g. \"*.rs\""},
                "output_mode": {"type": "string", "enum": ["content", "files_with_matches", "count"], "description": "default: files_with_matches"},
                "-i": {"type": "boolean", "description": "case insensitive"},
                "-n": {"type": "boolean", "description": "show line numbers — content mode only"},
                "-A": {"type": "integer", "description": "lines of context after each match — content mode only"},
                "-B": {"type": "integer", "description": "lines of context before each match — content mode only"},
                "-C": {"type": "integer", "description": "lines of context around each match — content mode only"},
                "head_limit": {"type": "integer", "description": "only the first N lines/entries of output"}
            },
            "required": ["pattern"]
        }))
}

pub fn glob_tool() -> Tool {
    Tool::new("Glob")
        .with_description(
            "Find files by name pattern (e.g. \"*.rs\", \"**/*.md\") under a directory, most \
             recently modified first. No content search — that's Grep.",
        )
        .with_schema(serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string"},
                "path": {"type": "string", "description": "directory to search under — default: this cat's working directory"}
            },
            "required": ["pattern"]
        }))
}

pub fn multi_edit_tool() -> Tool {
    Tool::new("MultiEdit")
        .with_description(
            "Apply several exact find/replace edits to one file, in order, as a single write. \
             Each edit's old_string must match uniquely (or set its own replace_all) against the \
             file as it stands after the edits before it — same rule as Edit, just batched. \
             All-or-nothing: if any edit fails, nothing is written.",
        )
        .with_schema(serde_json::json!({
            "type": "object",
            "properties": {
                "file_path": {"type": "string"},
                "edits": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "old_string": {"type": "string"},
                            "new_string": {"type": "string"},
                            "replace_all": {"type": "boolean"}
                        },
                        "required": ["old_string", "new_string"]
                    }
                }
            },
            "required": ["file_path", "edits"]
        }))
}

pub fn ls_tool() -> Tool {
    Tool::new("LS")
        .with_description("List one directory's immediate contents — names only, directories marked with a trailing /. Not recursive; Glob for that.")
        .with_schema(serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "ignore": {"type": "array", "items": {"type": "string"}, "description": "name patterns to leave out"}
            },
            "required": ["path"]
        }))
}

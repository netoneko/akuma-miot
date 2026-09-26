//! The `Edit` tool's shape — name and JSON schema only; `kot` writes its own
//! dispatch (`crates/kot/src/agent_state_machine.rs`, the `local_tool` match
//! arm next to `ReadFile`/`WriteFile`).
//!
//! Deliberately not kot's own invention. GLM and Qwen's coding-tuned
//! checkpoints are both marketed as drop-in replacements for Claude Code
//! (GLM's "coding plan" is literally "point `ANTHROPIC_BASE_URL` at us and
//! use Claude Code"; Alibaba ships the same pitch as "Qwen Code"), so their
//! agentic-coding SFT data is saturated with this exact shape — far more
//! than any bespoke schema kot could invent would ever see. Two sources,
//! reconciled:
//! - Anthropic's own built-in text editor tool
//!   (<https://platform.claude.com/docs/en/agents-and-tools/tool-use/text-editor-tool>,
//!   tool name `str_replace_based_edit_tool`): its `str_replace` command's
//!   rule that `old_str` must match the file exactly once is kept here
//!   verbatim — it's the behavior GLM/Qwen have specifically been trained to
//!   retry against (re-read, narrow the match) when it fails.
//! - Claude Code's own `Edit` tool: one tool per verb (matching kot's
//!   existing `ReadFile`/`WriteFile` convention, rather than Anthropic's
//!   single multi-command tool with a `command` field) and the
//!   `old_string`/`new_string`/`replace_all` field names, `replace_all`
//!   being Claude Code's extension over the bare Anthropic tool for a
//!   rename-across-the-file edit.
//!
//! Not a derivative-work concern: a tool's name and JSON parameter names are
//! functional API surface — closer to a method signature than to creative
//! expression — and Anthropic publishes the text editor tool's shape
//! specifically so third parties can implement compatible tools against it.
//! Nothing here copies Anthropic/Claude Code source; the dispatch in
//! `agent_state_machine.rs` is kot's own. This file exists separately, with
//! this note on top, purely so the provenance of the *shape* — not the code
//! — stays visible next to it.

use genai::chat::Tool;

pub fn edit_tool() -> Tool {
    Tool::new("Edit")
        .with_description(
            "Replace text in a file on this cat's own host. old_string must appear in the file \
             exactly once — whitespace and all — unless replace_all is true; ReadFile first to \
             get it exact. Fails, saying why, if old_string isn't found or (without replace_all) \
             matches more than once: nothing is written either way. Narrow old_string with more \
             surrounding context and try again, or use WriteFile for a full rewrite.",
        )
        .with_schema(serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "old_string": {"type": "string"},
                "new_string": {"type": "string"},
                "replace_all": {"type": "boolean", "description": "replace every occurrence instead of requiring exactly one — default false"}
            },
            "required": ["path", "old_string", "new_string"]
        }))
}

use serde_json::Value;

/// Turns raw `.jsonl` transcript lines (as produced by `TranscriptWatcher`)
/// into a short, human-readable summary of what the session is currently
/// doing, based on the format researched in #15: an `assistant` tool_use
/// with no matching `user` tool_result yet means that tool is still
/// running; otherwise the latest assistant text is shown.
pub fn summarize(lines: &[String]) -> String {
    let state = parse_state(lines);
    match state.pending {
        Some(tool_use) => describe_tool_use(&tool_use.name, &tool_use.input),
        None => match state.last_text {
            Some(text) => truncate(text.trim(), 60),
            None => "Idle".to_string(),
        },
    }
}

/// True when the session's most recent tool call is an interactive
/// question (`AskUserQuestion`) still waiting on its result - i.e. the
/// session is blocked waiting for the user to answer, distinct from
/// waiting on a tool-permission prompt (#19).
#[allow(dead_code)] // not wired into the app yet; that's #21's job
pub fn is_waiting_for_answer(lines: &[String]) -> bool {
    parse_state(lines)
        .pending
        .is_some_and(|tool_use| tool_use.name == "AskUserQuestion")
}

struct TranscriptState {
    pending: Option<PendingToolUse>,
    last_text: Option<String>,
}

struct PendingToolUse {
    name: String,
    input: Value,
}

fn parse_state(lines: &[String]) -> TranscriptState {
    let mut pending: Vec<(String, PendingToolUse)> = Vec::new();
    let mut last_text: Option<String> = None;

    for line in lines {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(content) = entry.pointer("/message/content").and_then(Value::as_array) else {
            continue;
        };

        match entry.get("type").and_then(Value::as_str) {
            Some("assistant") => {
                for block in content {
                    match block.get("type").and_then(Value::as_str) {
                        Some("tool_use") => {
                            let id = block.get("id").and_then(Value::as_str).unwrap_or("");
                            let name = block.get("name").and_then(Value::as_str).unwrap_or("");
                            pending.push((
                                id.to_string(),
                                PendingToolUse {
                                    name: name.to_string(),
                                    input: block.get("input").cloned().unwrap_or(Value::Null),
                                },
                            ));
                        }
                        Some("text") => {
                            if let Some(text) = block.get("text").and_then(Value::as_str) {
                                last_text = Some(text.to_string());
                            }
                        }
                        _ => {}
                    }
                }
            }
            Some("user") => {
                for block in content {
                    if block.get("type").and_then(Value::as_str) == Some("tool_result")
                        && let Some(id) = block.get("tool_use_id").and_then(Value::as_str)
                    {
                        pending.retain(|(pending_id, _)| pending_id != id);
                    }
                }
            }
            _ => {}
        }
    }

    TranscriptState {
        pending: pending.pop().map(|(_, tool_use)| tool_use),
        last_text,
    }
}

fn describe_tool_use(name: &str, input: &Value) -> String {
    let field = |key: &str| input.get(key).and_then(Value::as_str);

    match name {
        "Bash" => field("description")
            .or_else(|| field("command"))
            .map(|s| format!("Running: {}", truncate(s, 50)))
            .unwrap_or_else(|| "Running a command".to_string()),
        "Read" => field("file_path")
            .map(|p| format!("Reading {p}"))
            .unwrap_or_else(|| "Reading a file".to_string()),
        "Edit" => field("file_path")
            .map(|p| format!("Editing {p}"))
            .unwrap_or_else(|| "Editing a file".to_string()),
        "Write" => field("file_path")
            .map(|p| format!("Writing {p}"))
            .unwrap_or_else(|| "Writing a file".to_string()),
        "Grep" => field("pattern")
            .map(|p| format!("Searching for {}", truncate(p, 40)))
            .unwrap_or_else(|| "Searching".to_string()),
        "AskUserQuestion" => "Waiting for your answer".to_string(),
        "Agent" => field("description")
            .map(|s| truncate(s, 60))
            .unwrap_or_else(|| "Running a subagent".to_string()),
        other => format!("Running: {other}"),
    }
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max_chars).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assistant_tool_use(id: &str, name: &str, input: Value) -> String {
        serde_json::json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [{"type": "tool_use", "id": id, "name": name, "input": input}],
            },
        })
        .to_string()
    }

    fn assistant_text(text: &str) -> String {
        serde_json::json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [{"type": "text", "text": text}],
            },
        })
        .to_string()
    }

    fn user_tool_result(tool_use_id: &str) -> String {
        serde_json::json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [{"type": "tool_result", "tool_use_id": tool_use_id, "content": []}],
            },
        })
        .to_string()
    }

    #[test]
    fn pending_tool_use_shows_as_running() {
        let lines = vec![assistant_tool_use(
            "toolu_1",
            "Bash",
            serde_json::json!({"command": "cargo test", "description": "Run the test suite"}),
        )];
        assert_eq!(summarize(&lines), "Running: Run the test suite");
    }

    #[test]
    fn bash_without_description_falls_back_to_command() {
        let lines = vec![assistant_tool_use(
            "toolu_1",
            "Bash",
            serde_json::json!({"command": "cargo build"}),
        )];
        assert_eq!(summarize(&lines), "Running: cargo build");
    }

    #[test]
    fn resolved_tool_use_falls_back_to_last_text() {
        let lines = vec![
            assistant_text("Let's get started."),
            assistant_tool_use(
                "toolu_1",
                "Read",
                serde_json::json!({"file_path": "src/main.rs"}),
            ),
            user_tool_result("toolu_1"),
            assistant_text("All done here."),
        ];
        assert_eq!(summarize(&lines), "All done here.");
    }

    #[test]
    fn no_entries_at_all_is_idle() {
        assert_eq!(summarize(&[]), "Idle");
    }

    #[test]
    fn read_and_edit_show_file_paths() {
        let lines = vec![assistant_tool_use(
            "toolu_1",
            "Edit",
            serde_json::json!({"file_path": "src/config.rs", "old_string": "a", "new_string": "b"}),
        )];
        assert_eq!(summarize(&lines), "Editing src/config.rs");
    }

    #[test]
    fn malformed_lines_are_skipped_without_panicking() {
        let lines = vec![
            "not json at all".to_string(),
            assistant_tool_use("toolu_1", "Bash", serde_json::json!({"command": "ls"})),
        ];
        assert_eq!(summarize(&lines), "Running: ls");
    }

    #[test]
    fn pending_ask_user_question_is_waiting_for_answer() {
        let lines = vec![assistant_tool_use(
            "toolu_1",
            "AskUserQuestion",
            serde_json::json!({"questions": []}),
        )];
        assert!(is_waiting_for_answer(&lines));
    }

    #[test]
    fn resolved_ask_user_question_is_not_waiting() {
        let lines = vec![
            assistant_tool_use(
                "toolu_1",
                "AskUserQuestion",
                serde_json::json!({"questions": []}),
            ),
            user_tool_result("toolu_1"),
        ];
        assert!(!is_waiting_for_answer(&lines));
    }

    #[test]
    fn pending_non_question_tool_is_not_waiting_for_answer() {
        let lines = vec![assistant_tool_use(
            "toolu_1",
            "Bash",
            serde_json::json!({"command": "cargo test"}),
        )];
        assert!(!is_waiting_for_answer(&lines));
    }

    #[test]
    fn no_entries_is_not_waiting_for_answer() {
        assert!(!is_waiting_for_answer(&[]));
    }
}

use serde_json::Value;

/// A running summary of a session's current activity, built by feeding it
/// transcript lines as they arrive (via `update`) rather than reprocessing
/// the whole accumulated transcript on every call - doing that on every
/// redraw (as this used to) meant navigation lag that scaled with how long
/// a session had been running, since real transcripts can accumulate
/// thousands of lines with large embedded tool outputs.
///
/// Based on the format researched in #15: an `assistant` tool_use with no
/// matching `user` tool_result yet means that tool is still running;
/// otherwise the latest assistant text is shown.
#[derive(Debug, Default)]
pub struct ActivityState {
    pending: Vec<(String, PendingToolUse)>,
    last_text: Option<String>,
}

#[derive(Debug)]
struct PendingToolUse {
    name: String,
    input: Value,
}

impl ActivityState {
    /// Incorporates newly-appended transcript lines into the running state.
    /// Lines already seen must not be passed again.
    pub fn update(&mut self, new_lines: &[String]) {
        for line in new_lines {
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
                                self.pending.push((
                                    id.to_string(),
                                    PendingToolUse {
                                        name: name.to_string(),
                                        input: block.get("input").cloned().unwrap_or(Value::Null),
                                    },
                                ));
                            }
                            Some("text") => {
                                if let Some(text) = block.get("text").and_then(Value::as_str) {
                                    self.last_text = Some(text.to_string());
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
                            self.pending.retain(|(pending_id, _)| pending_id != id);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// A short, human-readable summary of what the session is currently
    /// doing.
    pub fn summary(&self) -> String {
        match self.pending.last() {
            Some((_, tool_use)) => describe_tool_use(&tool_use.name, &tool_use.input),
            None => match &self.last_text {
                Some(text) => truncate(text.trim(), 60),
                None => "Idle".to_string(),
            },
        }
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

    fn summarize(lines: &[String]) -> String {
        let mut state = ActivityState::default();
        state.update(lines);
        state.summary()
    }

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
    fn update_called_across_separate_batches_matches_a_single_batch() {
        // The whole point of ActivityState is to be fed new lines
        // incrementally as they arrive, rather than reprocessing history -
        // a tool_result arriving in a later batch must still resolve a
        // tool_use pushed in an earlier one.
        let mut state = ActivityState::default();
        state.update(&[assistant_tool_use(
            "toolu_1",
            "Bash",
            serde_json::json!({"command": "cargo test"}),
        )]);
        assert_eq!(state.summary(), "Running: cargo test");

        state.update(&[user_tool_result("toolu_1"), assistant_text("Tests passed.")]);
        assert_eq!(state.summary(), "Tests passed.");
    }
}

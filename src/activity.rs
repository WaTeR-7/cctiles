use std::collections::VecDeque;

use serde_json::Value;

/// A live feed of a session's recent activity, built by feeding it
/// transcript lines as they arrive (via `update`) rather than reprocessing
/// the whole accumulated transcript on every call - doing that on every
/// redraw (as this used to) meant navigation lag that scaled with how long
/// a session had been running, since real transcripts can accumulate
/// thousands of lines with large embedded tool outputs.
///
/// Each tool call and each assistant text block becomes its own entry, in
/// the order they occurred. There's no need to reconcile a tool call with
/// its later result (or keep unbounded history) - the tile just wants a
/// live-scrolling feed of what's happening, not a precise record, so this
/// caps how many entries it retains and lets the oldest fall off.
#[derive(Debug, Default)]
pub struct ActivityState {
    lines: VecDeque<String>,
}

/// Comfortably more than any tile will ever be tall enough to show at
/// once, while still bounding memory for a long-running session.
const MAX_LINES: usize = 200;

impl ActivityState {
    /// Incorporates newly-appended transcript lines into the running feed.
    /// Lines already seen must not be passed again.
    pub fn update(&mut self, new_lines: &[String]) {
        for line in new_lines {
            let Ok(entry) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if entry.get("type").and_then(Value::as_str) != Some("assistant") {
                continue;
            }
            let Some(content) = entry.pointer("/message/content").and_then(Value::as_array) else {
                continue;
            };

            for block in content {
                match block.get("type").and_then(Value::as_str) {
                    Some("tool_use") => {
                        let name = block.get("name").and_then(Value::as_str).unwrap_or("");
                        let input = block.get("input").cloned().unwrap_or(Value::Null);
                        self.push(describe_tool_use(name, &input));
                    }
                    Some("text") => {
                        if let Some(text) = block.get("text").and_then(Value::as_str) {
                            // Unlike the tool-use summaries above, this is
                            // content the user actually wants to read in
                            // full where space allows - a fixed char cap
                            // would cut it regardless of how wide the tile
                            // is. Since entries are never wrapped (see
                            // #63/#65), an untruncated line still only ever
                            // costs one row; the renderer clips it to
                            // whatever fits the tile's actual width.
                            self.push(text.trim().to_string());
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    /// Pushes a new feed entry, collapsing any embedded newlines first -
    /// e.g. a `Bash` call with no `description` falls back to showing its
    /// (possibly multi-line) `command`, which would otherwise render as a
    /// stray line break in the middle of what's meant to be one feed entry.
    fn push(&mut self, line: String) {
        self.lines.push_back(line.replace(['\n', '\r'], " "));
        while self.lines.len() > MAX_LINES {
            self.lines.pop_front();
        }
    }

    /// Recent activity, oldest first. The caller decides how many (from the
    /// end) fit the space it has available.
    pub fn recent_lines(&self) -> Vec<String> {
        if self.lines.is_empty() {
            vec!["Idle".to_string()]
        } else {
            self.lines.iter().cloned().collect()
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

    fn lines_of(lines: &[String]) -> Vec<String> {
        let mut state = ActivityState::default();
        state.update(lines);
        state.recent_lines()
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
    fn tool_use_shows_as_running() {
        let lines = vec![assistant_tool_use(
            "toolu_1",
            "Bash",
            serde_json::json!({"command": "cargo test", "description": "Run the test suite"}),
        )];
        assert_eq!(lines_of(&lines), vec!["Running: Run the test suite"]);
    }

    #[test]
    fn bash_without_description_falls_back_to_command() {
        let lines = vec![assistant_tool_use(
            "toolu_1",
            "Bash",
            serde_json::json!({"command": "cargo build"}),
        )];
        assert_eq!(lines_of(&lines), vec!["Running: cargo build"]);
    }

    #[test]
    fn assistant_text_is_not_truncated() {
        let long_text = "a".repeat(200);
        let lines = vec![assistant_text(&long_text)];
        assert_eq!(lines_of(&lines), vec![long_text]);
    }

    #[test]
    fn each_event_becomes_its_own_line_in_order() {
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
        assert_eq!(
            lines_of(&lines),
            vec![
                "Let's get started.".to_string(),
                "Reading src/main.rs".to_string(),
                "All done here.".to_string(),
            ]
        );
    }

    #[test]
    fn no_entries_at_all_is_idle() {
        assert_eq!(lines_of(&[]), vec!["Idle".to_string()]);
    }

    #[test]
    fn read_and_edit_show_file_paths() {
        let lines = vec![assistant_tool_use(
            "toolu_1",
            "Edit",
            serde_json::json!({"file_path": "src/config.rs", "old_string": "a", "new_string": "b"}),
        )];
        assert_eq!(lines_of(&lines), vec!["Editing src/config.rs"]);
    }

    #[test]
    fn malformed_lines_are_skipped_without_panicking() {
        let lines = vec![
            "not json at all".to_string(),
            assistant_tool_use("toolu_1", "Bash", serde_json::json!({"command": "ls"})),
        ];
        assert_eq!(lines_of(&lines), vec!["Running: ls"]);
    }

    #[test]
    fn embedded_newlines_in_a_multiline_bash_command_are_collapsed() {
        let lines = vec![assistant_tool_use(
            "toolu_1",
            "Bash",
            serde_json::json!({"command": "mkdir -p /tmp/foo\ncat > /tmp/foo/bar << 'EOF'"}),
        )];
        let result = lines_of(&lines);
        assert_eq!(result.len(), 1);
        assert!(!result[0].contains('\n'));
        assert_eq!(
            result[0],
            "Running: mkdir -p /tmp/foo cat > /tmp/foo/bar << 'EOF'"
        );
    }

    #[test]
    fn update_called_across_separate_batches_accumulates() {
        let mut state = ActivityState::default();
        state.update(&[assistant_tool_use(
            "toolu_1",
            "Bash",
            serde_json::json!({"command": "cargo test"}),
        )]);
        assert_eq!(state.recent_lines(), vec!["Running: cargo test"]);

        state.update(&[user_tool_result("toolu_1"), assistant_text("Tests passed.")]);
        assert_eq!(
            state.recent_lines(),
            vec![
                "Running: cargo test".to_string(),
                "Tests passed.".to_string()
            ]
        );
    }

    #[test]
    fn old_entries_fall_off_once_the_cap_is_exceeded() {
        let mut state = ActivityState::default();
        for i in 0..MAX_LINES + 10 {
            state.update(&[assistant_text(&format!("message {i}"))]);
        }
        let lines = state.recent_lines();
        assert_eq!(lines.len(), MAX_LINES);
        assert_eq!(lines.first(), Some(&"message 10".to_string()));
        assert_eq!(lines.last(), Some(&format!("message {}", MAX_LINES + 9)));
    }
}

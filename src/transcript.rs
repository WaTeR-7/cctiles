use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::activity::ActivityState;

/// Watches a Claude Code session's `.jsonl` transcript, located by
/// convention at `~/.claude/projects/<cwd with '/' replaced by '-'>/
/// <session-id>.jsonl`, and maintains a cached `ActivityState` from it. The
/// session id isn't known in advance, so this waits for a `.jsonl` file to
/// appear in that directory and picks the most recently modified one.
///
/// The cache is updated incrementally on a background thread as new lines
/// are drained, so reading a session's current activity (called on every
/// redraw) is a cheap lock+read instead of re-parsing the whole
/// accumulated transcript each time - the latter used to scale badly with
/// how long a session had been running (see #54).
pub struct TranscriptWatcher {
    activity: Arc<Mutex<ActivityState>>,
}

impl TranscriptWatcher {
    pub fn start(cwd: &str) -> Self {
        Self::start_in(claude_projects_dir(), cwd)
    }

    fn start_in(projects_root: PathBuf, cwd: &str) -> Self {
        let activity = Arc::new(Mutex::new(ActivityState::default()));
        let activity_for_thread = Arc::clone(&activity);
        let project_dir = projects_root.join(sanitize_cwd(cwd));
        std::thread::spawn(move || watch_loop(project_dir, activity_for_thread));
        Self { activity }
    }

    pub fn activity_summary(&self) -> String {
        self.activity
            .lock()
            .map(|state| state.summary())
            .unwrap_or_else(|_| "Idle".to_string())
    }
}

fn claude_projects_dir() -> PathBuf {
    directories::BaseDirs::new()
        .map(|dirs| dirs.home_dir().join(".claude").join("projects"))
        .unwrap_or_default()
}

fn sanitize_cwd(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c == '/' { '-' } else { c })
        .collect()
}

fn watch_loop(project_dir: PathBuf, activity: Arc<Mutex<ActivityState>>) {
    let Some(file_path) = wait_for_jsonl_file(&project_dir) else {
        return;
    };
    let Ok(mut file) = File::open(&file_path) else {
        return;
    };

    let mut carry = Vec::new();
    drain_new_lines(&mut file, &mut carry, &activity);

    let (tx, rx) = std::sync::mpsc::channel();
    let Ok(mut watcher) = RecommendedWatcher::new(
        move |res: notify::Result<Event>| {
            if let Ok(event) = res {
                let _ = tx.send(event);
            }
        },
        notify::Config::default(),
    ) else {
        return;
    };
    if watcher
        .watch(&file_path, RecursiveMode::NonRecursive)
        .is_err()
    {
        return;
    }

    // Drive draining off both notify events and a periodic fallback tick,
    // since inotify-backed watching has been observed to occasionally miss
    // or delay events in some (e.g. containerized CI) environments. The
    // fallback tick means new content still gets picked up within roughly
    // POLL_INTERVAL even if the event-driven path fails silently.
    const POLL_INTERVAL: Duration = Duration::from_millis(300);
    loop {
        match rx.recv_timeout(POLL_INTERVAL) {
            Ok(event) if matches!(event.kind, EventKind::Modify(_)) => {
                drain_new_lines(&mut file, &mut carry, &activity);
            }
            Ok(_) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                drain_new_lines(&mut file, &mut carry, &activity);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

/// Waits (polling) for a `.jsonl` file to show up in `project_dir` and
/// returns the most recently modified one. Runs on a dedicated background
/// thread, so blocking here is fine.
fn wait_for_jsonl_file(project_dir: &Path) -> Option<PathBuf> {
    loop {
        if let Ok(entries) = std::fs::read_dir(project_dir) {
            let mut candidates: Vec<PathBuf> = entries
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.path())
                .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
                .collect();
            candidates.sort_by_key(|path| {
                std::fs::metadata(path)
                    .and_then(|meta| meta.modified())
                    .ok()
            });
            if let Some(newest) = candidates.pop() {
                return Some(newest);
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Reads whatever is newly available on `file` (its cursor naturally picks
/// up where the previous read left off) and feeds each complete line into
/// `activity`, carrying over any trailing partial line for next time.
fn drain_new_lines(file: &mut File, carry: &mut Vec<u8>, activity: &Arc<Mutex<ActivityState>>) {
    let mut buf = Vec::new();
    if file.read_to_end(&mut buf).is_err() || buf.is_empty() {
        return;
    }
    carry.extend_from_slice(&buf);

    let mut new_lines = Vec::new();
    while let Some(newline_pos) = carry.iter().position(|&b| b == b'\n') {
        let line_bytes: Vec<u8> = carry.drain(..=newline_pos).collect();
        let line = String::from_utf8_lossy(&line_bytes[..line_bytes.len() - 1]).into_owned();
        if !line.is_empty() {
            new_lines.push(line);
        }
    }
    if new_lines.is_empty() {
        return;
    }

    if let Ok(mut state) = activity.lock() {
        state.update(&new_lines);
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::time::Instant;

    use super::*;

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

    #[test]
    fn streams_lines_appended_after_the_watcher_starts() {
        let projects_root =
            std::env::temp_dir().join(format!("cctiles-transcript-test-{}", std::process::id()));
        let cwd = "/fake/project/dir";
        let project_dir = projects_root.join(sanitize_cwd(cwd));
        std::fs::create_dir_all(&project_dir).expect("failed to create fake project dir");

        let jsonl_path = project_dir.join("session-abc.jsonl");
        std::fs::write(&jsonl_path, assistant_text("initial message") + "\n")
            .expect("failed to write initial line");

        let watcher = TranscriptWatcher::start_in(projects_root.clone(), cwd);

        // Give the watcher a moment to discover the file and pick up the
        // line that was already there before it started watching.
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && watcher.activity_summary() == "Idle" {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(watcher.activity_summary(), "initial message");

        // Now append a second line and confirm it streams in too.
        {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&jsonl_path)
                .expect("failed to open for append");
            writeln!(file, "{}", assistant_text("appended message"))
                .expect("failed to append line");
        }

        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && watcher.activity_summary() != "appended message" {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(watcher.activity_summary(), "appended message");

        let _ = std::fs::remove_dir_all(&projects_root);
    }
}

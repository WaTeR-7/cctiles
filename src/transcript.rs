use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

/// Streams newly appended lines from a Claude Code session's `.jsonl`
/// transcript, located by convention at
/// `~/.claude/projects/<cwd with '/' replaced by '-'>/<session-id>.jsonl`.
/// The session id isn't known in advance, so this waits for a `.jsonl`
/// file to appear in that directory and picks the most recently modified
/// one.
pub struct TranscriptWatcher {
    lines: Arc<Mutex<Vec<String>>>,
}

impl TranscriptWatcher {
    pub fn start(cwd: &str) -> Self {
        Self::start_in(claude_projects_dir(), cwd)
    }

    fn start_in(projects_root: PathBuf, cwd: &str) -> Self {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let lines_for_thread = Arc::clone(&lines);
        let project_dir = projects_root.join(sanitize_cwd(cwd));
        std::thread::spawn(move || watch_loop(project_dir, lines_for_thread));
        Self { lines }
    }

    #[allow(dead_code)]
    pub fn lines(&self) -> Vec<String> {
        self.lines.lock().map(|l| l.clone()).unwrap_or_default()
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

fn watch_loop(project_dir: PathBuf, lines: Arc<Mutex<Vec<String>>>) {
    let Some(file_path) = wait_for_jsonl_file(&project_dir) else {
        return;
    };
    let Ok(mut file) = File::open(&file_path) else {
        return;
    };

    let mut carry = Vec::new();
    drain_new_lines(&mut file, &mut carry, &lines);

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

    for event in rx {
        if matches!(event.kind, EventKind::Modify(_)) {
            drain_new_lines(&mut file, &mut carry, &lines);
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
/// up where the previous read left off), and pushes each complete line to
/// `lines`, carrying over any trailing partial line for next time.
fn drain_new_lines(file: &mut File, carry: &mut Vec<u8>, lines: &Arc<Mutex<Vec<String>>>) {
    let mut buf = Vec::new();
    if file.read_to_end(&mut buf).is_err() || buf.is_empty() {
        return;
    }
    carry.extend_from_slice(&buf);

    while let Some(newline_pos) = carry.iter().position(|&b| b == b'\n') {
        let line_bytes: Vec<u8> = carry.drain(..=newline_pos).collect();
        let line = String::from_utf8_lossy(&line_bytes[..line_bytes.len() - 1]).into_owned();
        if !line.is_empty()
            && let Ok(mut guard) = lines.lock()
        {
            guard.push(line);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::time::Instant;

    use super::*;

    #[test]
    fn streams_lines_appended_after_the_watcher_starts() {
        let projects_root =
            std::env::temp_dir().join(format!("cctiles-transcript-test-{}", std::process::id()));
        let cwd = "/fake/project/dir";
        let project_dir = projects_root.join(sanitize_cwd(cwd));
        std::fs::create_dir_all(&project_dir).expect("failed to create fake project dir");

        let jsonl_path = project_dir.join("session-abc.jsonl");
        std::fs::write(&jsonl_path, "{\"line\":\"initial\"}\n")
            .expect("failed to write initial line");

        let watcher = TranscriptWatcher::start_in(projects_root.clone(), cwd);

        // Give the watcher a moment to discover the file and pick up the
        // line that was already there before it started watching.
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && watcher.lines().is_empty() {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(watcher.lines(), vec!["{\"line\":\"initial\"}".to_string()]);

        // Now append a second line and confirm it streams in too.
        {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&jsonl_path)
                .expect("failed to open for append");
            writeln!(file, "{{\"line\":\"appended\"}}").expect("failed to append line");
        }

        let deadline = Instant::now() + Duration::from_secs(3);
        let mut lines = watcher.lines();
        while Instant::now() < deadline && lines.len() < 2 {
            std::thread::sleep(Duration::from_millis(50));
            lines = watcher.lines();
        }

        assert_eq!(
            lines,
            vec![
                "{\"line\":\"initial\"}".to_string(),
                "{\"line\":\"appended\"}".to_string(),
            ]
        );

        let _ = std::fs::remove_dir_all(&projects_root);
    }
}

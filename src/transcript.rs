use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Seek};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::activity::ActivityState;

/// Watches a Claude Code session's `.jsonl` transcript, located by
/// convention at `~/.claude/projects/<cwd with '/' replaced by '-'>/
/// <session-id>.jsonl`, and maintains a cached `ActivityState` from it. The
/// session id isn't known in advance, so this waits for a `.jsonl` file to
/// appear in that directory - specifically one that didn't already exist
/// when watching started, since a directory that's been used before
/// already has leftover `.jsonl` files from earlier sessions, and grabbing
/// whichever one happens to be newest at that instant would - before this
/// session's own file exists yet - just latch onto stale, unrelated
/// content and never reconsider (see #72).
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

    pub fn activity_lines(&self) -> Vec<String> {
        self.activity
            .lock()
            .map(|state| state.recent_lines())
            .unwrap_or_else(|_| vec!["Idle".to_string()])
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
    let pre_existing = jsonl_files_in(&project_dir);
    let Some(file_path) = wait_for_new_jsonl_file(&project_dir, &pre_existing) else {
        return;
    };
    let Ok(mut file) = File::open(&file_path) else {
        return;
    };

    let mut carry = Vec::new();
    drain_new_lines(&mut file, &file_path, &mut carry, &activity);

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
                drain_new_lines(&mut file, &file_path, &mut carry, &activity);
            }
            Ok(_) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                drain_new_lines(&mut file, &file_path, &mut carry, &activity);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn jsonl_files_in(project_dir: &Path) -> HashSet<PathBuf> {
    std::fs::read_dir(project_dir)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.path())
                .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
                .collect()
        })
        .unwrap_or_default()
}

/// Waits (polling) for a `.jsonl` file to show up in `project_dir` that
/// wasn't already present in `pre_existing` - i.e. this session's own
/// transcript, as opposed to a leftover from an earlier session in the same
/// directory. If more than one shows up (shouldn't normally happen for a
/// single session), picks the most recently modified. Runs on a dedicated
/// background thread, so blocking here is fine.
fn wait_for_new_jsonl_file(project_dir: &Path, pre_existing: &HashSet<PathBuf>) -> Option<PathBuf> {
    loop {
        let mut candidates: Vec<PathBuf> = jsonl_files_in(project_dir)
            .into_iter()
            .filter(|path| !pre_existing.contains(path))
            .collect();
        candidates.sort_by_key(|path| {
            std::fs::metadata(path)
                .and_then(|meta| meta.modified())
                .ok()
        });
        if let Some(newest) = candidates.pop() {
            return Some(newest);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Reads whatever is newly available on `file` (its cursor naturally picks
/// up where the previous read left off) and feeds each complete line into
/// `activity`, carrying over any trailing partial line for next time.
fn drain_new_lines(
    file: &mut File,
    file_path: &Path,
    carry: &mut Vec<u8>,
    activity: &Arc<Mutex<ActivityState>>,
) {
    reopen_if_replaced(file, file_path, carry);

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

/// If `file_path` no longer refers to the file we currently have open (e.g.
/// a context-compaction rewrite replaced or truncated it), `file`'s cursor
/// can end up sitting past the real end of the new content - `read_to_end`
/// from there just returns nothing forever, which otherwise makes a tile's
/// activity feed silently freeze partway through a long session. Detected
/// via the inode changing (a full replace) or the on-disk length dropping
/// below where we've already read up to (an in-place truncate), and
/// recovered by reopening fresh from the same path.
fn reopen_if_replaced(file: &mut File, file_path: &Path, carry: &mut Vec<u8>) {
    let Ok(on_disk) = std::fs::metadata(file_path) else {
        return;
    };
    let Ok(open) = file.metadata() else {
        return;
    };
    let Ok(cursor) = file.stream_position() else {
        return;
    };

    let replaced = on_disk.ino() != open.ino() || on_disk.len() < cursor;
    if replaced && let Ok(reopened) = File::open(file_path) {
        *file = reopened;
        carry.clear();
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

        let watcher = TranscriptWatcher::start_in(projects_root.clone(), cwd);

        // The project directory and the session's own file don't exist yet
        // when the watcher starts - they show up a moment later, same as a
        // real `claude` process starting up.
        std::thread::sleep(Duration::from_millis(100));
        std::fs::create_dir_all(&project_dir).expect("failed to create fake project dir");
        let jsonl_path = project_dir.join("session-abc.jsonl");
        std::fs::write(&jsonl_path, assistant_text("initial message") + "\n")
            .expect("failed to write initial line");

        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && watcher.activity_lines() == vec!["Idle".to_string()] {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(
            watcher.activity_lines(),
            vec!["initial message".to_string()]
        );

        // Now append a second line and confirm it streams in too.
        {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&jsonl_path)
                .expect("failed to open for append");
            writeln!(file, "{}", assistant_text("appended message"))
                .expect("failed to append line");
        }

        let expected = vec![
            "initial message".to_string(),
            "appended message".to_string(),
        ];
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && watcher.activity_lines() != expected {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(watcher.activity_lines(), expected);

        let _ = std::fs::remove_dir_all(&projects_root);
    }

    /// A directory that's been used with Claude Code before already has
    /// leftover `.jsonl` files from earlier, unrelated sessions sitting in
    /// it before this session's own file is ever created. The watcher must
    /// not mistake one of those for this session's transcript, no matter how
    /// large or how it compares by mtime (see #72).
    #[test]
    fn ignores_a_pre_existing_jsonl_file_from_an_earlier_session() {
        let projects_root = std::env::temp_dir().join(format!(
            "cctiles-transcript-decoy-test-{}",
            std::process::id()
        ));
        let cwd = "/fake/decoy/project";
        let project_dir = projects_root.join(sanitize_cwd(cwd));
        std::fs::create_dir_all(&project_dir).expect("failed to create fake project dir");

        // A leftover from an earlier session, already sitting in the
        // directory before this session's watcher even starts.
        let decoy_path = project_dir.join("old-session.jsonl");
        std::fs::write(
            &decoy_path,
            assistant_text("stale unrelated content") + "\n",
        )
        .expect("failed to write decoy file");

        let watcher = TranscriptWatcher::start_in(projects_root.clone(), cwd);

        // Give the watcher plenty of opportunity to (incorrectly) latch
        // onto the decoy if it were going to.
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(
            watcher.activity_lines(),
            vec!["Idle".to_string()],
            "must not have picked up the pre-existing decoy file"
        );

        // This session's own file shows up later, as it would once the
        // real `claude` process finishes starting up.
        let jsonl_path = project_dir.join("new-session.jsonl");
        std::fs::write(&jsonl_path, assistant_text("real session content") + "\n")
            .expect("failed to write the new session's transcript");

        let lines = wait_until_last_line_is(&watcher, "real session content");
        assert_eq!(lines, vec!["real session content".to_string()]);

        let _ = std::fs::remove_dir_all(&projects_root);
    }

    fn wait_until_last_line_is(watcher: &TranscriptWatcher, expected_last: &str) -> Vec<String> {
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut lines = watcher.activity_lines();
        while Instant::now() < deadline && lines.last().map(String::as_str) != Some(expected_last) {
            std::thread::sleep(Duration::from_millis(50));
            lines = watcher.activity_lines();
        }
        lines
    }

    /// A context-compaction rewrite (or similar) can replace the transcript
    /// file at the same path with an unrelated new one (a different inode).
    /// Without detecting that, the watcher's already-open handle would just
    /// look permanently quiet, since nothing writes to the old inode anymore.
    #[test]
    fn recovers_after_the_transcript_file_is_replaced_with_a_new_inode() {
        let projects_root = std::env::temp_dir().join(format!(
            "cctiles-transcript-replace-test-{}",
            std::process::id()
        ));
        let cwd = "/fake/replaced/project";
        let project_dir = projects_root.join(sanitize_cwd(cwd));

        let watcher = TranscriptWatcher::start_in(projects_root.clone(), cwd);

        std::thread::sleep(Duration::from_millis(100));
        std::fs::create_dir_all(&project_dir).expect("failed to create fake project dir");
        let jsonl_path = project_dir.join("session-abc.jsonl");
        std::fs::write(&jsonl_path, assistant_text("before replace") + "\n")
            .expect("failed to write initial line");

        let lines = wait_until_last_line_is(&watcher, "before replace");
        assert_eq!(lines.last(), Some(&"before replace".to_string()));

        // Remove the file and create a brand new one at the same path (a
        // different inode), as if it had been replaced out from under the
        // watcher.
        std::fs::remove_file(&jsonl_path).expect("failed to remove file");
        std::fs::write(&jsonl_path, assistant_text("after replace") + "\n")
            .expect("failed to write replacement file");

        let lines = wait_until_last_line_is(&watcher, "after replace");
        assert_eq!(lines.last(), Some(&"after replace".to_string()));

        let _ = std::fs::remove_dir_all(&projects_root);
    }

    /// An in-place truncate (same inode, shorter content) is a distinct
    /// failure mode from a full replace - the cursor ends up past the new
    /// end of file instead of pointing at an inode nothing writes to
    /// anymore - so this needs its own coverage.
    #[test]
    fn recovers_after_the_transcript_file_is_truncated_in_place() {
        let projects_root = std::env::temp_dir().join(format!(
            "cctiles-transcript-truncate-test-{}",
            std::process::id()
        ));
        let cwd = "/fake/truncated/project";
        let project_dir = projects_root.join(sanitize_cwd(cwd));

        let watcher = TranscriptWatcher::start_in(projects_root.clone(), cwd);

        std::thread::sleep(Duration::from_millis(100));
        std::fs::create_dir_all(&project_dir).expect("failed to create fake project dir");
        let jsonl_path = project_dir.join("session-abc.jsonl");
        std::fs::write(
            &jsonl_path,
            assistant_text("a fairly long message before truncation") + "\n",
        )
        .expect("failed to write initial line");

        let lines = wait_until_last_line_is(&watcher, "a fairly long message before truncation");
        assert_eq!(
            lines.last(),
            Some(&"a fairly long message before truncation".to_string())
        );

        {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(&jsonl_path)
                .expect("failed to truncate transcript file");
            writeln!(file, "{}", assistant_text("after truncate"))
                .expect("failed to write truncated content");
        }

        let lines = wait_until_last_line_is(&watcher, "after truncate");
        assert_eq!(lines.last(), Some(&"after truncate".to_string()));

        let _ = std::fs::remove_dir_all(&projects_root);
    }
}

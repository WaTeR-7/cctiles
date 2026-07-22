use std::fs::File;
use std::io::{Read, Seek};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::activity::ActivityState;

/// Watches a Claude Code session's `.jsonl` transcript and maintains a
/// cached `ActivityState` from it. Which file to watch isn't known in
/// advance - it's supplied later via `transcript_path`, populated from the
/// `transcript_path` field of the session's own hook events (see
/// `hooks.rs`) once its first turn actually happens. Following the hooks'
/// own report of the current file, rather than guessing from the directory
/// (e.g. "whichever `.jsonl` is newest"), also means this correctly follows
/// a mid-session switch to a *different* file - which happens if the user
/// runs `/resume` in the floating terminal to continue an earlier session
/// instead of the fresh one cctiles started (see #74, and #72 for the
/// directory-guessing approach this replaced).
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
    pub fn start(transcript_path: Arc<Mutex<Option<PathBuf>>>) -> Self {
        let activity = Arc::new(Mutex::new(ActivityState::default()));
        let activity_for_thread = Arc::clone(&activity);
        std::thread::spawn(move || watch_loop(transcript_path, activity_for_thread));
        Self { activity }
    }

    pub fn activity_lines(&self) -> Vec<String> {
        self.activity
            .lock()
            .map(|state| state.recent_lines())
            .unwrap_or_else(|_| vec!["Idle".to_string()])
    }
}

fn watch_loop(transcript_path: Arc<Mutex<Option<PathBuf>>>, activity: Arc<Mutex<ActivityState>>) {
    loop {
        let Some(path) = wait_for_path(&transcript_path) else {
            return;
        };
        watch_one_file(&path, &transcript_path, &activity);
    }
}

/// Blocks (polling) until `transcript_path` has a value, returning it. Only
/// returns `None` if the lock is poisoned.
fn wait_for_path(transcript_path: &Arc<Mutex<Option<PathBuf>>>) -> Option<PathBuf> {
    loop {
        match transcript_path.lock() {
            Ok(guard) => {
                if let Some(path) = guard.clone() {
                    return Some(path);
                }
            }
            Err(_) => return None,
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn current_path(transcript_path: &Arc<Mutex<Option<PathBuf>>>) -> Option<PathBuf> {
    transcript_path.lock().ok().and_then(|guard| guard.clone())
}

/// Follows a single transcript file until `transcript_path` reports a
/// different one - i.e. until this session gets replaced or resumed to a
/// different one - at which point this returns so `watch_loop` can start
/// following the new one instead.
fn watch_one_file(
    file_path: &Path,
    transcript_path: &Arc<Mutex<Option<PathBuf>>>,
    activity: &Arc<Mutex<ActivityState>>,
) {
    // Fresh state for whichever session we're now following - stale
    // entries from a previous file (before a `/resume` switch) would
    // otherwise linger mixed in with the new one's.
    if let Ok(mut state) = activity.lock() {
        *state = ActivityState::default();
    }

    let mut file = loop {
        if current_path(transcript_path).as_deref() != Some(file_path) {
            return;
        }
        if let Ok(file) = File::open(file_path) {
            break file;
        }
        std::thread::sleep(Duration::from_millis(200));
    };

    let mut carry = Vec::new();
    drain_new_lines(&mut file, file_path, &mut carry, activity);

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
        .watch(file_path, RecursiveMode::NonRecursive)
        .is_err()
    {
        return;
    }

    // Drive draining off both notify events and a periodic fallback tick,
    // since inotify-backed watching has been observed to occasionally miss
    // or delay events in some (e.g. containerized CI) environments. The
    // fallback tick means new content still gets picked up within roughly
    // POLL_INTERVAL even if the event-driven path fails silently; it also
    // doubles as how quickly a `/resume` switch away from this file gets
    // noticed.
    const POLL_INTERVAL: Duration = Duration::from_millis(300);
    loop {
        if current_path(transcript_path).as_deref() != Some(file_path) {
            return;
        }
        match rx.recv_timeout(POLL_INTERVAL) {
            Ok(event) if matches!(event.kind, EventKind::Modify(_)) => {
                drain_new_lines(&mut file, file_path, &mut carry, activity);
            }
            Ok(_) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                drain_new_lines(&mut file, file_path, &mut carry, activity);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
        }
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

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cctiles-transcript-test-{name}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("failed to create scratch dir");
        dir
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

    #[test]
    fn stays_idle_until_a_transcript_path_is_reported() {
        let transcript_path = Arc::new(Mutex::new(None));
        let watcher = TranscriptWatcher::start(Arc::clone(&transcript_path));

        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(watcher.activity_lines(), vec!["Idle".to_string()]);
    }

    #[test]
    fn streams_lines_once_the_transcript_path_is_reported() {
        let dir = scratch_dir("streams");
        let jsonl_path = dir.join("session-abc.jsonl");

        let transcript_path = Arc::new(Mutex::new(None));
        let watcher = TranscriptWatcher::start(Arc::clone(&transcript_path));

        // The path isn't known yet (no hook event has fired), same as a
        // real session before its first turn.
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(watcher.activity_lines(), vec!["Idle".to_string()]);

        std::fs::write(&jsonl_path, assistant_text("initial message") + "\n")
            .expect("failed to write initial line");
        *transcript_path.lock().unwrap() = Some(jsonl_path.clone());

        let lines = wait_until_last_line_is(&watcher, "initial message");
        assert_eq!(lines, vec!["initial message".to_string()]);

        // Now append a second line and confirm it streams in too.
        {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&jsonl_path)
                .expect("failed to open for append");
            writeln!(file, "{}", assistant_text("appended message"))
                .expect("failed to append line");
        }

        let lines = wait_until_last_line_is(&watcher, "appended message");
        assert_eq!(
            lines,
            vec![
                "initial message".to_string(),
                "appended message".to_string(),
            ]
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A directory that's been used with Claude Code before already has
    /// leftover `.jsonl` files from earlier, unrelated sessions sitting in
    /// it. Since the watcher only ever looks at the exact path it's told
    /// about, a decoy sitting nearby (even one that looks "newer") is never
    /// even considered (see #72).
    #[test]
    fn ignores_a_pre_existing_jsonl_file_it_was_never_told_about() {
        let dir = scratch_dir("decoy");
        let decoy_path = dir.join("old-session.jsonl");
        std::fs::write(
            &decoy_path,
            assistant_text("stale unrelated content") + "\n",
        )
        .expect("failed to write decoy file");

        let transcript_path = Arc::new(Mutex::new(None));
        let watcher = TranscriptWatcher::start(Arc::clone(&transcript_path));

        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(
            watcher.activity_lines(),
            vec!["Idle".to_string()],
            "must not have picked up the pre-existing decoy file"
        );

        let jsonl_path = dir.join("new-session.jsonl");
        std::fs::write(&jsonl_path, assistant_text("real session content") + "\n")
            .expect("failed to write the new session's transcript");
        *transcript_path.lock().unwrap() = Some(jsonl_path);

        let lines = wait_until_last_line_is(&watcher, "real session content");
        assert_eq!(lines, vec!["real session content".to_string()]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Simulates `/resume`: the session switches to writing a different,
    /// already-existing file instead of the one it started with. The
    /// watcher must follow the switch (not stay stuck on the old file) and
    /// must not mix the old file's entries in with the new one's (see #74).
    #[test]
    fn follows_a_resume_switch_to_a_different_pre_existing_file() {
        let dir = scratch_dir("resume");
        let first_path = dir.join("session-first.jsonl");
        std::fs::write(&first_path, assistant_text("from the fresh session") + "\n")
            .expect("failed to write first session's transcript");

        let resumed_path = dir.join("session-resumed.jsonl");
        std::fs::write(
            &resumed_path,
            assistant_text("earlier turn from the resumed conversation") + "\n",
        )
        .expect("failed to write resumed session's transcript");

        let transcript_path = Arc::new(Mutex::new(Some(first_path.clone())));
        let watcher = TranscriptWatcher::start(Arc::clone(&transcript_path));

        let lines = wait_until_last_line_is(&watcher, "from the fresh session");
        assert_eq!(lines, vec!["from the fresh session".to_string()]);

        // The user ran `/resume` in the floating terminal and picked the
        // other session - Claude Code now reports that file instead.
        *transcript_path.lock().unwrap() = Some(resumed_path.clone());

        let lines = wait_until_last_line_is(&watcher, "earlier turn from the resumed conversation");
        assert_eq!(
            lines,
            vec!["earlier turn from the resumed conversation".to_string()],
            "must show only the resumed file's content, not a mix with the old one"
        );

        // And it keeps following the resumed file as it grows too.
        {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&resumed_path)
                .expect("failed to open for append");
            writeln!(file, "{}", assistant_text("new turn after resuming"))
                .expect("failed to append line");
        }
        let lines = wait_until_last_line_is(&watcher, "new turn after resuming");
        assert_eq!(
            lines,
            vec![
                "earlier turn from the resumed conversation".to_string(),
                "new turn after resuming".to_string(),
            ]
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A context-compaction rewrite (or similar) can replace the transcript
    /// file at the same path with an unrelated new one (a different inode).
    /// Without detecting that, the watcher's already-open handle would just
    /// look permanently quiet, since nothing writes to the old inode anymore.
    #[test]
    fn recovers_after_the_transcript_file_is_replaced_with_a_new_inode() {
        let dir = scratch_dir("replace");
        let jsonl_path = dir.join("session-abc.jsonl");
        std::fs::write(&jsonl_path, assistant_text("before replace") + "\n")
            .expect("failed to write initial line");

        let transcript_path = Arc::new(Mutex::new(Some(jsonl_path.clone())));
        let watcher = TranscriptWatcher::start(Arc::clone(&transcript_path));
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

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An in-place truncate (same inode, shorter content) is a distinct
    /// failure mode from a full replace - the cursor ends up past the new
    /// end of file instead of pointing at an inode nothing writes to
    /// anymore - so this needs its own coverage.
    #[test]
    fn recovers_after_the_transcript_file_is_truncated_in_place() {
        let dir = scratch_dir("truncate");
        let jsonl_path = dir.join("session-abc.jsonl");
        std::fs::write(
            &jsonl_path,
            assistant_text("a fairly long message before truncation") + "\n",
        )
        .expect("failed to write initial line");

        let transcript_path = Arc::new(Mutex::new(Some(jsonl_path.clone())));
        let watcher = TranscriptWatcher::start(Arc::clone(&transcript_path));
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

        let _ = std::fs::remove_dir_all(&dir);
    }
}

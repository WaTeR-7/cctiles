use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::activity;
use crate::transcript::TranscriptWatcher;

const ROWS: u16 = 24;
const COLS: u16 = 80;

/// Text Claude Code's CLI renders when a tool call needs approval (verified
/// against a real permission prompt while investigating #19). The
/// transcript's .jsonl doesn't record the tool_use until *after* it's
/// approved, so this state can't be detected from the transcript at all -
/// it has to be read off the rendered screen instead.
const PERMISSION_PROMPT_MARKER: &str = "Do you want to proceed?";

pub struct Session {
    child: Mutex<Box<dyn Child + Send + Sync>>,
    master: Box<dyn MasterPty + Send>,
    writer: Mutex<Box<dyn Write + Send>>,
    screen: Arc<Mutex<vt100::Parser>>,
    transcript: TranscriptWatcher,
}

impl Session {
    pub fn spawn(dir: &str) -> anyhow::Result<Self> {
        Self::spawn_command("claude", &[], dir)
    }

    fn spawn_command(program: &str, args: &[&str], dir: &str) -> anyhow::Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows: ROWS,
            cols: COLS,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let mut cmd = CommandBuilder::new(program);
        cmd.args(args);
        cmd.cwd(dir);
        let child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let screen = Arc::new(Mutex::new(vt100::Parser::new(ROWS, COLS, 0)));

        // Continuously drain the PTY's output into the vt100 buffer, even
        // while this tile isn't focused/floated. Without this, a session
        // producing enough output could eventually block on a full
        // kernel-side PTY buffer; this also keeps an always-up-to-date
        // screen snapshot available for #22's reattach rendering.
        let screen_for_thread = Arc::clone(&screen);
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if let Ok(mut parser) = screen_for_thread.lock() {
                            parser.process(&buf[..n]);
                        }
                    }
                }
            }
        });

        let transcript = TranscriptWatcher::start(dir);

        Ok(Session {
            child: Mutex::new(child),
            master: pair.master,
            writer: Mutex::new(writer),
            screen,
            transcript,
        })
    }

    pub fn screen_contents(&self) -> String {
        self.screen
            .lock()
            .map(|parser| parser.screen().contents())
            .unwrap_or_default()
    }

    /// Gives read-only access to the session's live vt100 screen buffer, for
    /// rendering the floating terminal overlay (#22).
    pub fn with_screen<T>(&self, f: impl FnOnce(&vt100::Screen) -> T) -> Option<T> {
        self.screen.lock().ok().map(|parser| f(parser.screen()))
    }

    /// Forwards raw bytes (translated from key events) to the child
    /// process's stdin, as if typed directly into its terminal.
    pub fn write_input(&self, bytes: &[u8]) -> std::io::Result<()> {
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| std::io::Error::other("session writer lock poisoned"))?;
        writer.write_all(bytes)?;
        writer.flush()
    }

    /// Resizes the underlying PTY, so the child process sees a real window
    /// size change (SIGWINCH), and keeps the vt100 buffer's dimensions in
    /// sync so it renders correctly at that size. Used to match the
    /// floating terminal overlay (#22) to the real terminal window instead
    /// of the hardcoded spawn-time default.
    pub fn resize(&self, rows: u16, cols: u16) -> anyhow::Result<()> {
        self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        if let Ok(mut parser) = self.screen.lock() {
            parser.screen_mut().set_size(rows, cols);
        }
        Ok(())
    }

    pub fn activity_summary(&self) -> String {
        activity::summarize(&self.transcript.lines())
    }

    /// Whether the child process is still running. Checked with a
    /// non-blocking `try_wait`, so this is safe to call on every draw (#26).
    pub fn is_alive(&self) -> bool {
        self.child
            .lock()
            .map(|mut child| matches!(child.try_wait(), Ok(None)))
            .unwrap_or(false)
    }

    pub fn status(&self) -> SessionStatus {
        if !self.is_alive() {
            SessionStatus::Crashed
        } else if self.screen_contents().contains(PERMISSION_PROMPT_MARKER) {
            SessionStatus::WaitingForPermission
        } else if activity::is_waiting_for_answer(&self.transcript.lines()) {
            SessionStatus::WaitingForAnswer
        } else {
            SessionStatus::Normal
        }
    }
}

/// A session's status as relevant to the grid's status highlighting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStatus {
    Normal,
    WaitingForAnswer,
    WaitingForPermission,
    /// The child process exited (crashed or otherwise) without being
    /// deliberately killed via 'x' - see #26.
    Crashed,
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    #[test]
    fn screen_buffer_reflects_child_output() {
        let dir = std::env::temp_dir();
        let session = Session::spawn_command(
            "bash",
            &["-c", "printf 'hello-vt100-test\\n'; sleep 2"],
            dir.to_str().expect("temp dir path should be valid utf-8"),
        )
        .expect("failed to spawn test session");

        let deadline = Instant::now() + Duration::from_secs(3);
        let mut contents = String::new();
        while Instant::now() < deadline {
            contents = session.screen_contents();
            if contents.contains("hello-vt100-test") {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        assert!(
            contents.contains("hello-vt100-test"),
            "expected screen contents to contain the test marker, got: {contents:?}"
        );
    }

    #[test]
    fn status_reflects_waiting_for_answer() {
        let scratch_dir =
            std::env::temp_dir().join(format!("cctiles-status-test-{}", std::process::id()));
        std::fs::create_dir_all(&scratch_dir).expect("failed to create scratch dir");
        let dir_str = scratch_dir
            .to_str()
            .expect("scratch dir path should be valid utf-8");

        let session = Session::spawn_command("bash", &["-c", "sleep 5"], dir_str)
            .expect("failed to spawn test session");
        assert_eq!(session.status(), SessionStatus::Normal);

        let sanitized: String = dir_str
            .chars()
            .map(|c| if c == '/' { '-' } else { c })
            .collect();
        let project_dir = directories::BaseDirs::new()
            .expect("should be able to determine home dir")
            .home_dir()
            .join(".claude")
            .join("projects")
            .join(&sanitized);
        std::fs::create_dir_all(&project_dir).expect("failed to create fake project dir");

        let line = serde_json::json!({
            "type": "assistant",
            "message": {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_x", "name": "AskUserQuestion", "input": {}}
            ]},
        })
        .to_string();
        std::fs::write(project_dir.join("fake-session.jsonl"), line + "\n")
            .expect("failed to write fake transcript");

        let deadline = Instant::now() + Duration::from_secs(3);
        let mut status = session.status();
        while Instant::now() < deadline && status != SessionStatus::WaitingForAnswer {
            std::thread::sleep(Duration::from_millis(50));
            status = session.status();
        }
        assert_eq!(status, SessionStatus::WaitingForAnswer);

        let _ = std::fs::remove_dir_all(&project_dir);
    }

    #[test]
    fn status_reflects_waiting_for_permission() {
        let dir = std::env::temp_dir();
        let session = Session::spawn_command(
            "bash",
            &["-c", "printf 'Do you want to proceed?\\n'; sleep 5"],
            dir.to_str().expect("temp dir path should be valid utf-8"),
        )
        .expect("failed to spawn test session");

        let deadline = Instant::now() + Duration::from_secs(3);
        let mut status = session.status();
        while Instant::now() < deadline && status != SessionStatus::WaitingForPermission {
            std::thread::sleep(Duration::from_millis(50));
            status = session.status();
        }
        assert_eq!(status, SessionStatus::WaitingForPermission);
    }

    #[test]
    fn write_input_reaches_the_child_process() {
        let dir = std::env::temp_dir();
        let session = Session::spawn_command(
            "bash",
            &["-c", "read line; printf 'got: %s\\n' \"$line\""],
            dir.to_str().expect("temp dir path should be valid utf-8"),
        )
        .expect("failed to spawn test session");

        session
            .write_input(b"hello\r")
            .expect("failed to write input");

        let deadline = Instant::now() + Duration::from_secs(3);
        let mut contents = String::new();
        while Instant::now() < deadline {
            contents = session.screen_contents();
            if contents.contains("got: hello") {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            contents.contains("got: hello"),
            "expected the child to have received the input, got: {contents:?}"
        );
    }

    #[test]
    fn resize_updates_the_screen_buffer_dimensions() {
        let dir = std::env::temp_dir();
        let session = Session::spawn_command("bash", &["-c", "sleep 2"], dir.to_str().unwrap())
            .expect("failed to spawn test session");

        session.resize(40, 100).expect("failed to resize");

        let size = session
            .with_screen(|screen| screen.size())
            .expect("screen lock should not be poisoned");
        assert_eq!(size, (40, 100));
    }

    #[test]
    fn status_reflects_a_process_that_has_exited() {
        let dir = std::env::temp_dir();
        let session = Session::spawn_command("bash", &["-c", "exit 1"], dir.to_str().unwrap())
            .expect("failed to spawn test session");

        let deadline = Instant::now() + Duration::from_secs(3);
        let mut status = session.status();
        while Instant::now() < deadline && status != SessionStatus::Crashed {
            std::thread::sleep(Duration::from_millis(50));
            status = session.status();
        }
        assert_eq!(status, SessionStatus::Crashed);
    }

    #[test]
    fn spawn_with_a_missing_binary_returns_an_error() {
        let dir = std::env::temp_dir();
        let result = Session::spawn_command(
            "cctiles-definitely-not-a-real-binary",
            &[],
            dir.to_str().expect("temp dir path should be valid utf-8"),
        );
        assert!(result.is_err());
    }
}

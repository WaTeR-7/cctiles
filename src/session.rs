use std::io::Read;
use std::sync::{Arc, Mutex};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::activity;
use crate::transcript::TranscriptWatcher;

const ROWS: u16 = 24;
const COLS: u16 = 80;

pub struct Session {
    child: Box<dyn Child + Send + Sync>,
    #[allow(dead_code)]
    master: Box<dyn MasterPty + Send>,
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
            child,
            master: pair.master,
            screen,
            transcript,
        })
    }

    #[allow(dead_code)]
    pub fn screen_contents(&self) -> String {
        self.screen
            .lock()
            .map(|parser| parser.screen().contents())
            .unwrap_or_default()
    }

    pub fn activity_summary(&self) -> String {
        activity::summarize(&self.transcript.lines())
    }

    pub fn status(&self) -> SessionStatus {
        let lines = self.transcript.lines();
        if activity::is_waiting_for_answer(&lines) {
            SessionStatus::WaitingForAnswer
        } else {
            SessionStatus::Normal
        }
    }
}

/// A session's status as relevant to the grid's status highlighting.
/// `WaitingForPermission` will join this once #19 is resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStatus {
    Normal,
    WaitingForAnswer,
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
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
}

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::git_info::GitStatusWatcher;
use crate::hooks::HookServer;
use crate::transcript::TranscriptWatcher;

const ROWS: u16 = 24;
const COLS: u16 = 80;

pub struct Session {
    child: Mutex<Box<dyn Child + Send + Sync>>,
    master: Box<dyn MasterPty + Send>,
    writer: Mutex<Box<dyn Write + Send>>,
    screen: Arc<Mutex<vt100::Parser>>,
    transcript: TranscriptWatcher,
    git_status: GitStatusWatcher,
    /// Live status fed by Claude Code's own hooks (see `hooks.rs`), or
    /// `None` for sessions spawned without a `HookServer` (only test
    /// sessions that don't run the real `claude` binary and so never fire
    /// any hooks).
    hook_status: Option<Arc<Mutex<SessionStatus>>>,
}

impl Session {
    pub fn spawn(dir: &str, hooks: &HookServer) -> anyhow::Result<Self> {
        let settings_path = hooks.settings_path().to_string_lossy().into_owned();
        Self::spawn_command("claude", &["--settings", &settings_path], dir, Some(hooks))
    }

    fn spawn_command(
        program: &str,
        args: &[&str],
        dir: &str,
        hooks: Option<&HookServer>,
    ) -> anyhow::Result<Self> {
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
        let git_status = GitStatusWatcher::start(dir);
        let hook_status = hooks.map(|hooks| hooks.register(dir));

        Ok(Session {
            child: Mutex::new(child),
            master: pair.master,
            writer: Mutex::new(writer),
            screen,
            transcript,
            git_status,
            hook_status,
        })
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

    pub fn activity_lines(&self) -> Vec<String> {
        self.transcript.activity_lines()
    }

    /// The current branch and working-tree diffstat (e.g. `"main  +3/-1"`),
    /// or `None` when `dir` isn't a git repository.
    pub fn git_status_summary(&self) -> Option<String> {
        self.git_status.summary()
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
            return SessionStatus::Crashed;
        }
        self.hook_status
            .as_ref()
            .and_then(|state| state.lock().ok().map(|status| *status))
            .unwrap_or(SessionStatus::Idle)
    }
}

/// A session's status as relevant to the grid's status highlighting, driven
/// by Claude Code's own hooks (see `hooks.rs`) rather than by scraping the
/// rendered screen or re-deriving state from the transcript - see #57.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStatus {
    Idle,
    Working,
    WaitingForAnswer,
    WaitingForPermission,
    /// A backgrounded shell task (`run_in_background`) is still running
    /// after the turn that started it has already ended.
    BackgroundTaskRunning,
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

    use serde_json::json;

    use super::*;
    use crate::hooks::post_hook_event;

    fn screen_contents(session: &Session) -> String {
        session
            .with_screen(|screen| screen.contents())
            .unwrap_or_default()
    }

    #[test]
    fn screen_buffer_reflects_child_output() {
        let dir = std::env::temp_dir();
        let session = Session::spawn_command(
            "bash",
            &["-c", "printf 'hello-vt100-test\\n'; sleep 2"],
            dir.to_str().expect("temp dir path should be valid utf-8"),
            None,
        )
        .expect("failed to spawn test session");

        let deadline = Instant::now() + Duration::from_secs(3);
        let mut contents = String::new();
        while Instant::now() < deadline {
            contents = screen_contents(&session);
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
    fn status_defaults_to_idle_without_a_hook_server() {
        let dir = std::env::temp_dir();
        let session =
            Session::spawn_command("bash", &["-c", "sleep 2"], dir.to_str().unwrap(), None)
                .expect("failed to spawn test session");
        assert_eq!(session.status(), SessionStatus::Idle);
    }

    /// Simulates the hook lifecycle a real turn produces (see #57's live
    /// capture): a `UserPromptSubmit` marks the session as working, a
    /// `PreToolUse` for `AskUserQuestion` means it's now blocked on an
    /// interactive answer, and a `PermissionRequest` means it's blocked on a
    /// permission prompt instead.
    #[test]
    fn status_follows_hook_events_for_working_answer_and_permission() {
        let scratch_dir =
            std::env::temp_dir().join(format!("cctiles-hook-status-test-{}", std::process::id()));
        std::fs::create_dir_all(&scratch_dir).expect("failed to create scratch dir");
        let dir_str = scratch_dir
            .to_str()
            .expect("scratch dir path should be valid utf-8");

        let hooks = HookServer::start().expect("failed to start hook server");
        let session = Session::spawn_command("bash", &["-c", "sleep 5"], dir_str, Some(&hooks))
            .expect("failed to spawn test session");
        assert_eq!(session.status(), SessionStatus::Idle);

        post_hook_event(
            hooks.port(),
            &json!({"hook_event_name": "UserPromptSubmit", "cwd": dir_str}),
        );
        assert_eq!(
            wait_for_status(&session, SessionStatus::Working),
            SessionStatus::Working
        );

        post_hook_event(
            hooks.port(),
            &json!({"hook_event_name": "PreToolUse", "cwd": dir_str, "tool_name": "AskUserQuestion"}),
        );
        assert_eq!(
            wait_for_status(&session, SessionStatus::WaitingForAnswer),
            SessionStatus::WaitingForAnswer
        );

        post_hook_event(
            hooks.port(),
            &json!({"hook_event_name": "PermissionRequest", "cwd": dir_str}),
        );
        assert_eq!(
            wait_for_status(&session, SessionStatus::WaitingForPermission),
            SessionStatus::WaitingForPermission
        );

        let _ = std::fs::remove_dir_all(&scratch_dir);
    }

    /// A `Stop` with a non-empty `background_tasks` array (an undocumented
    /// field confirmed via a live capture in #57) means a backgrounded shell
    /// is still running even though the turn ended; an empty array means the
    /// session is genuinely idle.
    #[test]
    fn status_distinguishes_background_task_running_from_idle_on_stop() {
        let scratch_dir = std::env::temp_dir().join(format!(
            "cctiles-hook-status-bg-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&scratch_dir).expect("failed to create scratch dir");
        let dir_str = scratch_dir
            .to_str()
            .expect("scratch dir path should be valid utf-8");

        let hooks = HookServer::start().expect("failed to start hook server");
        let session = Session::spawn_command("bash", &["-c", "sleep 5"], dir_str, Some(&hooks))
            .expect("failed to spawn test session");

        post_hook_event(
            hooks.port(),
            &json!({
                "hook_event_name": "Stop",
                "cwd": dir_str,
                "background_tasks": [{"id": "abc", "status": "running"}],
            }),
        );
        assert_eq!(
            wait_for_status(&session, SessionStatus::BackgroundTaskRunning),
            SessionStatus::BackgroundTaskRunning
        );

        post_hook_event(
            hooks.port(),
            &json!({"hook_event_name": "Stop", "cwd": dir_str, "background_tasks": []}),
        );
        assert_eq!(
            wait_for_status(&session, SessionStatus::Idle),
            SessionStatus::Idle
        );

        let _ = std::fs::remove_dir_all(&scratch_dir);
    }

    fn wait_for_status(session: &Session, expected: SessionStatus) -> SessionStatus {
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut status = session.status();
        while Instant::now() < deadline && status != expected {
            std::thread::sleep(Duration::from_millis(20));
            status = session.status();
        }
        status
    }

    #[test]
    fn write_input_reaches_the_child_process() {
        let dir = std::env::temp_dir();
        let session = Session::spawn_command(
            "bash",
            &["-c", "read line; printf 'got: %s\\n' \"$line\""],
            dir.to_str().expect("temp dir path should be valid utf-8"),
            None,
        )
        .expect("failed to spawn test session");

        session
            .write_input(b"hello\r")
            .expect("failed to write input");

        let deadline = Instant::now() + Duration::from_secs(3);
        let mut contents = String::new();
        while Instant::now() < deadline {
            contents = screen_contents(&session);
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
        let session =
            Session::spawn_command("bash", &["-c", "sleep 2"], dir.to_str().unwrap(), None)
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
        let session =
            Session::spawn_command("bash", &["-c", "exit 1"], dir.to_str().unwrap(), None)
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
            None,
        );
        assert!(result.is_err());
    }
}

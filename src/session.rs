use std::io::Read;
use std::sync::{Arc, Mutex};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

const ROWS: u16 = 24;
const COLS: u16 = 80;

pub struct Session {
    child: Box<dyn Child + Send + Sync>,
    #[allow(dead_code)]
    master: Box<dyn MasterPty + Send>,
    screen: Arc<Mutex<vt100::Parser>>,
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

        Ok(Session {
            child,
            master: pair.master,
            screen,
        })
    }

    #[allow(dead_code)]
    pub fn screen_contents(&self) -> String {
        self.screen
            .lock()
            .map(|parser| parser.screen().contents())
            .unwrap_or_default()
    }
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
}

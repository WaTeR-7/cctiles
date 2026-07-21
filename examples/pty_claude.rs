// Spike for issue "Spike: run the actual claude CLI as the PTY child and
// validate full interactivity". Run with: cargo run --example pty_claude
//
// Same raw-passthrough approach as pty_input.rs, but spawns the real
// `claude` binary instead of bash, to validate that its colors, redraws,
// and interactive prompts survive being relayed through our PTY wrapper.
// Runs in an isolated scratch directory so a nested session has nothing
// of the actual project to touch.
use std::io::{self, Read, Write};
use std::thread;

use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

fn main() -> anyhow::Result<()> {
    let scratch_dir = std::env::temp_dir().join("cctiles-pty-claude-spike");
    std::fs::create_dir_all(&scratch_dir)?;

    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    let mut cmd = CommandBuilder::new("claude");
    cmd.cwd(&scratch_dir);
    let mut child = pair.slave.spawn_command(cmd)?;
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader()?;
    let mut writer = pair.master.take_writer()?;

    enable_raw_mode()?;

    let output_thread = thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut stdout = io::stdout();
                    let _ = stdout.write_all(&buf[..n]);
                    let _ = stdout.flush();
                }
            }
        }
        let _ = disable_raw_mode();
        std::process::exit(0);
    });

    let mut stdin = io::stdin();
    let mut buf = [0u8; 1024];
    loop {
        let n = stdin.read(&mut buf)?;
        if n == 0 || writer.write_all(&buf[..n]).is_err() {
            break;
        }
        let _ = writer.flush();
    }

    disable_raw_mode()?;
    let _ = child.wait();
    let _ = output_thread.join();
    Ok(())
}

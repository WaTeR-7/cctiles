// Spike for issue "Spike: forward keyboard input to the PTY child process".
// Run with: cargo run --example pty_input
//
// Puts our own terminal in raw mode and relays raw bytes bidirectionally
// between it and a bash session running under a PTY, like a minimal
// terminal multiplexer. Type `exit` (or press Ctrl+D) in the child shell
// to end the session.
use std::io::{self, Read, Write};
use std::thread;

use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

fn main() -> anyhow::Result<()> {
    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    let cmd = CommandBuilder::new("bash");
    let mut child = pair.slave.spawn_command(cmd)?;
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader()?;
    let mut writer = pair.master.take_writer()?;

    enable_raw_mode()?;

    // Relay child output to our stdout. Once the child exits, its PTY
    // closes and this thread sees EOF; it then restores the terminal and
    // ends the process so the blocked stdin read below doesn't have to
    // wait for one more keystroke to notice.
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

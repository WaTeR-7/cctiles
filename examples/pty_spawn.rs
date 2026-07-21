// Spike for issue "Spike: spawn a child process with a PTY and capture its
// raw output". Run with: cargo run --example pty_spawn
use std::io::Read;

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

fn main() -> anyhow::Result<()> {
    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    let mut cmd = CommandBuilder::new("bash");
    cmd.args([
        "-c",
        "for i in 1 2 3; do echo \"hello from child: $i\"; sleep 0.3; done",
    ]);
    let mut child = pair.slave.spawn_command(cmd)?;
    // Drop our copy of the slave so the master's reader sees EOF once the
    // child exits, instead of hanging forever waiting for other writers.
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader()?;
    let mut buf = [0u8; 4096];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => print!("{}", String::from_utf8_lossy(&buf[..n])),
            Err(err) => {
                eprintln!("read error: {err}");
                break;
            }
        }
    }

    child.wait()?;
    Ok(())
}

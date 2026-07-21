use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

pub struct Session {
    child: Box<dyn Child + Send + Sync>,
    #[allow(dead_code)]
    master: Box<dyn MasterPty + Send>,
}

impl Session {
    pub fn spawn(dir: &str) -> anyhow::Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let mut cmd = CommandBuilder::new("claude");
        cmd.cwd(dir);
        let child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave);

        Ok(Session {
            child,
            master: pair.master,
        })
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

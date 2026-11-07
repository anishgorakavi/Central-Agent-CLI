//! An embedded terminal: runs a real CLI (Claude Code, Codex...) in a pseudo-terminal and keeps its screen in a vt100 emulator.
use crate::config::Launch;
use anyhow::{Context, Result};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::io::{Read, Write};
use std::num::NonZeroU16;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

// Enough history to scroll back through a long Claude Code session.
const SCROLLBACK_LINES: usize = 5000;
const READ_BUFFER_BYTES: usize = 16 * 1024;

/// A child process attached to a pseudo-terminal, plus the emulated screen it draws on.
/// The process is killed on drop.
pub struct PtyPane {
    parser: Arc<Mutex<vt100::Parser>>,
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    alive: Arc<AtomicBool>,
    size: (u16, u16),
}

impl PtyPane {
    /// Starts `launch` in `cwd` on a `rows` x `cols` terminal. Output is parsed on a background thread.
    pub fn spawn(launch: &Launch, cwd: &Path, rows: u16, cols: u16) -> Result<Self> {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("opening a pseudo-terminal")?;
        let mut command = CommandBuilder::new(&launch.program);
        command.args(&launch.args);
        command.cwd(cwd);
        // The embedded emulator understands xterm sequences and 24-bit color.
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        for (name, value) in &launch.env {
            command.env(name, value);
        }
        let child = pair
            .slave
            .spawn_command(command)
            .with_context(|| format!("starting {}", launch.program))?;
        // Closing our copy of the slave lets the reader see EOF when the child exits.
        drop(pair.slave);

        let mut reader = pair
            .master
            .try_clone_reader()
            .context("cloning the pty reader")?;
        let writer = pair.master.take_writer().context("taking the pty writer")?;
        let parser = Arc::new(Mutex::new(vt100::Parser::new(
            non_zero(rows),
            non_zero(cols),
            SCROLLBACK_LINES,
        )));
        let alive = Arc::new(AtomicBool::new(true));
        let (feed, done) = (parser.clone(), alive.clone());
        std::thread::spawn(move || {
            let mut buffer = [0u8; READ_BUFFER_BYTES];
            while let Ok(read) = reader.read(&mut buffer) {
                if read == 0 {
                    break;
                }
                feed.lock().expect("parser lock").process(&buffer[..read]);
            }
            done.store(false, Ordering::Relaxed);
        });
        Ok(PtyPane {
            parser,
            writer,
            master: pair.master,
            child,
            alive,
            size: (rows, cols),
        })
    }

    /// True while the child process is still running.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    /// Resizes the pty and the emulated screen together; a no-op when the size is unchanged.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        if self.size == (rows, cols) {
            return;
        }
        self.size = (rows, cols);
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
        self.parser
            .lock()
            .expect("parser lock")
            .set_size(non_zero(rows), non_zero(cols));
    }

    /// Sends raw bytes to the program's stdin and snaps the view back to the live screen.
    pub fn write(&mut self, bytes: &[u8]) {
        self.scroll_to_live();
        let _ = self.writer.write_all(bytes);
        let _ = self.writer.flush();
    }

    /// Runs `f` with the emulated screen locked.
    pub fn with_screen<R>(&self, f: impl FnOnce(&vt100::Screen) -> R) -> R {
        f(self.parser.lock().expect("parser lock").screen())
    }

    /// Scrolls the view by `lines` (positive = toward older output); the emulator clamps to its history.
    pub fn scroll(&self, lines: isize) {
        let mut parser = self.parser.lock().expect("parser lock");
        let current = parser.screen().scrollback();
        parser
            .screen_mut()
            .set_scrollback(current.saturating_add_signed(lines));
    }

    /// Returns the view to the live screen.
    pub fn scroll_to_live(&self) {
        self.parser
            .lock()
            .expect("parser lock")
            .screen_mut()
            .set_scrollback(0);
    }
}

/// The emulator requires non-zero dimensions; a collapsed layout can briefly produce 0.
fn non_zero(n: u16) -> NonZeroU16 {
    NonZeroU16::new(n).unwrap_or(NonZeroU16::MIN)
}

impl Drop for PtyPane {
    /// Stops the child so no orphan process outlives its tab. portable-pty's own kill sends SIGHUP,
    /// which Claude Code survives, so the child is SIGKILLed to keep quitting instantaneous.
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.child.process_id() {
            // SAFETY: plain kill(2) on the child pid we spawned; no memory is touched.
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Launches `sh -c script` on a 10x40 terminal.
    fn shell(script: &str) -> PtyPane {
        let launch = Launch {
            program: "sh".to_owned(),
            args: vec!["-c".to_owned(), script.to_owned()],
            env: Default::default(),
        };
        PtyPane::spawn(&launch, Path::new("."), 10, 40).unwrap()
    }

    /// Polls until the screen text contains `needle` or two seconds pass.
    fn wait_for(pane: &PtyPane, needle: &str) -> bool {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if pane.with_screen(|screen| screen.contents().contains(needle)) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    // Program output ends up on the emulated screen, and exit is detected.
    #[test]
    fn captures_output_and_exit() {
        let pane = shell("printf 'hello pty'");
        assert!(wait_for(&pane, "hello pty"));
        let deadline = Instant::now() + Duration::from_secs(2);
        while pane.is_alive() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!pane.is_alive());
    }

    // Input written to the pane reaches the program, and the terminal reports the resized dimensions.
    #[test]
    fn forwards_input_and_resizes() {
        let mut pane = shell("read line; echo got:$line; stty size");
        pane.resize(12, 50);
        pane.write(b"abc\r");
        assert!(wait_for(&pane, "got:abc"));
        assert!(wait_for(&pane, "12 50"));
    }
}

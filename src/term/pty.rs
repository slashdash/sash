// SPDX-FileCopyrightText: 2026 Sash contributors
// SPDX-License-Identifier: MIT

//! Pty plumbing: spawn a shell in a real pty and stream its output into the app thread.

use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};

use anyhow::{Context, Result};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

/// Wakes the UI thread after pty output, so `poll` drains it at once instead of
/// waiting for the next input event or idle tick.
pub type Waker = Arc<dyn Fn() + Send + Sync>;

pub struct Pty {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    rx: Receiver<Vec<u8>>,
    eof: bool,
    pub shell: String,
}

impl Pty {
    pub fn spawn_shell(
        cols: u16,
        rows: u16,
        cell_w: u32,
        cell_h: u32,
        waker: Waker,
    ) -> Result<Self> {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
        let mut command = CommandBuilder::new(&shell);
        command.arg("-l");
        Self::spawn(command, cols, rows, cell_w, cell_h, shell, waker)
    }

    pub fn spawn(
        command: CommandBuilder,
        cols: u16,
        rows: u16,
        cell_w: u32,
        cell_h: u32,
        label: String,
        waker: Waker,
    ) -> Result<Self> {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: cell_w.saturating_mul(cols as u32) as u16,
                pixel_height: cell_h.saturating_mul(rows as u32) as u16,
            })
            .context("openpty")?;

        let mut command = command;
        command.env("TERM", "xterm-256color");
        command.env("TERM_PROGRAM", "sash");
        command.env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));
        command.env("COLORTERM", "truecolor");
        if std::env::var_os("LANG").is_none() {
            command.env("LANG", "en_US.UTF-8");
        }

        let child = pair.slave.spawn_command(command).context("spawn shell")?;
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().context("clone pty reader")?;
        let writer = pair.master.take_writer().context("take pty writer")?;
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name(format!("sash-pty-{label}"))
            .spawn(move || {
                let mut buf = [0u8; 16 * 1024];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if tx.send(buf[..n].to_vec()).is_err() {
                                break;
                            }
                            waker();
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
                // EOF: wake once more so `poll` paints the exit line.
                waker();
            })
            .context("spawn pty reader thread")?;

        Ok(Self {
            master: pair.master,
            writer,
            child,
            rx,
            eof: false,
            shell: label,
        })
    }

    /// Every chunk read since the last call, concatenated.
    pub fn drain(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            match self.rx.try_recv() {
                Ok(chunk) => {
                    // A burst usually spans several reads; reserve room once so
                    // concatenating them does not reallocate per chunk.
                    out.reserve(chunk.len().max(64 * 1024));
                    out.extend_from_slice(&chunk);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.eof = true;
                    break;
                }
            }
        }
        out
    }

    pub fn eof(&self) -> bool {
        self.eof
    }

    /// The shell's pid, so its live working directory can be asked for directly.
    pub fn pid(&self) -> Option<u32> {
        self.child.process_id()
    }

    /// The foreground process group of the pty, which is the shell itself at a
    /// prompt and a child (vim, ssh, a running job) while it works.
    pub fn foreground_pgid(&self) -> Option<u32> {
        let fd = self.master.as_raw_fd()?;
        let pgid = unsafe { libc::tcgetpgrp(fd) };
        (pgid > 0).then_some(pgid as u32)
    }

    /// Whether something other than the shell owns the terminal right now.
    pub fn has_running_command(&self) -> bool {
        let Some(fg) = self.foreground_pgid() else {
            return false;
        };
        self.pid().is_some_and(|shell| fg != shell)
    }

    /// The shell's current directory, if the kernel will tell us.
    pub fn cwd(&self) -> Option<std::path::PathBuf> {
        super::cwd::cwd_of(self.pid()?)
    }

    pub fn write(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let _ = self.writer.write_all(bytes);
        let _ = self.writer.flush();
    }

    pub fn resize(&self, cols: u16, rows: u16, cell_w: u32, cell_h: u32) {
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: cell_w.saturating_mul(cols as u32) as u16,
            pixel_height: cell_h.saturating_mul(rows as u32) as u16,
        });
    }

    pub fn try_wait(&mut self) -> Option<portable_pty::ExitStatus> {
        match self.child.try_wait() {
            Ok(Some(status)) => Some(status),
            _ => None,
        }
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        let _ = self.child.kill();
        // Reap the child so a closed tab does not leave a zombie until quit.
        let _ = self.child.wait();
    }
}

use parking_lot::RwLock;
use rustix::termios::{self, Winsize};
use rustix_openpty::openpty;
use std::io::Read;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::parser::VteHandler;
use super::TerminalState;

/// Global registry of live PTYs: (child_pid, per-instance shutdown flag).
/// Used by `shutdown_all()` on app termination to signal every PTY reader thread.
static PTY_REGISTRY: parking_lot::Mutex<Vec<(u32, Arc<AtomicBool>)>> =
    parking_lot::Mutex::new(Vec::new());

pub struct Pty {
    master_fd: OwnedFd,
    child_pid: u32,
    shutdown: Arc<AtomicBool>,
}

impl Pty {
    pub fn spawn(
        cols: u16,
        rows: u16,
        terminal: Arc<RwLock<TerminalState>>,
        shell_exited: Arc<AtomicBool>,
        shell_ready: Arc<AtomicBool>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let pty_pair = openpty(None, None)?;

        let master_fd = pty_pair.controller;
        let slave_fd = pty_pair.user;

        // Set initial window size
        let winsize = Winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let _ = termios::tcsetwinsize(master_fd.as_fd(), winsize);

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
        // Login shell: arg0 = "-" + shell name (e.g. "-zsh")
        let shell_name = std::path::Path::new(&shell)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("zsh");
        let arg0 = format!("-{}", shell_name);

        let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());

        // Raw fd values for use inside pre_exec (which is async-signal-safe)
        let slave_raw = slave_fd.as_raw_fd();
        let master_raw = master_fd.as_raw_fd();

        // Spawn shell using Command + pre_exec (like Alacritty).
        // pre_exec runs in the child after fork, before exec — the correct
        // place for setsid + TIOCSCTTY to establish the controlling terminal.
        let child = unsafe {
            std::process::Command::new(&shell)
                .arg0(&arg0)
                .stdin(std::process::Stdio::from(std::fs::File::from_raw_fd(libc::dup(slave_raw))))
                .stdout(std::process::Stdio::from(std::fs::File::from_raw_fd(libc::dup(slave_raw))))
                .stderr(std::process::Stdio::from(std::fs::File::from_raw_fd(libc::dup(slave_raw))))
                .env("TERM", "xterm-256color")
                .env("TERM_PROGRAM", "Kova")
                .current_dir(&home)
                .pre_exec(move || {
                    // New session — required before TIOCSCTTY
                    if libc::setsid() == -1 {
                        return Err(std::io::Error::last_os_error());
                    }

                    // Set the slave PTY as controlling terminal
                    // (stdin fd 0 is the slave after Command sets it up)
                    if libc::ioctl(0, libc::TIOCSCTTY as libc::c_ulong, 0) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }

                    // Close fds that the child doesn't need
                    libc::close(slave_raw);
                    libc::close(master_raw);

                    // Reset signal handlers
                    libc::signal(libc::SIGCHLD, libc::SIG_DFL);
                    libc::signal(libc::SIGHUP, libc::SIG_DFL);
                    libc::signal(libc::SIGINT, libc::SIG_DFL);
                    libc::signal(libc::SIGQUIT, libc::SIG_DFL);
                    libc::signal(libc::SIGTERM, libc::SIG_DFL);
                    libc::signal(libc::SIGALRM, libc::SIG_DFL);

                    Ok(())
                })
                .spawn()?
        };

        let child_pid = child.id();
        drop(slave_fd);

        let shutdown = Arc::new(AtomicBool::new(false));
        PTY_REGISTRY.lock().push((child_pid, shutdown.clone()));

        let dup_fd = unsafe { libc::dup(master_fd.as_raw_fd()) };
        if dup_fd < 0 {
            return Err("dup() failed".into());
        }
        let reader_fd = unsafe { OwnedFd::from_raw_fd(dup_fd) };

        // Dup for VteHandler write-back (CSI responses)
        let writer_dup = unsafe { libc::dup(master_fd.as_raw_fd()) };
        if writer_dup < 0 {
            return Err("dup() failed for writer".into());
        }
        let writer_fd = Arc::new(unsafe { OwnedFd::from_raw_fd(writer_dup) });

        let reader_shutdown = shutdown.clone();
        std::thread::Builder::new()
            .name("pty-reader".into())
            .spawn(move || {
                let mut file = unsafe { std::fs::File::from_raw_fd(reader_fd.into_raw_fd()) };
                let mut parser = vte::Parser::new();
                let mut handler = VteHandler::new(terminal, writer_fd);
                let mut buf = [0u8; 4096];
                let mut eof = false;

                loop {
                    if reader_shutdown.load(Ordering::Relaxed) {
                        break;
                    }
                    match file.read(&mut buf) {
                        Ok(0) => { eof = true; break; }
                        Ok(n) => {
                            if !shell_ready.load(Ordering::Relaxed) {
                                shell_ready.store(true, Ordering::Relaxed);
                            }
                            parser.advance(&mut handler, &buf[..n]);
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => { eof = true; break; }
                    }
                }
                if eof {
                    shell_exited.store(true, Ordering::Relaxed);
                }
                log::info!("PTY reader thread exiting");
            })?;

        Ok(Pty {
            master_fd,
            child_pid,
            shutdown,
        })
    }

    pub fn write(&self, data: &[u8]) {
        let _ = rustix::io::write(&self.master_fd, data);
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        let winsize = Winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let _ = termios::tcsetwinsize(self.master_fd.as_fd(), winsize);
        // TIOCSWINSZ (via tcsetwinsize) automatically sends SIGWINCH to the
        // foreground process group when the controlling terminal is properly
        // established (setsid + TIOCSCTTY in pre_exec).
    }
}

/// Signal all live PTY reader threads to stop and kill their child processes.
/// Called once from `AppDelegate::will_terminate`.
pub fn shutdown_all() {
    let entries = PTY_REGISTRY.lock().clone();
    for (pid, shutdown) in &entries {
        shutdown.store(true, Ordering::Relaxed);
        unsafe {
            libc::kill(*pid as i32, libc::SIGHUP);
        }
    }
    // Give children a moment to exit, then reap
    std::thread::sleep(std::time::Duration::from_millis(50));
    for (pid, _) in &entries {
        unsafe {
            libc::waitpid(*pid as i32, std::ptr::null_mut(), libc::WNOHANG);
        }
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        unsafe {
            libc::kill(self.child_pid as i32, libc::SIGHUP);
            libc::waitpid(self.child_pid as i32, std::ptr::null_mut(), 0);
        }
        PTY_REGISTRY
            .lock()
            .retain(|(pid, _)| *pid != self.child_pid);
        log::info!("PTY child {} cleaned up", self.child_pid);
    }
}

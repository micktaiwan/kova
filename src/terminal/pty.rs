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

/// Entry in the global PTY registry.
struct PtyEntry {
    child_pid: u32,
    /// Raw fd of the master PTY. Valid as long as this entry is in the registry.
    /// SAFETY: `Pty::drop` removes the entry *before* `OwnedFd` is dropped,
    /// so the fd is always valid while the entry exists.
    master_fd: i32,
    shutdown: Arc<AtomicBool>,
}

impl Clone for PtyEntry {
    fn clone(&self) -> Self {
        PtyEntry { child_pid: self.child_pid, master_fd: self.master_fd, shutdown: self.shutdown.clone() }
    }
}

/// Global registry of live PTYs.
/// Used by `shutdown_all()` on app termination to signal every PTY reader thread,
/// and by `foreground_process_count()` to check running processes globally.
static PTY_REGISTRY: parking_lot::Mutex<Vec<PtyEntry>> =
    parking_lot::Mutex::new(Vec::new());

/// Returns the foreground process group ID if it differs from the shell's PID
/// (i.e. a command like vim, cargo, etc. is running).
fn foreground_pgid(master_fd: i32, child_pid: u32) -> Option<i32> {
    let fg_pgid = unsafe { libc::tcgetpgrp(master_fd) };
    if fg_pgid > 0 && fg_pgid != child_pid as i32 {
        Some(fg_pgid)
    } else {
        None
    }
}

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
        working_dir: Option<&str>,
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

        let start_dir = working_dir
            .map(String::from)
            .unwrap_or_else(|| std::env::var("HOME").unwrap_or_else(|_| "/".to_string()));

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
                .env("KOVA_SHELL_INTEGRATION", "1")
                .current_dir(&start_dir)
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
        log::info!("PTY spawned: pid={}, shell={}, cols={}, rows={}, cwd={}", child_pid, shell, cols, rows, start_dir);
        drop(slave_fd);

        let shutdown = Arc::new(AtomicBool::new(false));
        PTY_REGISTRY.lock().push(PtyEntry { child_pid, master_fd: master_fd.as_raw_fd(), shutdown: shutdown.clone() });

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
                            handler.release_guard();
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(e) => { log::warn!("PTY read error: {}", e); eof = true; break; }
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
        log::debug!("PTY resize: pid={}, cols={}, rows={}", self.child_pid, cols, rows);
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

    /// Returns the name of the foreground process if it differs from the shell
    /// (i.e. a command like vim, cargo, etc. is running).
    pub fn foreground_process_name(&self) -> Option<String> {
        let fg_pgid = foreground_pgid(self.master_fd.as_raw_fd(), self.child_pid)?;
        let mut name_buf = [0u8; 256];
        let len = unsafe {
            libc::proc_name(fg_pgid, name_buf.as_mut_ptr() as *mut libc::c_void, 256)
        };
        if len > 0 {
            Some(String::from_utf8_lossy(&name_buf[..len as usize]).to_string())
        } else {
            None
        }
    }

    /// Returns the current working directory of the child shell process.
    /// Uses macOS `proc_pidinfo` with `PROC_PIDVNODEPATHINFO`.
    pub fn cwd(&self) -> Option<String> {
        unsafe {
            let mut vpi: libc::proc_vnodepathinfo = std::mem::zeroed();
            let ret = libc::proc_pidinfo(
                self.child_pid as i32,
                libc::PROC_PIDVNODEPATHINFO,
                0,
                &mut vpi as *mut _ as *mut libc::c_void,
                std::mem::size_of::<libc::proc_vnodepathinfo>() as i32,
            );
            if ret <= 0 {
                return None;
            }
            let path = std::ffi::CStr::from_ptr(vpi.pvi_cdir.vip_path.as_ptr() as *const i8);
            path.to_str().ok().map(String::from)
        }
    }
}

/// Escalate signals to reap a child process: SIGHUP → SIGTERM → SIGKILL.
/// Each step waits `step_ms` before checking with `waitpid(WNOHANG)`.
fn reap_child(pid: i32, step_ms: u64) {
    let signals = [
        (libc::SIGHUP, "SIGHUP"),
        (libc::SIGTERM, "SIGTERM"),
        (libc::SIGKILL, "SIGKILL"),
    ];
    for (sig, name) in &signals {
        unsafe {
            if libc::kill(pid, *sig) != 0 {
                log::debug!("reap_child: pid {} already gone before {}", pid, name);
                return;
            }
        }
        log::debug!("reap_child: sent {} to pid {}", name, pid);
        std::thread::sleep(std::time::Duration::from_millis(step_ms));
        let ret = unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) };
        if ret != 0 {
            log::info!("reap_child: pid {} reaped after {}", pid, name);
            return;
        }
    }
    log::warn!("reap_child: pid {} still alive after SIGKILL (should not happen)", pid);
}

/// Count how many PTYs have a foreground process that differs from the shell.
/// This is the global equivalent of `Pane::foreground_process_name().is_some()`.
pub fn foreground_process_count() -> u32 {
    let registry = PTY_REGISTRY.lock();
    registry.iter().filter(|e| foreground_pgid(e.master_fd, e.child_pid).is_some()).count() as u32
}

/// Signal all live PTY reader threads to stop and kill their child processes.
/// Called once from `AppDelegate::will_terminate`.
pub fn shutdown_all() {
    let entries = PTY_REGISTRY.lock().clone();
    log::info!("Shutting down {} PTY(s)", entries.len());
    let handles: Vec<_> = entries
        .into_iter()
        .map(|entry| {
            entry.shutdown.store(true, Ordering::Relaxed);
            let pid = entry.child_pid;
            std::thread::Builder::new()
                .name(format!("pty-reaper-{}", pid))
                .spawn(move || reap_child(pid as i32, 25))
        })
        .collect();
    for h in handles {
        if let Ok(h) = h {
            let _ = h.join();
        }
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        let pid = self.child_pid;
        PTY_REGISTRY.lock().retain(|e| e.child_pid != pid);
        let result = std::thread::Builder::new()
            .name(format!("pty-reaper-{}", pid))
            .spawn(move || reap_child(pid as i32, 50));
        if let Err(e) = result {
            log::warn!("Failed to spawn reaper for pid {}: {}, reaping synchronously", pid, e);
            reap_child(pid as i32, 50);
        }
        log::info!("PTY child {} cleanup delegated to reaper thread", pid);
    }
}

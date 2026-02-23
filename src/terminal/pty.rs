use parking_lot::RwLock;
use rustix::termios::{self, Winsize};
use rustix_openpty::openpty;
use std::io::Read;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::parser::VteHandler;
use super::TerminalState;

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

static PTY_PIDS: parking_lot::Mutex<Vec<u32>> = parking_lot::Mutex::new(Vec::new());

pub struct Pty {
    master_fd: OwnedFd,
    child_pid: u32,
}

impl Pty {
    pub fn spawn(
        cols: u16,
        rows: u16,
        terminal: Arc<RwLock<TerminalState>>,
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

        // Use posix_spawnp instead of fork+exec (safe in multi-threaded context)
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
        let shell_c = std::ffi::CString::new(shell.as_str())?;
        let arg0 = std::ffi::CString::new("-kova")?;
        let argv: [*mut libc::c_char; 2] = [arg0.as_ptr() as *mut _, std::ptr::null_mut()];

        // Build full environment with TERM override
        let mut env_strs: Vec<std::ffi::CString> = std::env::vars()
            .filter(|(k, _)| k != "TERM")
            .map(|(k, v)| std::ffi::CString::new(format!("{k}={v}")).unwrap())
            .collect();
        env_strs.push(std::ffi::CString::new("TERM=xterm-256color").unwrap());
        let mut envp: Vec<*mut libc::c_char> = env_strs.iter().map(|s| s.as_ptr() as *mut _).collect();
        envp.push(std::ptr::null_mut());

        // File actions: dup2 slave_fd to stdin/stdout/stderr, close slave_fd
        let mut file_actions: libc::posix_spawn_file_actions_t = unsafe { std::mem::zeroed() };
        unsafe {
            libc::posix_spawn_file_actions_init(&mut file_actions);
            libc::posix_spawn_file_actions_adddup2(&mut file_actions, slave_fd.as_raw_fd(), 0);
            libc::posix_spawn_file_actions_adddup2(&mut file_actions, slave_fd.as_raw_fd(), 1);
            libc::posix_spawn_file_actions_adddup2(&mut file_actions, slave_fd.as_raw_fd(), 2);
            if slave_fd.as_raw_fd() > 2 {
                libc::posix_spawn_file_actions_addclose(&mut file_actions, slave_fd.as_raw_fd());
            }
        }

        // Spawn attributes: new session (POSIX_SPAWN_SETSID on macOS)
        let mut spawnattr: libc::posix_spawnattr_t = unsafe { std::mem::zeroed() };
        unsafe {
            libc::posix_spawnattr_init(&mut spawnattr);
            // POSIX_SPAWN_SETSID = 0x0400 on macOS (not in libc crate yet)
            libc::posix_spawnattr_setflags(&mut spawnattr, 0x0400i16);
        }

        let mut child_pid: libc::pid_t = 0;
        let ret = unsafe {
            libc::posix_spawnp(
                &mut child_pid,
                shell_c.as_ptr(),
                &file_actions,
                &spawnattr,
                argv.as_ptr(),
                envp.as_ptr(),
            )
        };

        unsafe {
            libc::posix_spawn_file_actions_destroy(&mut file_actions);
            libc::posix_spawnattr_destroy(&mut spawnattr);
        }

        if ret != 0 {
            return Err(format!("posix_spawnp failed: {}", std::io::Error::from_raw_os_error(ret)).into());
        }

        drop(slave_fd);

        let child_pid = child_pid as u32;
        PTY_PIDS.lock().push(child_pid);

        let dup_fd = unsafe { libc::dup(master_fd.as_raw_fd()) };
        if dup_fd < 0 {
            return Err("dup() failed".into());
        }
        let reader_fd = unsafe { OwnedFd::from_raw_fd(dup_fd) };

        std::thread::Builder::new()
            .name("pty-reader".into())
            .spawn(move || {
                let mut file = unsafe { std::fs::File::from_raw_fd(reader_fd.into_raw_fd()) };
                let mut parser = vte::Parser::new();
                let mut handler = VteHandler::new(terminal);
                let mut buf = [0u8; 4096];

                loop {
                    if SHUTDOWN.load(Ordering::Relaxed) {
                        break;
                    }
                    match file.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            parser.advance(&mut handler, &buf[..n]);
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
                log::info!("PTY reader thread exiting");
            })?;

        Ok(Pty {
            master_fd,
            child_pid,
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
        unsafe {
            libc::kill(self.child_pid as i32, libc::SIGWINCH);
        }
    }
}

pub fn shutdown_all() {
    SHUTDOWN.store(true, Ordering::Relaxed);
    let pids = PTY_PIDS.lock().clone();
    for pid in pids {
        unsafe {
            libc::kill(pid as i32, libc::SIGHUP);
        }
    }
    // Give children a moment to exit, then reap
    std::thread::sleep(std::time::Duration::from_millis(50));
    for pid in PTY_PIDS.lock().iter() {
        unsafe {
            libc::waitpid(*pid as i32, std::ptr::null_mut(), libc::WNOHANG);
        }
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        unsafe {
            libc::kill(self.child_pid as i32, libc::SIGHUP);
            libc::waitpid(self.child_pid as i32, std::ptr::null_mut(), 0);
        }
        PTY_PIDS.lock().retain(|&pid| pid != self.child_pid);
        log::info!("PTY child {} cleaned up", self.child_pid);
    }
}

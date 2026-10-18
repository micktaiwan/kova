# PTY spawn: posix_spawn vs fork+exec vs Command

## Context

The child shell must have the slave PTY as its **controlling terminal** so that:
- `TIOCSWINSZ` (resize) automatically sends `SIGWINCH` to the foreground process group
- `tcgetpgrp()` returns the right pgid
- Subprocesses (Claude CLI, etc.) receive the resize signals

## Attempt 1: posix_spawn + POSIX_SPAWN_SETSID

- `posix_spawn` is safe in a multi-threaded context (no fork)
- But `POSIX_SPAWN_SETSID` (0x0400 on macOS) does not guarantee the `setsid` → file actions order
- Result: the slave PTY is never the controlling terminal, `tcgetpgrp` returns 0

## Attempt 2: raw fork + setsid + TIOCSCTTY + execve

- Guaranteed order: `setsid()` → `open(slave_path)` → `ioctl(TIOCSCTTY)` → `execve`
- Problem: after `fork()`, the Rust allocator can be in an inconsistent state (locks held)
- The shell started but exited immediately (exit code 1)
- `format!()` and other Rust allocations in the child = undefined behavior

## Solution: std::process::Command + pre_exec (like Alacritty)

Source: https://github.com/alacritty/alacritty/blob/master/alacritty_terminal/src/tty/unix.rs

- `Command::spawn()` handles fork+exec cleanly (async-signal-safe)
- The `pre_exec` closure runs in the child after fork, before exec
- In it we do: `setsid()` → `ioctl(TIOCSCTTY)` → `close(slave/master)` → reset signals
- `Command` handles stdin/stdout/stderr via `Stdio::from()` (dup of the fds)
- No need for a manual SIGWINCH: `TIOCSWINSZ` does it automatically once the controlling terminal is established

## Reference

Alacritty, WezTerm, kitty: all use fork+exec (not posix_spawn) for the same reason.

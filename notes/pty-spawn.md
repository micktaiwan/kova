# PTY spawn : posix_spawn vs fork+exec vs Command

## Contexte

Le shell child doit avoir le slave PTY comme **controlling terminal** pour que :
- `TIOCSWINSZ` (resize) envoie automatiquement `SIGWINCH` au foreground process group
- `tcgetpgrp()` retourne le bon pgid
- Les sous-processes (Claude CLI, etc.) reçoivent les signaux de resize

## Tentative 1 : posix_spawn + POSIX_SPAWN_SETSID

- `posix_spawn` est safe en contexte multi-thread (pas de fork)
- Mais `POSIX_SPAWN_SETSID` (0x0400 sur macOS) ne garantit pas l'ordre `setsid` → file actions
- Résultat : le slave PTY n'est jamais le controlling terminal, `tcgetpgrp` retourne 0

## Tentative 2 : fork brut + setsid + TIOCSCTTY + execve

- Ordre garanti : `setsid()` → `open(slave_path)` → `ioctl(TIOCSCTTY)` → `execve`
- Problème : après `fork()`, l'allocateur Rust peut être dans un état incohérent (locks held)
- Le shell démarrait mais quittait immédiatement (exit code 1)
- `format!()` et autres allocations Rust dans le child = undefined behavior

## Solution : std::process::Command + pre_exec (comme Alacritty)

Source : https://github.com/alacritty/alacritty/blob/master/alacritty_terminal/src/tty/unix.rs

- `Command::spawn()` gère fork+exec proprement (async-signal-safe)
- `pre_exec` closure s'exécute dans le child après fork, avant exec
- On y fait : `setsid()` → `ioctl(TIOCSCTTY)` → `close(slave/master)` → reset signals
- `Command` gère stdin/stdout/stderr via `Stdio::from()` (dup des fd)
- Pas besoin de SIGWINCH manuel : `TIOCSWINSZ` le fait automatiquement quand le controlling terminal est établi

## Référence

Alacritty, WezTerm, kitty : tous utilisent fork+exec (pas posix_spawn) pour la même raison.

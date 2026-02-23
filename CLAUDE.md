# Kova

Terminal Mac ultra-rapide en Rust + Metal.

## Stack

- **Rust** — langage principal
- **Metal** — rendu GPU natif macOS
- **AppKit** — fenêtre et events (via `objc2`)
- **CoreText** — glyph shaping
- **`vte`** — parsing séquences VT

## Architecture

- Un arbre binaire de splits par tab
- Un PTY par terminal pane
- Atlas de glyphes sur GPU

## État

- **V0 terminée** — single pane fonctionnel (PTY, rendu Metal, scrollback, alternate screen, dirty flag, reflow au resize)
- **V1 en cours** — voir `roadmap.md` pour le détail et l'ordre recommandé

## Notes techniques

- `notes/pty-spawn.md` — pourquoi `Command + pre_exec` plutôt que `posix_spawn` ou `fork` brut pour le controlling terminal

## Principes

- Mac-only, pas de cross-platform
- Performance et RAM minimale avant tout
- Pas de feature creep : tabs, splits, config, c'est tout

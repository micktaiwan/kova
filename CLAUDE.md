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

## Principes

- Mac-only, pas de cross-platform
- Performance et RAM minimale avant tout
- Pas de feature creep : tabs, splits, config, c'est tout

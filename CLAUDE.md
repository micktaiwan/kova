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

Voir `roadmap.md` pour le détail des versions et l'avancement.

## Build

- Le target directory Cargo est **global** : `~/.cargo/target` (pas `./target`)
- Le binaire release se trouve donc dans `~/.cargo/target/release/kova`
- **Build** : **toujours** utiliser `./build.sh`, même pour vérifier que le code compile. Ne jamais lancer `cargo build` directement — le binaire ne serait pas copié dans le bundle `/Applications/Kova.app` et l'app ne serait pas mise à jour.
- `build.sh` fait : cargo build → copie du binaire + Info.plist dans le bundle → codesign du bundle entier (nécessaire pour que macOS conserve les permissions TCC entre les builds).

## Installation

```bash
mkdir -p /Applications/Kova.app/Contents/MacOS /Applications/Kova.app/Contents/Resources
cp Info.plist /Applications/Kova.app/Contents/
cp assets/kova.icns /Applications/Kova.app/Contents/Resources/
./build.sh
```

## Release

`/release <major|minor|patch>` — skill Claude Code qui bump la version dans Cargo.toml + Info.plist, commit avec un message basé sur le changelog, tag `vX.Y.Z`, push, et crée une GitHub release.

## Logs

`~/Library/Logs/Kova/kova.log` (level DEBUG par défaut, configurable via `RUST_LOG`).

## Notes techniques

- `notes/pty-spawn.md` — pourquoi `Command + pre_exec` plutôt que `posix_spawn` ou `fork` brut pour le controlling terminal

## Pièges récurrents

- **Bytes vs chars** — Les cellules du terminal sont indexées par colonne (1 Cell = 1 char), mais les `String` Rust sont indexées par byte. Ne JAMAIS faire `&text[i..i+n]` sur du texte issu des cellules (contient des emoji, box-drawing, etc.). Toujours travailler avec `Vec<char>` ou itérateurs de chars quand on manipule des positions de colonnes.

## Tests

Ne jamais lancer l'application (open, Kova.app, etc.) — laisser l'utilisateur tester manuellement.

## Principes

- Mac-only, pas de cross-platform
- Performance et RAM minimale avant tout
- Pas de feature creep : tabs, splits, config, c'est tout

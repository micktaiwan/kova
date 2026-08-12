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

`~/Library/Logs/Kova/kova.log` (level INFO par défaut, configurable via `RUST_LOG`, ex. `RUST_LOG=debug`).

## Notes techniques

- `notes/pty-spawn.md` — pourquoi `Command + pre_exec` plutôt que `posix_spawn` ou `fork` brut pour le controlling terminal

## Pièges récurrents

- **Bytes vs chars** — Les cellules du terminal sont indexées par colonne (1 Cell = 1 char), mais les `String` Rust sont indexées par byte. Ne JAMAIS faire `&text[i..i+n]` sur du texte issu des cellules (contient des emoji, box-drawing, etc.). Toujours travailler avec `Vec<char>` ou itérateurs de chars quand on manipule des positions de colonnes.

## Tests

- **Lancer les tests automatisés après chaque modification de code.** Dès qu'une modif touche le code Rust, exécuter `cargo test` (le target est global, pas besoin de `build.sh` pour ça) et vérifier que tout est vert avant de considérer la modif terminée. Un test rouge fait partie du diff : le corriger, ne pas le laisser de côté.
- **Écrire un TU quand on ajoute/modifie de la logique pure et testable** (formatage, calcul de layout/géométrie, hit-test, parsing, machine à états). Ajouter un `#[test]` inline qui la couvre, dans un `#[cfg(test)] mod tests` du même fichier. Ne pas chercher à tester le rendu Metal ni l'UI AppKit (effets de bord GPU/fenêtre) : isoler la logique pure et la tester elle.
- Ne jamais lancer l'application (open, Kova.app, etc.) — laisser l'utilisateur tester manuellement.

## Principes

- Mac-only, pas de cross-platform
- Performance et RAM minimale avant tout
- Pas de feature creep : tabs, splits, config, c'est tout

## Le skill `kova` est la surface publique de cet outil

`~/.claude/skills/kova/SKILL.md` — le vrai fichier est
`~/projects/perso/dotfiles/claude/skills/kova/SKILL.md`, le symlink n'est que la façon dont Claude
Code le voit — décrit comment une session, depuis n'importe où sur ce Mac, se sert de kova : les
commandes, les chemins, les ports, ce qu'elle n'a pas le droit de faire. Rien ne le synchronise
automatiquement.

**Un changement ici qui touche ce que le skill promet se répercute dans le skill dans la foulée** :
une commande ou un flag, un chemin de socket ou de données, un port, une valeur par défaut, un nom
de fichier de conf, une règle sur ce qu'une session peut toucher. Un skill périmé est pire que pas
de skill du tout, parce qu'une session agit sur ce qu'il dit. `/skill-check kova` compare les deux
et signale ce qui a divergé.

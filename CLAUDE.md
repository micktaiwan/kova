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

- **Découpe de texte en f32** — Un texte aligné à droite part de `max_x - n × cell_w`, donc sa dernière cellule finit pile sur `max_x` en arithmétique exacte, jamais en f32 : un dépassement d'un ulp faisait disparaître le dernier caractère (deux colonnes du switcher affichaient `2.1.22` et `2.1.228` pour la même chaîne). Tout test de dépassement de marge passe par `glyph_fits` (`src/renderer/mod.rs`), qui tolère un quart de pixel — ne jamais recomparer `x + cell_w > max_x` en dur.

- **Une notif de bureau qui s'affiche ne prouve pas que son clic marche** — `terminal-notifier` 2.0.0 ne parle que `NSUserNotification`, l'API dépréciée depuis 10.14 (`nm` sur son binaire ne montre que ça). Sur macOS 26 la bannière est bien livrée, mais le mécanisme qui relance l'app au clic est mort : son `-execute` ne tourne jamais, en silence. Symptôme : « cliquer la notif ne fait rien », et le diagnostic part à tort vers le socket ou la commande. Kova poste donc ses notifications lui-même (`src/notification.rs`, commande IPC `notify`) : le pane id voyage dans le `userInfo` et le clic est traité dans le process, sans helper externe. Toute action au clic passe par là — ne jamais la rebrancher sur un notifier tiers sans avoir vérifié qu'il utilise `UNUserNotificationCenter`.

- **Tout ce que Kova retape dans un PTY vient d'ailleurs et se valide avant** — la ligne pré-tapée à la restauration (`src/session.rs`, `restore_command`) est fabriquée à partir de deux sources que Kova n'écrit pas : le `sessionId` de `~/.claude/sessions/*.json`, et `last_command`, que n'importe quel programme peut poser en imprimant un OSC 7777 sur son propre tty. Un `\n` dans la première suffit à faire partir une commande sans que personne n'appuie sur Entrée. D'où deux gardes à ne pas retirer : `is_safe_session_id` (`src/claude_session.rs`) refuse tout ce qui sort de `[A-Za-z0-9_-]`, et `last_command_slot_open` (`src/terminal/`) n'accepte qu'un seul OSC 7777 par OSC 133;C, celui que le hook `preexec` du shell envoie — la sortie d'une commande qui tourne n'a plus le droit de nommer la commande suivante. Toute nouvelle source de texte injecté passe par la même question : qui peut l'écrire ? Troisième source depuis, même garde : la reprise d'une session fermée depuis la palette de recherche (`src/claude_history.rs`) prend l'id dans le **nom du fichier** de transcript, donc un nom de fichier posé par n'importe qui dans `~/.claude/projects/` — il passe par `resume_command`, qui refuse tout id hors `[A-Za-z0-9_-]`, à l'indexation comme à l'ouverture.

- **`NSApplication::windows()` liste des fenêtres qui ne sont pas les nôtres** — panneaux AppKit, porteurs de tooltip, etc. Caster leur `contentView` en `KovaView` sans vérifier lit les ivars d'une autre classe : le `Vec` de tabs obtenu portait un pointeur nul, et `Cmd+J` a segfaulté dans `Tab::for_each_pane` avec `self = 0` (crash du 2026-08-14, v1.9.0). `kova_view` (`src/app.rs`) demande maintenant `isKindOfClass` avant de caster — passer par lui, jamais par un cast direct.

## Tests

- **Lancer les tests automatisés après chaque modification de code.** Dès qu'une modif touche le code Rust, exécuter `cargo test` (le target est global, pas besoin de `build.sh` pour ça) et vérifier que tout est vert avant de considérer la modif terminée. Un test rouge fait partie du diff : le corriger, ne pas le laisser de côté.
- **Écrire un TU quand on ajoute/modifie de la logique pure et testable** (formatage, calcul de layout/géométrie, hit-test, parsing, machine à états). Ajouter un `#[test]` inline qui la couvre, dans un `#[cfg(test)] mod tests` du même fichier. Ne pas chercher à tester le rendu Metal ni l'UI AppKit (effets de bord GPU/fenêtre) : isoler la logique pure et la tester elle.
- Ne jamais lancer l'application (open, Kova.app, etc.) — laisser l'utilisateur tester manuellement.

## Principes

- Mac-only, pas de cross-platform
- Performance et RAM minimale avant tout
- Pas de feature creep : tabs, splits, config, c'est tout
- **Du code que Mickael a validé (« ça me convient », « c'est bon ») ne se modifie plus.** Si une revue trouve ensuite un écart entre le code et les specs, ce sont les specs qu'on aligne sur le code, jamais l'inverse : toucher au comportement après validation introduit des régressions sur quelque chose qu'il a déjà accepté.

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

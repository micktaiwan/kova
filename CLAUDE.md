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

- **Tout ce que Kova retape dans un PTY vient d'ailleurs et se valide avant** — la ligne pré-tapée à la restauration (`src/session.rs`, `restore_command`) est fabriquée à partir de deux sources que Kova n'écrit pas : le `sessionId` de `~/.claude/sessions/*.json`, et `last_command`, que n'importe quel programme peut poser en imprimant un OSC 7777 sur son propre tty. Un `\n` dans la première suffit à faire partir une commande sans que personne n'appuie sur Entrée. D'où deux gardes à ne pas retirer : `is_safe_session_id` (`src/claude_session.rs`) refuse tout ce qui sort de `[A-Za-z0-9_-]`, et `last_command_slot_open` (`src/terminal/`) n'accepte qu'un seul OSC 7777 par OSC 133;C, celui que le hook `preexec` du shell envoie — la sortie d'une commande qui tourne n'a plus le droit de nommer la commande suivante. Toute nouvelle source de texte injecté passe par la même question : qui peut l'écrire ? Troisième source depuis, même garde : la reprise d'une session fermée depuis la palette de recherche (`src/claude_history.rs`) prend l'id dans le **nom du fichier** de transcript, donc un nom de fichier posé par n'importe qui dans `~/.claude/projects/` — il passe par `resume_command`, qui refuse tout id hors `[A-Za-z0-9_-]`, à l'indexation comme à l'ouverture. Quatrième source, de l'autre côté de la bibliothèque : l'option `command` de `kova_pane_new` (`include/kova.h`) vient de l'app hôte (Kana) et part avec Entrée sans aucune garde de Kova — c'est l'hôte qui en répond, et un `\n` dedans lance une seconde commande. Cinquième source : le message de livraison tardive de `kova ask` (`src/ask.rs`), retapé dans le pane de l'agent qui a posé la question ou passé en argument à `claude --resume` dans un nouvel onglet. Il porte deux textes que Kova n'écrit pas : la question, envoyée par n'importe quel process qui atteint le socket, et la réponse que l'utilisateur tape sur la carte. Tout caractère de contrôle y devient une espace (`late_answer_message`, pas seulement `\n` : un `^U` effacerait la ligne), la ligne est quotée pour zsh (`resume_line`), l'id de session passe par `is_safe_session_id`, et le pane n'est visé que s'il fait tourner la conversation de l'agent (`pane_holds_session`).

- **`NSApplication::windows()` liste des fenêtres qui ne sont pas les nôtres** — panneaux AppKit, porteurs de tooltip, etc. Caster leur `contentView` en `KovaView` sans vérifier lit les ivars d'une autre classe : le `Vec` de tabs obtenu portait un pointeur nul, et `Cmd+J` a segfaulté dans `Tab::for_each_pane` avec `self = 0` (crash du 2026-08-14, v1.9.0). `kova_view` (`src/app.rs`) demande maintenant `isKindOfClass` avant de caster — passer par lui, jamais par un cast direct.

- **Un deuxième Kova démarre sans tabs, et ce n'est ni la conf ni le fichier de session** — la première instance verrouille `~/.config/kova/session.json` (`owns_session`, `src/session.rs`), les suivantes ouvrent une fenêtre vierge et ne sauvent rien. Le log le dit en clair : « Another Kova owns … : this instance starts fresh and will not save its session ». Symptôme trompeur : on relance `kova &` alors qu'un Kova tourne encore, on ne retrouve pas ses tabs, et le diagnostic part vers la lecture de la conf. Lire `~/Library/Logs/Kova/kova.log` avant de chercher ailleurs.

- **« Un pane se focus tout seul » : la ligne `IPC: focused pane N` ne prouve pas l'IPC** — elle est écrite par `ipc_focus_pane` (`src/window/ipc_handlers.rs`), que prennent aussi Cmd+P, Cmd+J, le clic sur une notif, la palette de recherche et le dialogue de doublon. Pour trouver l'auteur : regarder la ligne `performKeyEquivalent` juste avant (un Cmd+P = Mickael), puis chercher dans les transcripts `~/.claude/projects/**/*.jsonl` les `new-tab`, `close-tab`, `split` ou `focus-pane` envoyés au même moment. Le 14/10/2026, c'était une session qui testait Kova Float en ouvrant et fermant des onglets dans le Kova de Mickael. Les lignes du log ne portent que l'heure, sans la date : filtrer par numéro de ligne à partir d'un repère connu, pas par heure.

## Tests

- **Lancer les tests automatisés après chaque modification de code.** Dès qu'une modif touche le code Rust, exécuter `cargo test` (le target est global, pas besoin de `build.sh` pour ça) et vérifier que tout est vert avant de considérer la modif terminée. Un test rouge fait partie du diff : le corriger, ne pas le laisser de côté.
- **Écrire un TU quand on ajoute/modifie de la logique pure et testable** (formatage, calcul de layout/géométrie, hit-test, parsing, machine à états). Ajouter un `#[test]` inline qui la couvre, dans un `#[cfg(test)] mod tests` du même fichier. Ne pas chercher à tester le rendu Metal ni l'UI AppKit (effets de bord GPU/fenêtre) : isoler la logique pure et la tester elle.
- Ne jamais lancer l'application (open, Kova.app, etc.) — laisser l'utilisateur tester manuellement.
- **Une instance de test (le binaire de build lancé à côté du Kova de Mickael, quand il l'a autorisé) s'annonce avant de partir, par une carte** : `kova ask "…" --actions "OK,Plus tard"` (ou `kfloat ask` tant que son Kova ne connaît pas `ask`), et on attend son OK. Sa fenêtre passe devant au lancement ; vue arriver en plein travail, il la ferme. Lancer avec un environnement propre (`env -i HOME USER SHELL PATH…`) : héritées de la session Claude qui lance, les variables `CLAUDE_CODE_*` font croire aux Claude des panes qu'ils sont des sous-sessions, sans registre ni transcript. Et un `KOVA_ASK_DIR` à part, pour ne pas toucher aux questions du vrai Kova. Né le 14/10/2026 pendant le portage de `kova ask` ; ses mots : « préviens moi parce que qd je vois la fenetre arrivée alors que je travaille je la ferme, maintenant qu'on a un moyen de communiquer pendant que kova est pas focus, utilise-le ».

## Principes

- Mac-only, pas de cross-platform
- Performance et RAM minimale avant tout
- Pas de feature creep côté app : tabs, splits, config, c'est tout. Deux extensions admises : Kova sert aussi de bibliothèque de terminal à Kana (levée par Mickael le 13/10/2026, chantier décrit dans `docs/embedding.md`), et Kova affiche les questions des agents (`kova ask`, levée le 14/10/2026 : « la règle je m'en fous » ; `src/ask.rs`, `src/ask_card.rs`, `docs/ipc.md` § `ask`), portées depuis Kova Float.
- **Le comportement que Mickael a validé (« ça me convient », « c'est bon ») ne change plus ; le code, lui, peut bouger, à condition que des tests prouvent que le comportement est identique.** Ordre imposé : les tests qui figent le comportement d'abord, verts, puis le déplacement. Si une revue trouve un écart entre le code et les specs, ce sont les specs qu'on aligne sur le code, jamais l'inverse : changer un comportement validé introduit des régressions sur quelque chose qu'il a déjà accepté. (Règle reformulée le 13/10/2026 ; avant, le code validé lui-même ne se modifiait plus.)

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

# Kova — Roadmap

## Vision

Le terminal Mac le plus rapide et léger possible.
Rust + Metal, zéro compromis cross-platform.

## V0 — Preuve de concept

**Objectif** : une fenêtre qui lance un shell, affiche la sortie, accepte l'input.

- [x] Fenêtre AppKit minimale via `objc2`
- [x] Rendu texte Metal (monospace, un seul font, pas de ligatures)
- [x] Atlas de glyphes basique via CoreText
- [x] Atlas dynamique (rasterisation à la demande des caractères non-ASCII)
- [x] PTY : spawn d'un shell (zsh) via `posix_spawnp` (safe multi-thread)
- [x] Input clavier → PTY
- [x] Output PTY → écran (parsing VT via `vte`)
- [x] Scrollback basique
- [x] Ctrl+C (signal au process)
- [x] Quitter proprement (Cmd+Q, fermeture fenêtre, cleanup PTY)
- [x] Buffers Metal pré-alloués (double-buffering, pas d'alloc par frame)
- [x] Scroll trackpad (accumulateur fractionnaire)
- [x] Alternate screen buffer (CSI ?1049 h/l)
- [x] Resize fenêtre (recalcul cols/rows + SIGWINCH)
  - Passé de `posix_spawn`+`SETSID` à `fork`+`setsid`+`TIOCSCTTY` : nécessaire pour que le slave PTY soit le controlling terminal et que SIGWINCH atteigne les sous-processes (fork OK car single-threaded à ce stade)
- [x] Cmd+V (coller depuis le presse-papier)
- [x] Rendu à la demande (dirty flag) — ne redessiner que quand l'état change

**Critère de succès** : lancer `ls`, `htop`, `claude` et que ça marche.

## V1 — Utilisable au quotidien

Ordre recommandé : config d'abord (indépendant), puis sélection texte (pane unique),
puis refacto multi-pane, puis splits, puis tabs par-dessus.

### Config & fondations

- [x] Config fichier (TOML) : font, taille, couleurs, FPS, cursor blink, scrollback
  - `~/.config/kova/config.toml`, defaults sensibles, fallback silencieux
- [x] Détecter la mort du shell (EOF sur PTY) → fermer la fenêtre
- [x] Status bar (CWD via OSC 7, git branch, indicateur scroll, titre OSC 0/2, heure HH:MM, couleur par élément configurable)
- [x] Shift+Tab (backtab) — envoie `CSI Z` au lieu du raw `0x19`
- [x] Sélection texte + copier/coller (mouseDown/Dragged/Up, Cmd+C, highlight sélection, copie auto dans presse-papier, respect du soft-wrap)
- [x] Resize fenêtre : reflow du texte (struct `Row` avec flag `wrapped`, reconstruction des lignes logiques, re-wrap à la nouvelle largeur)
- [x] Restauration position fenêtre au lancement — `NSWindow.setFrameAutosaveName` (persistence automatique via `NSUserDefaults`)

### Input macOS
- [x] Option+Left/Right — déplacement mot par mot (envoie `\x1bb`/`\x1bf`)
- [x] Cmd+Backspace — effacer toute la ligne (envoie `\x15` Ctrl+U)

### Refacto multi-pane (prérequis splits)

- [x] PTY lifecycle per-pane — shutdown par PTY (Arc<AtomicBool> par instance)
- [x] Split tree (`enum SplitTree { Leaf(Pane), Hsplit(...), Vsplit(...) }`) — arbre binaire dans `pane.rs`
- [x] Modèle de focus — tracker le pane actif pour router l'input clavier
- [x] Renderer multi-pane — `render()` accepte un viewport par pane, clipping et offset

### Splits & tabs

- [x] Splits horizontaux et verticaux (arbre binaire)
- [x] Navigation entre splits (raccourcis clavier)
- [x] Séparateurs visuels entre splits (ligne 1px semi-transparente)
- [x] Padding horizontal des panes (10px)
- [x] Nouveau split hérite du CWD du pane focusé (via `proc_pidinfo`)
- [ ] Resize des splits (raccourcis + drag)
- [ ] Tabs (barre minimale en haut)
- [ ] Navigation entre tabs
- [x] Fermeture split — `exit`/Cmd+W retire le pane de l'arbre, reporte le focus, `app.terminate` seulement quand plus aucun pane

## V2 — Polished

- [x] Focus events (DEC mode 1004) — notifier le shell/app quand la fenêtre gagne/perd le focus
- [x] Kitty keyboard protocol (CSI u) — réponse à la query `CSI > 0 u` (flags=0, fallback propre)
- [ ] Config keybindings (raccourcis hardcodés suffisent pour V1)
- [x] Synchronized output (mode 2026) — bufferiser le rendu entre h/l pour éviter le tearing
- [x] CPR (Cursor Position Report, CSI 6 n) — réponse position curseur
- [x] DA1 (Device Attributes, CSI c) — identification VT220 + ANSI color
- [x] DECRPM (Report Private Mode, CSI ? Ps $ p) — report état des modes 1, 7, 25, 1004, 1049, 2004, 2026
- [x] Bracketed paste mode (DEC 2004) — wrapping `\x1b[200~`/`\x1b[201~` sur Cmd+V
- [x] DECCKM (mode 1) — cursor keys application mode (`\x1bO` vs `\x1b[`)
- [x] DECAWM (mode 7) — auto-wrap on/off, respecté dans put_char
- [x] Insert mode (SM/RM 4) — décale les caractères au lieu d'écraser
- [x] ICH (CSI @) — insertion de caractères blancs à la position curseur
- [x] DECSCUSR (CSI Ps SP q) — cursor shape block/underline/bar
- [ ] Thèmes de couleurs (quelques built-in + custom)
- [ ] Support ProMotion (120Hz)
- [ ] Recherche dans le scrollback
- [ ] Clickable URLs
- [ ] Support multi-fenêtres
- [ ] Déplacer un split (réorganiser l'arbre de splits par drag ou raccourci)
- [ ] Notifications visuelles (bell, activité dans un split inactif)
- [ ] PTY cleanup non-bloquant — remplacer le `waitpid` bloquant dans `Drop for Pty` par une escalade SIGHUP → SIGTERM → SIGKILL avec timeouts (~200ms max), pour éviter un freeze UI si un process ignore SIGHUP
- [ ] Font fallback (emoji, symboles) — CoreText fallback fonctionne mais block elements/box-drawing nécessitent un rendu custom (voir `notes/font-fallback-investigation.md`)
- [ ] Ligatures (optionnel)

## V3 — Avancé

- [ ] Support images (Sixel ou protocole Kitty)
- [ ] Shell integration (marks, navigation prompt à prompt)
- [ ] Complétion inline / suggestions
- [ ] Metriques perf exposées (frame time, mémoire)

## Non-goals

- Cross-platform (macOS uniquement)
- Plugin system
- Protocoles custom propriétaires
- Multiplexer réseau (ssh tunneling etc.)
- Built-in AI (Claude tourne dans le terminal, pas besoin)

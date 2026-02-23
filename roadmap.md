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
- [ ] Config keybindings
- [ ] Sélection texte + copier/coller (sur le pane unique actuel)
- [ ] Resize fenêtre : reflow du texte

### Refacto multi-pane (prérequis splits)

- [ ] PTY lifecycle per-pane — remplacer les singletons globaux (`static SHUTDOWN`, `static PTY_PIDS`) par un shutdown par PTY (Arc<AtomicBool> par instance)
- [ ] Split tree (`enum SplitTree { Leaf(Pane), Hsplit(...), Vsplit(...) }`) — remplacer le terminal/pty/renderer uniques dans KovaView ivars
- [ ] Modèle de focus — tracker le pane actif pour router l'input clavier
- [ ] Renderer multi-pane — `render()` accepte un `&SplitTree`, clipping et offset par pane

### Splits & tabs

- [ ] Splits horizontaux et verticaux (arbre binaire)
- [ ] Navigation entre splits (raccourcis clavier)
- [ ] Resize des splits (raccourcis + drag)
- [ ] Tabs (barre minimale en haut)
- [ ] Navigation entre tabs
- [ ] Fermeture tab/split — actuellement `exit` ferme toute l'app (`app.terminate`), à remplacer par fermeture du pane seul

## V2 — Polished

- [x] Focus events (DEC mode 1004) — notifier le shell/app quand la fenêtre gagne/perd le focus
- [x] Kitty keyboard protocol (CSI u) — réponse à la query `CSI > 0 u` (flags=0, fallback propre)
- [ ] Synchronized output (mode 2026) — bufferiser le rendu entre h/l pour éviter le tearing
- [ ] Thèmes de couleurs (quelques built-in + custom)
- [ ] Support ProMotion (120Hz)
- [ ] Recherche dans le scrollback
- [ ] Clickable URLs
- [ ] Support multi-fenêtres
- [ ] Notifications visuelles (bell, activité dans un split inactif)
- [ ] Font fallback (emoji, symboles)
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

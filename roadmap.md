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
- [x] Git branch polling — re-lecture de `.git/HEAD` toutes les ~2s pour détecter les changements de branche sans attendre un changement de CWD
- [x] Shift+Tab (backtab) — envoie `CSI Z` au lieu du raw `0x19`
- [x] Sélection texte + copier/coller (mouseDown/Dragged/Up, Cmd+C, highlight sélection, copie auto dans presse-papier, respect du soft-wrap)
- [x] Resize fenêtre : reflow du texte (struct `Row` avec flag `wrapped`, reconstruction des lignes logiques, re-wrap à la nouvelle largeur)
- [x] Restauration position fenêtre au lancement — `NSWindow.setFrameAutosaveName` (persistence automatique via `NSUserDefaults`)

### Input macOS
- [x] Option+Left/Right — déplacement mot par mot (envoie `\x1bb`/`\x1bf`)
- [x] Cmd+Backspace — effacer toute la ligne (envoie `\x15` Ctrl+U)
- [x] Cmd+Left/Right — début/fin de ligne (envoie Home `\x1b[H` / End `\x1b[F`)
- [x] Option key — envoie le caractère composé macOS quand différent du caractère de base

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
- [x] Resize des splits (Cmd+Ctrl+arrows + drag souris sur séparateurs, clamp 0.1–0.9)
- [x] Égalisation automatique des splits — après ajout/suppression d'un pane, tous les panes d'un même axe sont redistribués à taille égale (1/N chacun)
- [x] Tabs (barre minimale en haut, Cmd+T nouveau tab, Cmd+W ferme pane/tab, rendu Metal, tab bar cliquable)
- [x] Navigation entre tabs (Cmd+Shift+[/], Cmd+1..9)
- [x] Drag & reorder des tabs (drag souris avec seuil 3px, swap temps réel)
- [x] Renommage de tab (Cmd+Shift+R, nom custom prioritaire, vider pour revenir au nom auto)
- [x] Fermeture split — `exit`/Cmd+W retire le pane de l'arbre, reporte le focus, `app.terminate` seulement quand plus aucun pane

## V2 — Polished

- [x] Focus events (DEC mode 1004) — notifier le shell/app quand la fenêtre gagne/perd le focus
- [x] Kitty keyboard protocol (CSI u) — réponse à la query `CSI > 0 u` (flags=0, fallback propre)
- [x] Save/restore session layout — sauvegarde arbre de tabs/splits et CWD au quit, restauration au lancement
- [x] File logging — écriture des logs dans un fichier pour debug
- [x] Tab bar redesign — couleurs de tabs, refonte visuelle
- [x] Navigation cross-tab (Cmd+Shift+Arrows entre splits de différents tabs)
- [x] Lazy write lock dans le parser VTE — acquisition du write lock uniquement quand nécessaire, réduit la contention
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
- [x] Recherche dans le scrollback (Cmd+F — filtre overlay, highlight query, click pour scroller)
- [x] App icon dans Info.plist (`CFBundleIconFile`) — corrige l'icône surdimensionnée dans Alt-Tab
- [x] Clickable URLs (Cmd+hover souligne en bleu + curseur main + URL en status bar, Cmd+click ouvre dans le navigateur)
- [x] Wide characters (emojis, CJK) — détection via `unicode-width`, placeholder `'\0'` en col+1, rasterisation 2× cell_width dans l'atlas
- [x] Déplacer un split par raccourci (Cmd+Shift+Arrows — swap le pane focusé avec son voisin)
- [x] Bell indicator sur tabs inactifs (point orange sur les tabs non focusés quand bell reçu)
- [x] Horizontal scroll splits — quand les splits dépassent la largeur écran, scroll horizontal trackpad + auto-reveal du pane focusé. `min_split_width` configurable.
- [x] Color emoji rendering via CoreText fallback fonts
- [x] Grapheme cluster emoji (flags, ZWJ sequences, skin tones)
- [ ] Emoji presentation fallback — les caractères avec `Emoji_Presentation` par défaut (U+2B1C, U+2B1B, U+25AA…) utilisent le glyphe de la font mono (petit carré géométrique) au lieu de la version couleur Apple Color Emoji, car `resolve_glyph` retourne le glyphe primaire sans vérifier la présentation attendue. Fix : forcer le fallback emoji pour les codepoints Unicode `Emoji_Presentation=Yes`.
- [x] Optimisation RAM Cell — compact cell storage pour le scrollback (28→12 bytes/cell, -57% RAM). fg/bg stockés en `u32` RGBA au lieu de `[f32; 3]`.
- [x] Multi-fenêtres — Cmd+N nouvelle fenêtre, Cmd+Q ferme fenêtre active, Cmd+Option+Q kill sans save, Cmd+Shift+T detach tab vers nouvelle fenêtre, Cmd+Ctrl+T break pane vers nouvel onglet. Session restore multi-window. Dealloc différé pour éviter segfault AppKit.
- [x] Config keybindings (raccourcis configurables via `[keys]` dans config.toml)
- [ ] Déplacer un split par drag (anchor visuelle pendant le drag — le swap par raccourci Cmd+Shift+Arrows existe déjà)
- [x] Notifications visuelles avancées (activité dans un split inactif)
- [ ] Batching du parser VT — le pty-reader tient le write lock sur `TerminalState` pendant tout `parser.advance()` d'un chunk 4 Ko. Quand un pane en background reçoit beaucoup de données (build, logs…), le write lock bloque les read locks du renderer (parking_lot donne priorité aux writers). Solution : op-buffer local (`Vec<TermOp>`) parsé sans lock, flush en un seul write lock. Voir `notes/vt-parser-batching.md`.
- [x] PTY cleanup sur thread dédié — `Drop for Pty` délègue l'escalade SIGHUP → SIGTERM → SIGKILL à un thread détaché (`pty-reaper-{pid}`), zéro sleep sur le main thread. `shutdown_all()` fait la même escalade en synchrone avec timeouts réduits (25ms/étape)
- [ ] Font fallback (block elements/box-drawing) — nécessitent un rendu custom (voir `notes/font-fallback-investigation.md`)
- [ ] **Tab bar font size** : taille de fonte des tabs configurable indépendamment (`tab_bar.font_size`), override possible par fenêtre. Voir `notes/tab-font-size.md`.
- [x] **Trim trailing blanks** : tronquer les cellules vides en fin de ligne dans le scrollback (`shrink_to_fit`), re-expand au resize. ~50-70% de réduction RAM scrollback.
- [ ] **Run-length encoding** : compresser les séquences de même couleur. (Gain marginal après trim, complexité élevée — déprioritisé.)
- [x] Metriques perf exposées (mémoire, allocations) — overlay Cmd+Shift+I avec RSS, détail terminal/renderer/pane. Frame time non inclus.
- [x] Double-clic sur un mot → sélectionne le mot entier
- [ ] Minimisation de pane — réduire un pane à une barre 24px affichant titre/CWD. Le pane reste dans le split tree, le sibling récupère l'espace. PTY continue en background, bell/activité visibles sur la barre. Cmd+M minimise le pane focusé, Cmd+Opt+M restaure le dernier minimisé (FILO), click sur la barre restaure ce pane spécifique.
- [ ] Cmd+V dans le champ de recherche (Cmd+F) — le paste ne fonctionne pas actuellement dans l'overlay de recherche
- [x] Flèches dans le renommage de tab/pane — les flèches gauche/droite naviguent dans le texte, curseur positionnable, backspace/insertion au curseur

## V3 — Avancé

- [ ] Support images inline (Kitty Graphics Protocol) — affichage d'images dans le terminal (`icat`, `yazi`, etc.). Parser APC, image store, texture manager Metal, draw calls séparés. Voir [`docs/image-support.md`](docs/image-support.md)
- [ ] Shell integration (marks, navigation prompt à prompt)
- [ ] Complétion inline / suggestions

## Nice to have

Items intéressants mais non prioritaires — le gain ne justifie pas l'effort à court terme.

- [ ] Support ProMotion (120Hz) — le dirty flag fait déjà que le rendu est skip quand rien ne change, donc le surcoût est limité au scroll/grosses sorties. Mais la différence 60→120 Hz est marginale pour un terminal (texte statique 99% du temps).
- [ ] Thèmes de couleurs — les couleurs sont déjà configurables individuellement dans `config.toml`. Les thèmes ajouteraient un niveau d'abstraction (`theme = "catppuccin-mocha"`) pour switcher toute la palette d'un coup (16 ANSI + fg/bg/cursor/sélection). Pratique mais pas bloquant : l'utilisateur peut déjà copier-coller un bloc de couleurs dans son config.
- [ ] Ligatures — complexe (shaping CoreText par groupes de glyphes vs 1 cell = 1 glyph actuel)

## Non-goals

- Cross-platform (macOS uniquement)
- Plugin system
- Protocoles custom propriétaires
- Multiplexer réseau (ssh tunneling etc.)
- Built-in AI (Claude tourne dans le terminal, pas besoin)

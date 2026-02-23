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

**Critère de succès** : lancer `ls`, `htop`, `claude` et que ça marche.

## V1 — Utilisable au quotidien

- [ ] Splits horizontaux et verticaux (arbre binaire)
- [ ] Navigation entre splits (raccourcis clavier)
- [ ] Resize des splits (raccourcis + drag)
- [ ] Tabs (barre minimale en haut)
- [ ] Navigation entre tabs
- [ ] Fermeture tab/split
- [ ] Config fichier (TOML) : font, taille, couleurs, keybindings
- [ ] Sélection texte + copier/coller
- [ ] Resize fenêtre (reflow du texte)
- [ ] Rendu à la demande (dirty flag) — ne redessiner que quand l'état change

## V2 — Polished

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

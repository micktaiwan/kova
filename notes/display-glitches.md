# Display glitches — investigation et corrections (v1.8.0)

Symptôme d'origine : avec Claude Code dans Kova, lignes noires et grands espaces
entre deux lignes, persistants jusqu'à Cmd+R. ~50 bugs corrigés en 4 itérations
de revue (multi-agents + vérification adversariale, 41 tests de régression).

## Raisonnement clé

Le renderer reconstruit tous les vertices depuis la grille à chaque frame
rendue, et un rebuild complet est forcé au moins toutes les 2 s (tick RSS).
Donc un glitch **persistant** = grille de cellules corrompue, pas une frame
perdue. Cmd+R répare parce qu'il force l'app à réécrire toutes les cellules
(soft-reset + double SIGWINCH). Les TUI à rendu différentiel (Claude Code) ne
réécrivent que les lignes qu'ils croient modifiées : toute dérive d'une ligne
côté terminal persiste indéfiniment.

## Causes racines du glitch

1. **Pending wrap (xterm "Last Column Flag")** — après un caractère en
   dernière colonne, `cursor_x` valait `cols` (hors grille). Un mouvement
   vertical (CUU/CUD) suivi d'un print déclenchait un wrap parasite : texte
   une ligne plus bas que prévu, scroll parasite possible de l'écran entier
   en bas de pane → tout le rendu différentiel décalé en permanence.
   Fix : flag `pending_wrap` explicite, sémantique xterm/DEC STD 070
   (effacé par CR/LF/BS/CUP/CUU…, PAS par HT/CBT ; sauvé par DECSC).

2. **Synchronized output (DEC 2026) non respecté au dessin** — le defer ne
   tenait que 16 ms et le pane était dessiné quand même depuis la grille en
   cours de mise à jour (lignes effacées pas encore réécrites = lignes
   noires). Fix : cache de vertices par pane (la frame précédente cohérente
   reste affichée pendant la rafale, fenêtre 150 ms), avec flags `mid_sync`
   / `ready` pour ne jamais geler une frame déchirée, invalidation sur
   changement de viewport et de génération d'atlas.

3. **Gravité `y_offset_rows`** (contenu poussé en bas quand l'écran n'est pas
   plein) se déclenchait pour les TUI en plein redraw → grands espaces.
   Fix : gravité limitée au flux shell (curseur sur/après la dernière ligne
   de contenu), cellules à fond coloré comptées comme contenu.

## Autres familles corrigées

- **Reflow unifié** : scrollback + grille reflowés comme un seul flux logique
  (les lignes wrappées à cheval sur la frontière restaient coupées à vie).
  Pièges du reflow : ne jamais couper une paire wide (`base` + `\0`), les
  trims doivent garder les `\0` et les fonds colorés, la relocalisation du
  curseur doit rejouer le chunking pair-aware (les pads insérés aux
  frontières décalent les offsets bruts), pop des lignes vides du bas AVANT
  le split (sinon du contenu visible part en scrollback).
- **Glyph atlas** : le packer shelf avançait `next_y` de la hauteur du
  nouveau glyphe au lieu de la hauteur réelle du shelf (bas des glyphes
  hauts écrasé en texture, en permanence). Compteur `generation` + retry du
  build quand l'atlas change en milieu de frame (UVs normalisés invalidés
  par la croissance), y compris quand c'est un overlay qui le fait grossir.
- **ED 3 (CSI 3J)** était aliasé sur ED 2 : le scrollback n'était jamais
  vidé (le `/clear` de Claude Code émet 2J+3J).
- **SGR sous-paramètres `:`** (ITU) : l'aplatissement vte corrompait les
  attributs — `4:0` isolé devenait un SGR 0 (reset complet, vu avec neovim).
- **BCE** (back-color erase) partout, cohérent avec reverse/bold/dim.
- **Charset graphique DEC** (`ESC ( 0` + SO/SI + table ACS) : les bordures
  ncurses rendaient `qqqxxx` au lieu de lignes. État sauvé par DECSC.
- **Combining marks / ZWJ coupés par les chunks PTY de 4 Ko** : fusion dans
  la cellule précédente, holdback borné à un chunk de grâce, recalcul de
  largeur sur promotion VS16.
- **Resize** : DECSC/DECRC clampés et complets (SGR + flags + charset),
  resize en alt-screen (la grille principale sauvegardée perdait le prompt),
  détection de round-trip A→B→A (SIGWINCH coalescés → nudge de repaint),
  tab stops réels (HTS/TBC/CBT), DECOM, marges CUU/CUD, REP (CSI b),
  DA2/XTVERSION, modes 47/1047/1048.

## Garde-fous

- 41 tests dans `src/terminal/mod.rs` et `src/terminal/parser.rs` (ce
  dernier pilote le vrai parser vte avec des bytes bruts, chunks séparés).
- Les repros des bugs de reflow sont encodés en tests — toute régression du
  chunking ou de la relocalisation du curseur casse la suite.

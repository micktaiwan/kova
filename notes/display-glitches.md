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

---

# Round 2 — « le trou revient » (juin 2026) : c'est l'alt-screen, PAS le scrollback

Symptôme rapporté (2026-06-17, Mickael, sur Kova **v1.8.0**) : bande blanche
(« trou ») au milieu du texte dans une pane Claude Code. Cmd+R répare. En
re-scrollant (haut puis bas), le **même** trou réapparaît au même endroit.

## Méthode

Revue de code multi-agents (7 reviewers + vérification adversariale) PUIS
vérité-terrain par dump IPC (`get-pane-content`) des panes Claude Code en
live. **La vérité-terrain a invalidé une partie des conclusions de la revue.**

## Faits établis (VÉRIFIÉS)

1. **Claude Code tourne en écran alterné.** `count-pane-content mode=scrollback`
   renvoie **0 octet** sur les 6 panes Claude testées (98, 100, 113, 135, 182,
   194). Donc : aucun scrollback Kova, `scroll_offset` reste 0, `visible_lines()`
   == la grille, et `y_offset_rows()` renvoie 0 (early-return alt-screen,
   `mod.rs:2330`). → Tout le pan « scrollback » des analyses précédentes
   (push_to_scrollback, stitch visible_lines+offset, gravité) est **hors-sujet**
   pour ce bug.

2. **Le trou est une bande de N rangées vides dans la grille alt.** Dump de la
   pane 194 (rows=65, cols=85, curseur {row:60,col:2}) au moment du trou :
   lignes **33–41 vides (9 rangées consécutives)**, juste après un bloc
   `⏺ Update(~/.claude/skills/emails-filter/SKILL.md)` contenant des **lignes
   soft-wrappées** (continuations aux lignes 22-23, 25-26, 27-28), et juste avant
   `⏺ Capitalisé …`. Snapshot : `/tmp/kova_hole_194.json`.

3. **Mécanisme du scroll.** Claude active le mouse reporting SGR (mode ≥1000 +
   1006). Au scroll, Kova forward des événements molette (`button 64/65`) au PTY
   (`window.rs:919-930`) ; Claude redessine sa grille alt **en différentiel**.
   Kova parse ce redraw. Le trou = cellules que Kova ne met pas à jour pendant ce
   redraw.

4. **Cmd+R = RepaintPane** (`window.rs:1582`) : `soft_reset` (réinit région de
   scroll/SGR/curseur, `mod.rs:1470`) + nudge winsize rows±1 → double SIGWINCH →
   Claude repeint TOUT → répare la grille. Le trou revient au scroll suivant car
   le repaint différentiel re-déclenche le bug.

## Innocenté (revue + lecture du HEAD actuel)

- **Renderer / cache de vertices** : reconstruit depuis la grille à chaque frame
  dirty ; `reuse_cached` borné à la fenêtre sync DEC 2026 (≤150 ms) + viewport
  (`renderer/mod.rs:443-453`, `605-623`). (Corriger la note Round 1 : il n'y a
  PAS de « rebuild forcé toutes les 2 s ».)
- **Gravité `y_offset_rows`** : renvoie 0 en alt-screen (`mod.rs:2330`).
- **Bleed cross-tab** : `pane_id` globalement uniques, cache scopé au tab actif.
- **Flag `wrapped` / reflow** : le renderer ne consulte jamais `wrapped` au
  dessin ; reflow ne tourne que sur changement de colonnes (jamais via Cmd+R).
- **`reverse_index` (1930), `scroll_down` (961), `scroll_up` (939), IL (1281) /
  DL (1307), `set_scroll_region` (1376)** : conformes VT, pas d'off-by-one à la
  lecture.

## Pas encore élucidé (la CAUSE)

La séquence VT exacte que Kova rate pendant le repaint différentiel de scroll.
Forte présomption : **désync de hauteur** entre ce que Claude croit avoir écrit
et ce que Kova place, autour d'un bloc soft-wrappé → piste **wrap / wide-char /
largeur**, ou une séquence région+repaint. Non prouvable par lecture seule
(une autre session a fini par ajouter des sondes `probe_*` **non-commitées** dans
`mod.rs`, sur une hypothèse « scrollback » que la vérité-terrain écarte).

## Déjà corrigé ? NON (VÉRIFIÉ)

- Process Kova qui tourne : démarré **2026-06-11 07:18** → binaire = **v1.8.0**
  (`afd6e9a`), aucun commit entre v1.8.0 et 07:18.
- Seul commit terminal post-v1.8.0 : `48625d1` (search/URL wide-char, clamp SGR,
  invalidation sélection, cap OSC8) — **ne touche pas** scroll/RI/région/wrap.
- → Le bug est présent dans le HEAD actuel (`0e30761`) aussi : un rebuild ne le
  corrige pas.

## Reprise (plan)

1. Capturer le flux PTY d'un repro : `script -q /tmp/kova_cap.raw claude` dans une
   pane neuve, générer des blocs hauts, scroller pour faire apparaître le trou.
2. Rejouer le fichier via le harness `drive(cols, rows, chunks)` (`parser.rs:913`)
   à 85×65 (ou dims réelles), reproduire la bande hors de Kova.
3. Bissecter le flux jusqu'à la séquence fautive ; chunker à la taille de lecture
   PTY si le bug dépend des frontières de chunk.
4. Test de régression (`parser.rs`) + fix.
- Alternative si non reproductible en session neuve : hook de capture des bytes
  bruts dans `pty.rs` (gated par env var), build via `build.sh`, lancé en 2ᵉ
  instance pour ne pas tuer les sessions en cours.

### Capture ON par défaut (juin 2026)

Le bug étant **non reproductible à la demande**, la capture doit déjà tourner
quand le trou survient. Donc capture **activée par défaut** (`pty.rs`) :

- Désactivable avec `KOVA_PTY_CAPTURE=0` (ou `off`/`false`/`no`).
- Fichier `~/Library/Logs/Kova/pty-capture-{pane_id}.raw`, **tronqué à
  l'ouverture** (les `pane_id` repartent à 1 à chaque lancement → append
  splicerait des sessions sans rapport).
- Plafond **256 MiB par pane** (stop + warn au-delà) pour borner le disque.
- **Purge au démarrage** des captures de plus de 24 h (`prune_old_captures`
  dans `main.rs`).

Workflow quand le trou apparaît : noter le `pane_id` de la pane fautive (sans
Cmd+R d'abord — ou peu importe, le `.raw` contient déjà les octets d'avant),
récupérer le `.raw` correspondant, le rejouer dans `drive()` pour reproduire la
bande hors-Kova, puis bissecter.

Snapshots de cette session : `/tmp/kova_hole_194.json` (grille avec trou),
`/tmp/kova_dump_194_visible.json`, `/tmp/kova_dump_194_all.json`.

---

# Round 3 (juillet 2026) : le trou est MULTI-CAUSAL — séquences VT ignorées + pending-wrap

Méthode : scan IPC des 54 panes live → **4 panes avec le trou en cours** (13, 45,
97, 100 ; scrollback 0 = alt-screen confirmé). Signature reconfirmée : bande vide
juste après un **listing numéroté soft-wrappé** (souvent avec glyphes spéciaux
`⏺ ⎿ ⚠️`), juste avant un bloc `⏺`. Puis étude multi-agents (7 cartographes →
9 hypothèses → 11 sondes de repro en worktrees avec vrais `cargo test` →
vérification adversariale, chaque divergence rejouée + validée contre les sources
xterm). **Le trou a été reproduit hors-app par plusieurs chemins indépendants** —
d'où l'échec des rounds précédents à le « corriger » d'un coup.

## Causes racines corrigées (chacune avec test de régression dans `parser.rs`)

1. **ESC D (IND) et ESC E (NEL) silencieusement ignorés** (`esc_dispatch`,
   `parser.rs`). Aucun arm `(b'D',[])`/`(b'E',[])` → un scroll perdu par usage.
   Émis par de vrais terminfo (tmux-256color `nel=\EE`). Un seul décale tout le
   rendu différentiel en dessous → bande jamais repeinte. Reproduit à 85×65 alt.
   Fix : IND → `Newline` ; NEL → `CarriageReturn` + `Newline`.
2. **Aplatissement des sous-paramètres `:` corrompt les CSI non-SGR**
   (`csi_dispatch`, `parser.rs:~609`). Le `flat_map` fusionnait les subparams ITU
   dans la liste positionnelle → `CSI 3:99;5H` mettait la colonne à 99, et
   `CSI ?7:25l` togglait un mode 25 fantôme (curseur caché). Fix : une valeur par
   groupe (`first`), `raw_groups` inchangé pour l'arm `'m'`. (Cousin du bug SGR
   `4:0` du round 1, mais côté positions.)
3. **VPR (CSI e), HPR (CSI a), HPA (CSI \`) ignorés** → repaints positionnés qui
   s'empilent sur une seule ligne, laissant des rangées jamais peintes. Fix :
   fusionnés dans les arms équivalents (VPR→CursorDown, HPR→CUF, HPA→CHA).
4. **ICH (CSI @) / DCH (CSI P) n'annulaient pas `pending_wrap`** (`insert_chars`/
   `delete_chars`, `mod.rs`), alors que `erase_chars` le fait. Une édition de fin
   sur une ligne pile pleine → wrap parasite ; **en bas d'écran ça scrolle toute
   la grille alt d'un cran**, non modélisé par l'app → bande persistante. Fix :
   `self.pending_wrap = false;` en tête des deux fonctions (conforme xterm
   `ResetWrap`). C'est la cause « hole-capable » la plus directe.

Suite : 80 tests verts (71 + 9 régressions). Les 9 nouveaux tests ont été
vérifiés rouges sur le code d'avant-fix (revert temporaire ciblé).

## Outillage durci (même session)

- **Capture PTY qui ne meurt plus** : le cap 256 MiB arrêtait la capture
  définitivement, or les panes vivent des jours → au moment du trou la capture
  était éteinte. Remplacé par une **rotation 2×128 MiB** (`pty.rs`) : capture
  continue, disque borné. Replay = `.raw.1` puis `.raw`.
- **Chemins scopés par PID d'instance** : `pty-capture-{pid}-{pane}.raw`. Avant,
  le nom ne dépendait que du `pane_id` (repart à 1 par lancement) → une 2ᵉ
  instance Kova tronquait les captures de l'instance vivante. `prune_old_captures`
  et `is_capture_file` (`main.rs`) reconnaissent les nouveaux noms + le legacy.
- **Outil de replay** : `parser.rs`, test `#[ignore]` `replay_capture_file`.
  `KOVA_REPLAY_FILE=<.raw> KOVA_REPLAY_COLS=85 KOVA_REPLAY_ROWS=65 cargo test
  replay_capture_file -- --ignored --nocapture` → grille finale + curseur +
  détection auto des bandes vides intérieures. C'est le bisecteur du plan ci-dessus.
- **`kova --version`/`--help`** (`main.rs`) affichent et sortent au lieu de lancer
  l'app complète (qui restaurait/supprimait `session.json` et tronquait les
  captures d'une instance concurrente — incident rencontré pendant la session).

## Reste ouvert (non prouvé cause du trou observé, mais suspects hole-capable)

- **DECSTBM param 0 / région invalide** : `CSI 5;0r` traité comme plein écran au
  lieu de « défaut = dernière ligne » ; région `top>=bottom` reset + home curseur.
  Reproduit en test mais fix non appliqué (plus invasif — à faire avec sa géométrie
  de repro dédiée).
- **Frontière de chunk PTY au milieu d'un graphème** (émoji ZWJ / skin-tone) :
  largeur gonflée → overflow en bas d'écran. Reproduit ; recouvre en partie le
  holdback combining-mark du round 1 mais pas les clusters ZWJ/skin-tone.
- **Divergence de table de largeur** app vs Kova sur `⏺`/VS16 (`⚠️`) : Node
  string-width compte 2 colonnes là où Kova en compte 1 → ligne pré-wrappée par
  l'app déborde. Piste prioritaire pour la prochaine occurrence : rejouer une
  capture réelle avec `replay_capture_file` et bissecter autour du glyphe.

Snapshots round 3 : `holes_report.json` + `hole_pane_{13,45,97,100}.json`
(scratchpad de session). Journal du workflow (code des tests de repro, verdicts) :
`subagents/workflows/wf_13ff246f-482/journal.jsonl`.

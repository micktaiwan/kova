# Track — Kova

## En cours

### Pane switcher — rebind Cmd+P + layout 3 colonnes

**Statut** : Rebind fait + layout 3 colonnes implémenté et buildé (2026-06-23) ; reste le test manuel.

**Contexte** : Cmd+Shift+P était pris par autre chose sur le Mac de Mickael → switcher rebindé sur **Cmd+P**.

**Fait** :
- Défaut `open_pane_switcher` passé de `cmd+shift+p` à `cmd+p` (`src/config.rs:362`)
- Commentaire mis à jour (`src/window.rs:114`) ; build OK
- À noter : si un fichier de config perso surcharge `open_pane_switcher`, le défaut codé en dur ne s'applique pas — vérifier.

**Layout 3 colonnes (implémenté, mode « une colonne = un groupe »)** :
- `PaneSwitcherState` est passé d'une liste plate (`rows`) à `columns: Vec<Vec<SwitcherRow>>` + `selected_col` / `selected_row` + `scroll: Vec<usize>` (un offset par colonne) (`src/window.rs`).
- Partition : `do_open_pane_switcher` groupe chaque tab (header + ses panes) puis répartit les groupes sur `ncols = min(3, nb_tabs)` colonnes contiguës, équilibrées par nb de lignes — un tab n'est jamais coupé entre 2 colonnes. Décision greedy : on ferme la colonne courante avant d'ajouter un groupe si ça rapproche du target par colonne.
- Navigation : ↑↓ dans la colonne (skip headers), ←→ entre colonnes via `nearest_pane_row` (snap au pane le plus proche en index). Keycodes 0x7B/0x7C ajoutés.
- Hit-test clic : colonne = `px / (viewport_w/ncols)`, puis ligne via `overlay_list_geometry` + `scroll[col]` (`handle_pane_switcher_click`, prend maintenant `px`).
- Rendu : `build_pane_switcher_overlay_vertices` dessine les colonnes côte à côte (largeur `viewport_w/ncols`), highlight sur `(selected_col, selected_row)`, indicateurs de scroll ▲▼ par colonne. Render data : `PaneSwitcherColumnRender` + `PaneSwitcherRenderData { columns, selected_col, selected_row }` (`src/renderer/mod.rs`).
- Scroll vertical : conservé **par colonne** pour le cas dégénéré (1 tab à très nombreux panes) ; seule la colonne sélectionnée est clampée.

**Prochaine action** : test manuel — ouvrir le switcher (Cmd+P) avec ≥3 tabs, vérifier la répartition équilibrée, la nav ←→/↑↓, le clic par colonne, et le cas 1-2 tabs (1-2 colonnes).

### Indicateur pane-level pour bell (BEL) et completion (OSC 133) — sous-bugs

**Statut** : Les 2 sous-bugs sont fixes et commites (`eeff01e`, pushe 2026-06-11) — reste le test manuel.

**Contexte** : On veut que quand Claude Code finit de repondre (BEL) ou qu'une commande se termine (OSC 133;D), le pane non-focus affiche un indicateur visuel (dot + status bar teintee).

**Ce qui a ete fait** :

1. **zshrc** (commite + pushe dans dotfiles) :
   - Ajout hook `precmd` emettant `OSC 133;D` (command finished)
   - Ajout hook `preexec` emettant `OSC 133;C` (command started)
   - Le completion indicator (vert) fonctionne maintenant avec `sleep 3`, etc.

2. **Kova (commite dans main — verifie 2026-05-13)** :
   - Ajout enum `PaneAttention` (None / Completion / Bell) dans `renderer/mod.rs:83-94`
   - `pane_data` passe maintenant 8 champs (ajout `has_bell` per-pane, `renderer/mod.rs:174-175`)
   - `build_status_bar_vertices` recoit `PaneAttention` : fond orange (bell), vert (completion), ou defaut
   - Dot en haut a droite du pane : orange pour bell, vert pour completion
   - Clear du bell flag quand on focus le pane (2 endroits dans window.rs)
   - Bell lu par `load` (sans consommer) pour le pane-level, `swap` toujours utilise par `check_bell` pour le tab-level

3. **Bell pane-level — FIXE (commite `eeff01e`)** : la race etait confirmee — `pane_data` lisait le bell a la frame N, puis `check_bell()` (meme frame, tous les tabs y compris l'actif) faisait `swap(false)` : le dot ne vivait qu'une frame (~16ms), invisible. Fix :
   - `pane.rs check_bell()` : `swap(false)` → `load` (le flag pane est sticky, le tab-level s'en derive)
   - `window.rs` render loop (pane_data) : le pane focuse cleare son bell a chaque frame ("vu"), evite un dot perime quand le focus repart ailleurs. `command_completed` n'est PAS cleare la (contrat IPC `wait-for-completion` : flag sticky jusqu'au prochain 133;C)
   - Les 2 clears existants au changement de focus (click + nav clavier) restent

4. **Dots au demarrage — FIXE (commite `eeff01e`)** : ajout `osc133_primed: bool` sur `TerminalState`. Le premier `133;D` sans `C` prealable (= precmd de demarrage du shell) est avale et prime le flag ; tout D suivant fonctionne, y compris D-sans-C du hook Stop de Claude Code. 2 tests de regression ajoutes dans `parser.rs` (`first_osc133_d_without_c_is_swallowed`, `osc133_c_then_d_sets_completed`).

5. **Fix hooks Claude Code** (2026-03-07, dans dotfiles `claude/settings.json`) :
   - Les hooks Stop/Notification de Claude Code utilisaient `printf '\a' > /dev/tty` pour envoyer un BEL a Kova, mais `/dev/tty` n'est pas disponible dans le contexte des hooks (pas de TTY attache)
   - Fix : remplace par `PARENT_TTY=$(ps -o tty= -p $PPID) && printf > /dev/$PARENT_TTY` pour ecrire sur le TTY du process parent
   - Ajout de `OSC 133;D` dans le hook Stop pour declencher l'indicateur de completion dans le pane quand Claude termine
   - Desactive le plugin ralph-loop (non utilise, causait une erreur "Failed with non-blocking status code: No")
   - Resultat : bell (point orange tab) + completion (point vert pane) fonctionnent depuis les hooks Claude Code

**Prochaine action** : test manuel — (a) relancer Kova et verifier qu'aucun dot vert n'apparait au demarrage, (b) faire emettre un BEL depuis un pane non-focuse (hook Stop de Claude Code ou `printf '\a'`) et verifier le dot orange pane-level persistant jusqu'au focus.

### Bug: scrollback affiche le contenu d'un autre tab

**Statut** : En cours (logging commite dans main, en attente de reproduction).

**Contexte** : En scrollant vers le haut dans un tab (ex: Pincer), le scrollback affiche le contenu d'une session d'un autre tab (ex: Lemlist). Un seul pane concerne, pas un split. Taper un caractere reset le scroll et corrige l'affichage. Bug intermittent, observe au moins 2 fois.

**Analyse (2026-03-11)** : review complete du code — aucun bug evident trouve. Chaque pane a son propre `TerminalState` avec scrollback isole, le rendu utilise le bon terminal avec scissor rect, le routage PTY→terminal est correct. Hypotheses restantes :
- Race condition subtile entre PTY reader thread et main thread
- Corruption du scrollback lors du reflow (resize declenche par changement de tab)
- Bug memoire lie au `unsafe` dans `pane_at_event` (reference raw pointer apres drop du borrow)

**Logging — etat 2026-06-11 (SCROLL-BEGIN commite `31e8341`)** :
- `terminal_id` unique sur chaque `TerminalState` : en place
- `SCROLL-START term_id=X sb_len cwd first_sb` : niveau **info** (`terminal/mod.rs:498`) — identite du terminal au demarrage d'un scroll
- `SCROLL-BEGIN tab=X pane=X term_id=X` : niveau **info** (`window.rs`, juste avant `term.scroll`) — une ligne par session de scroll (offset 0 → >0), donne la correlation tab/pane/term_id sans `RUST_LOG=debug` ni spam (c'est pour ca que `SCROLL-EVENT` reste en debug : il fire a chaque tick de trackpad)
- `SCROLL-EVENT ...` : reste en **debug** (`window.rs`)
- `RENDER-SCROLLED` : supprime (commit `65cd62b`), pas remis

**Prochaine action** : a la prochaine repro, checker `~/Library/Logs/Kova/kova.log` : si le `term_id` de `SCROLL-BEGIN` (tab/pane ou on scrolle) ≠ le `term_id` de `SCROLL-START` (terminal qui scrolle reellement), le routage event→terminal est en cause ; si egaux mais contenu faux, c'est le scrollback lui-meme (reflow/corruption).

### Bug: bande blanche (« trou ») dans Claude Code — repaint différentiel alt-screen

**Statut** : En cours — cause non élucidée, repro déterministe à faire. Détail complet : `notes/display-glitches.md` § Round 2.

**Contexte** : Trou (bande de rangées vides) au milieu du texte d'une pane Claude Code ; Cmd+R répare ; re-scroller (haut puis bas) le ramène au même endroit. Rapporté 2026-06-17 sur Kova v1.8.0. Bug récurrent (déjà ~50 fixes en v1.8.0 sans le tuer).

**Faits établis (vérité-terrain par dump IPC, 2026-06-17)** :
- Claude Code est en **alt-screen** (`count-pane-content mode=scrollback` = 0 octet sur 6 panes) → le bug n'a RIEN à voir avec le scrollback / la gravité / le cross-tab (tous innocentés).
- Le trou = bande de N rangées vides dans la **grille alt** (pane 194 : 9 rangées vides, lignes 33-41, juste après un bloc `Update()` soft-wrappé, avant `⏺ Capitalisé`). Kova ne met pas à jour ces cellules pendant le repaint différentiel que Claude émet au scroll (molette → SGR mouse mode 1000+1006 → redraw, `window.rs:919`).
- **Pas déjà corrigé** : process qui tourne lancé le 11/06 07:18 = binaire v1.8.0 ; seul commit terminal depuis (`48625d1`) sans rapport (search/URL/SGR/sélection). Bug présent dans HEAD (`0e30761`) aussi → rebuild ne corrige pas.
- `reverse_index` (1930) / `scroll_down` (961) / IL (1281) / DL (1307) / régions (1376) corrects à la lecture ; renderer innocenté ; `y_offset_rows` renvoie 0 en alt-screen.
- Une autre session a laissé des sondes `probe_*` **non-commitées** dans `src/terminal/mod.rs` (hypothèse scrollback, écartée par la vérité-terrain) — ne pas y toucher.

**Prochaine action** : repro déterministe — `script -q /tmp/kova_cap.raw claude` dans une pane neuve, faire apparaître le trou, puis rejouer le fichier dans `drive(cols, rows, chunks)` (`parser.rs:913`) à 85×65, bissecter → séquence fautive → test de régression + fix. Snapshots de la session : `/tmp/kova_hole_194.json`, `/tmp/kova_dump_194_{visible,all}.json`.

## En attente

### Bug: resultats de recherche perimes si du texte arrive overlay ouvert

**Statut** : En attente (decision de design).

**Contexte** : trouve lors de la campagne de bug-hunt du 2026-06-11 (11 bugs corriges, commits `559c268..12674b0`). Les resultats de l'overlay de recherche (`FilterMatch.abs_line`, remplis par `search_lines` dans `terminal/mod.rs`) stockent des indices de ligne absolus (scrollback + grid). Si du texte arrive pendant que l'overlay est ouvert et que le scrollback est plein (`pop_front` a chaque ligne), tous les indices se decalent : cliquer un resultat (`scroll_to_abs_line`) scrolle au mauvais endroit. Meme probleme apres un resize (reflow) overlay ouvert.

**Note** : la selection avait le meme defaut, corrige dans `48625d1` (elle suit son contenu au trim et est invalidee au reflow). Les matches du filtre n'ont pas ete traites car le fix demande un choix d'UX.

**Options** :
1. Re-executer `search_lines` quand le contenu du terminal change pendant que l'overlay est ouvert (simple, coute une re-recherche par batch d'output)
2. Decaler les `abs_line` des matches au `pop_front` (comme la selection) + invalider au reflow (plus chirurgical, ne rattrape pas les nouvelles lignes qui matchent)
3. Figer : invalider/fermer les matches des que le contenu change (le plus simple, UX moins bonne)

**Point cosmetique lie (non bloquant)** : le soulignement de hover d'URL (Cmd maintenu) peut rester affiche au mauvais endroit si du texte defile, jusqu'au prochain mouvement de souris. Le Cmd+clic est sur depuis `7263155` (re-validation au clic) — seul l'affichage transitoire est faux.

### Kitty Keyboard Protocol (flags=1 disambiguate)

**Statut** : Commite cote Kova ; **PR Ink mergee le 2026-03-09** (verifie via gh le 2026-06-11). A tester avec un Claude Code recent.

**Contexte** : Les apps TUI (Claude Code, neovim) activent le kitty keyboard protocol pour recevoir des sequences de touches non ambigues (CSI u). Sans ca, Ctrl+O et d'autres combos sont silencieusement perdus.

**Ce qui a ete fait** :

1. **Kova** (commite dans main — verifie 2026-05-13) : implementation complete du protocole kitty flags=1.
   - `src/terminal/mod.rs:224,305` : champ `kitty_keyboard_flags: Vec<u8>` + helper `kitty_flags()`
   - `src/terminal/parser.rs:325-329` : push (`CSI > flags u`), pop (`CSI < u`), query (`CSI ? u`)
   - `src/input.rs` : encodage CSI u pour Ctrl/Alt+key, xterm modifiers pour touches speciales
   - `src/window.rs` : bypass `interpretKeyEvents` en mode kitty pour Ctrl/Alt
   - Stack videe automatiquement sur RIS (full reset)
   - Verifie manuellement : `printf '\e[>1u' && cat -v` → Ctrl+O produit `^[[111;5u` ✓

2. **Pourquoi ca ne marche pas avec Claude Code** : Ink (la lib UI) a une whitelist hardcodee de 4 terminaux (`iTerm.app`, `kitty`, `WezTerm`, `ghostty`). Le mecanisme de query `CSI ? u` existe dans Ink mais n'est envoye qu'aux terminaux de la liste. Kova n'y est pas → pas de push → pas de kitty.

3. **PR Ink** : https://github.com/vadimdemedes/ink/pull/895 — **MERGEE le 2026-03-09**
   - Supprime la whitelist, envoie la query `CSI ? u` a tous les terminaux TTY en mode auto
   - Le timeout de 200ms gere deja les terminaux non-compatibles

4. **Analyse binaire CC 2.1.173 (2026-06-11)** : le binaire embarque un parser de reponse `kittyKeyboard` (regex sur `CSI ? flags u`) au sein d'un systeme de probing de capacites (da1/da2/decrpm) — coherent avec l'Ink post-PR #895 (query envoyee a tous les terminaux). Pas de preuve definitive depuis les strings que la query part bien vers Kova ; Kova ne loggue pas les pushes kitty donc pas de confirmation possible par les logs.

**Prochaine action** : test interactif par Mickael — Ctrl+O dans Claude Code (2.1.173+) dans Kova. Si KO, ajouter un log info sur `KittyKeyboardPush` dans `parser.rs:425` pour trancher.

## Idees

- **Infos child processes sur raccourci** : afficher le nombre de process enfants en cours. ⚠️ Cmd+Shift+I est deja pris (memory/perf report — cf README), choisir un autre combo.


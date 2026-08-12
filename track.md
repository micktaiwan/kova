# Track — Kova

## En cours

### IPC — `subscribe`, le flux d'events

**Statut** : codé, `cargo test` vert (193 tests), buildé en release. Pas encore de client branché dessus.

**Pourquoi** : un client qui veut suivre l'attention (track, `/pane-sweep`, Kite) n'avait que le polling de `list-panes`, qui coûte un `proc_pidinfo` + un walk de la table des process par pane. Le déclencheur est track : imputer le temps par projet quand Mickael passe d'une session Claude à l'autre toute la journée.

**Ce que ça fait** : `{"cmd":"subscribe","events":[…]}` transforme la connexion en flux. La réponse est un **snapshot** (focus + tous les panes, mêmes objets que `list-panes`) — donc pas de bootstrap séparé ni de trou entre l'état et les changements — puis une ligne JSON par edge : `focus`, `pane-status`, `pane-working`, `pane-open`, `pane-close`, plus un `ping` toutes les 30 s de silence.

**Décisions structurantes** :
- **`focus` intègre « Kova est-il au premier plan »**. Partir sur Slack émet `pane: null` / `reason: "app-inactive"`. Sinon chaque client devrait recroiser ce flux avec les notifications d'app active du système pour savoir si l'utilisateur regarde vraiment le pane.
- **Diff sur le tick, pas d'instrumentation des mutations**. Le focus bouge depuis une douzaine d'endroits et `working`/`awaiting` sont dérivés, pas posés : hooker chaque site aurait garanti d'en oublier un. `src/events.rs` garde le dernier état publié et le compare.
- **Le main thread n'écrit jamais sur une socket**. Il pousse dans des files bornées (256), le thread de la connexion écrit. Un client lent est **déconnecté** plutôt que servi troué : il se reconnecte, le snapshot le resynchronise. C'est ce qui garantit qu'aucun abonné ne peut ralentir la boucle de rendu.
- **Inscription avant snapshot** : un event qui tombe entre les deux est livré juste après le snapshot. Redondant parfois, jamais troué — chaque event porte un état absolu.
- **Coût zéro quand personne n'écoute** : un load atomique par tick. Le focus est comparé chaque frame (deux lectures de `RefCell`), le balayage des panes est throttlé à ~4 Hz, et le JSON coûteux (CWD, process) n'est construit que sur un edge.

**Fichiers** : `src/events.rs` (neuf — diff + snapshot), `src/ipc.rs` (registre d'abonnés, `publish`, `stream_events`, parsing), `src/window.rs` (`pane_json` extrait et partagé par `list-panes`, le snapshot et les events), `src/app.rs` (poll dans le tick, `applicationDidBecomeActive/ResignActive`).

**À tester** : `printf '%s' '{"cmd":"subscribe"}' | nc -U $KOVA_SOCKET` puis bouger entre panes/onglets/apps et regarder les lignes tomber.

**Limite connue** : le socket porte le pid de Kova, donc un redémarrage déplace le chemin — un abonné longue durée doit re-glob `/tmp/kova-*.sock` et se réabonner.

**Suite (2026-08-12)** : `claude_session_id` et `claude_session_name` ajoutés à l'objet pane (donc dans `list-panes`, le snapshot et les events). Lus depuis `~/.claude/sessions/<pid>.json` sur le même probe throttlé que le reste ; `Pane.claude_name` devient `Pane.claude_session` et porte l'objet entier. Motif : c'est le **seul identifiant qui survive au pane** — le `pane_id` meurt avec l'onglet, le `cwd` est partagé par toutes les conversations d'un même repo. Track s'en sert pour attacher une conversation à une tâche.

**Suite (2026-08-13)** : la conversation fait partie de l'**identité du focus** (`FocusKey.session` dans `src/events.rs`), pas seulement du payload. Sans ça, l'event `focus` ne partait qu'au changement de *pane* : lancer `claude` dans le pane où on est déjà n'annonçait rien, et track ne l'apprenait qu'en sortant puis revenant (constaté à l'usage par Mickael). Nouvelle valeur de `reason` : `session`. Nécessite un redémarrage de Kova pour prendre effet.

### Restauration des sessions Claude Code au redémarrage

**Statut** : mécanisme permanent en place et vérifié le 2026-08-04. Le bootstrap jetable qui l'accompagnait est retiré, c'est lui qui a causé la panne du 2026-08-04.

**Ce que ça fait** : au snapshot de session, Kova repère le Claude Code qui tourne dans chaque pane et mémorise son identifiant de conversation. À la restauration, ce pane reçoit `claude --resume <id>` pré-tapé au lieu de la dernière commande shell, sans retour chariot, donc rien ne part sans un Entrée. Un pane qui était à l'invite du shell garde le comportement d'avant. Le titre de conversation est sauvé et réinjecté lui aussi, pour qu'un pane non relancé ne retombe pas sur le nom de son répertoire dans le switcher.

**Comment le pane est relié à sa session** : Claude Code écrit `~/.claude/sessions/<pid>.json` (contient `sessionId`) tant que le process vit, et le supprime en sortant. Kova lit ce répertoire et remonte la filiation de chaque process `claude` jusqu'au shell du pane. La lecture doit donc précéder `pty::shutdown_all()`, c'est déjà l'ordre de `applicationWillTerminate`.

**Contrainte vérifiée le 2026-07-29** : `claude --resume <uuid>` ne retrouve la conversation que depuis le répertoire où elle a démarré (les transcripts sont rangés par cwd dans `~/.claude/projects/<slug>/`). Testé dans les deux sens : même répertoire → la conversation revient ; autre répertoire → `No conversation found`. Kova restaure déjà le cwd du pane, donc l'injection tombe juste.

**Piège de l'identité du process** : l'exécutable de Claude Code est un fichier au nom de version (`~/.local/share/claude/versions/2.1.220`), donc `proc_name` renvoie `2.1.220`, jamais `claude`. Un garde-fou anti-fichier-périmé basé sur le nom rejetait toutes les sessions. Il compare maintenant l'heure de démarrage du process au champ `startedAt` du fichier de session (dérive mesurée : moins d'une seconde). Même cause pour `foreground_process_name()`, **corrigé le 2026-08-10** : `process_info` lit `argv[0]` via `sysctl KERN_PROCARGS2` (`src/terminal/pty.rs:82`) et garde le nom du fichier exécuté comme version quand c'en est une, donc le nom est `claude` et la version est un champ à part.

**Post-mortem du 2026-08-04 : le bootstrap a tiré 5 jours trop tard.** Un fichier jetable, `~/.config/kova/claude-sessions.json`, écrit hors de Kova le 2026-07-29 à 14:55 avec 26 sessions, servait à couvrir le tout premier redémarrage depuis un build qui ne savait pas encore enregistrer les identifiants. Il n'a été consommé que le 2026-08-04 à 14:16, sur une disposition de panes qui n'avait plus rien à voir : 19 captures sans pane correspondant, 7 panes pré-remplis avec des conversations vieilles de 5 jours. **Décision : le bootstrap est supprimé** (code de fusion, script de capture, format de fichier). Un fichier de reprise sans date de péremption ni lien avec l'état courant est un piège, et le mécanisme permanent n'en a plus besoin.

**Vérifié le 2026-08-04** : la détection native marche, `~/.config/kova/session.json` porte l'identifiant réel des 4 sessions Claude vivantes ainsi que leur titre. Reste le fichier mort `~/.config/kova/claude-sessions.used.json`, supprimable.

**Séquelle à connaître** : les 6 panes pré-remplis le 2026-08-04 portent une ligne `claude --resume` vers une conversation du 2026-07-29. Tant qu'aucune commande n'y est lancée, cette ligne se recopie de redémarrage en redémarrage. Effacer la ligne dans le pane suffit à s'en débarrasser.

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

### Bug: bande blanche (« trou ») dans Claude Code

**Statut** : Round 6 — nouveau déclencheur identifié (le scroll, pas le resize). Fix côté entrée souris posé et buildé, à valider après redémarrage de Kova. Détail : `notes/display-glitches.md` § Round 6 (rounds 1-5 = historique).

**Contexte** : Trou (bande de rangées vides) au milieu du texte d'une pane Claude Code ; Cmd+R répare. Récurrent depuis v1.8.0.

**Ce qu'on sait maintenant** : le trou apparaît sans aucun resize, en scrollant vers le haut dans une session Claude Code. Le flux émis ne contient aucun effacement d'écran ; l'app redessine son bloc 4 rangées plus bas à chaque cran et laisse vides les rangées libérées. Rejoué hors app, tmux produit exactement la même grille que Kova : le rendu est fidèle, le trou est déjà dans les octets reçus.

**Différence Kova vs Terminal.app** : sur le même geste, Kova envoyait 37 % de rapports souris en double, Terminal.app aucun. Deux chemins de livraison de `mouseMoved:` étaient actifs en même temps. Corrigé.

**Angle mort connu** : les garde-fous du round 5 ne s'arment qu'après un resize, donc ils ne verront jamais un trou né d'un scroll.

**Prochaine action** : redémarrer Kova, relancer `scripts/mouse-probe.py` (zéro doublon de motion attendu), puis rescroller dans une session Claude Code chargée. Si le trou revient, suspect suivant : la rafale d'un événement par ligne de molette.

**Question ouverte** : 7 s sans aucun événement au lancement du mouchard, cause non établie. Suspect : le verrou d'axe de scroll, global à la fenêtre et jamais réinitialisé pour une souris à molette.

### Bug: le nombre de colonnes/lignes change au switch d'écran (scale 2x ↔ 1x)

**Statut** : En cours — diagnostiqué (2026-07-03), aucun fix. Analyse par trace complète + relecture des lignes clés.

**Symptôme** : en passant d'un écran externe au retina (ou l'inverse), la grille du terminal change de dimensions → une table affichée par un programme reflow avec une largeur différente. Pénible et récurrent.

**Cause racine** : tout le calcul de grille se fait en **pixels physiques**, et la cellule est arrondie *après* multiplication par le scale, ce qui rend la taille de cellule ramenée au point logique dépendante de l'écran.
- L'atlas est construit à `font.size * scale` (`src/renderer/mod.rs:342`, rebuild `:1883`).
- Les métriques sont `ceil()` en physique : `cell_height` (`src/renderer/glyph_atlas.rs:88`), `cell_width` (`:109`).
- `cols`/`rows` = viewport physique / cellule physique (`src/window.rs:4636-4638`, idem `viewport_to_grid` `:2004`). `cell_size()` renvoie du physique (`src/renderer/mod.rs:2485`).
- Exemple police 14, avance 'M' ≈ 8,42 pt : 2x → `ceil(16.84)=17` phys = 8,5 px logiques/col ; 1x → `ceil(8.42)=9` phys = 9,0 px logiques/col. Même largeur logique → **plus de colonnes en retina** qu'en externe.
- Chaîne au switch : `viewDidChangeBackingProperties` (`src/window.rs:748`) → `handle_resize` (rebuild atlas au nouveau scale) → `resize_all_panes` → `term.resize` + `pty.resize` → SIGWINCH → reflow.
- Contributeur secondaire : `PANE_H_PADDING = 10.0` (`src/renderer/mod.rs:5`) est en px physiques, non scalé (5 px logiques à 2x, 10 à 1x).

**Prochaine action** : choisir la direction de fix (calcul de grille en espace logique vs cellule arrondie en logique puis scalée pour l'atlas ; + scaler le padding), montrer le diff, builder.

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


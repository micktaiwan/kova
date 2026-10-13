# Kova en bibliothèque : un terminal pour Kova.app et pour Kana

Déplacé le 13/10/2026 depuis `kana/docs/claude-pane.md`, où vit le reste de l'analyse (le
besoin dans les mots de Mickael, la partie Claude, la synchro avec le texte, les pistes
écartées). Ce fichier porte la partie qui se fait dans Kova : l'architecture, les tests à écrire
avant de toucher au code, et l'ordre du chantier. Les numéros de ligne datent du 13/10/2026 ;
les relire avant de s'en servir.

Le besoin, en une phrase : Mickael veut dans Kana (son éditeur, `~/projects/perso/kana`) un
pane Kova par note, où tourne une conversation Claude Code, avec le même code de terminal que
Kova.app (aucune copie), zéro régression sur Kova garantie par des tests écrits avant le
refactoring, et les sessions visibles dans Kite.

## La solution retenue : Kova devient une bibliothèque, Kova.app et Kana en sont deux clients

Un seul code de terminal, utilisé par deux apps. Kova.app est le premier client et ne perd rien ;
Kana est le second et affiche un pane par note. C'est l'architecture de Ghostty : un cœur de
terminal en bibliothèque, et une app macOS qui l'utilise avec une vue par split
(`ghostty_surface_new`, `include/ghostty.h` du repo ghostty-org/ghostty). Ghostty présente
cette API comme interne, pas conçue pour d'autres apps ; ici les deux clients sont à Mickael,
donc ce n'est pas un frein.

### Les pièces

- **Rien n'existe tant qu'aucun pane n'est ouvert** : ouvrir le pane d'une note crée son
  `Surface` (le shell, puis `claude`) ; le fermer détruit le `Surface`, ce qui termine `claude`.
  Le contexte de rendu partagé est créé avec le premier pane et libéré avec le dernier.

- **Un `Surface` par pane, dans la bibliothèque** : le PTY, l'état du terminal, la sélection, le
  défilement, la session d'agent, le flag « attend l'utilisateur ». La base existe :
  `Pane` porte déjà son PTY, son terminal, `awaiting` et `agent_session`
  (`kova/src/pane.rs:1656-1687`). Le rafraîchissement de la session d'agent, aujourd'hui fait au
  niveau de l'onglet (`Tab::check_running`, `pane.rs:779-800`), descend dans le pane.
- **L'entrée par une API typée, comme Ghostty** : `key`, `text`, `preedit` (le texte en cours de
  composition, les accents), `mouse`, `scroll`. Toute la logique (quels octets envoyer pour une
  touche, le choix entre composer un accent ou envoyer Option, Ctrl+L, la sélection au clic,
  le défilement) vit dans la bibliothèque, une seule fois. La traduction des événements macOS
  (touches, texte en composition, souris) vers cette API est elle aussi écrite une seule fois,
  dans la bibliothèque : un composant de saisie partagé. `KovaView` traite d'abord ce qui
  appartient à la fenêtre (overlays, raccourcis d'onglets) puis lui passe la main pour le pane
  actif ; dans Kana, la vue du pane est fournie par la bibliothèque et n'est que ce composant
  plus un `Surface`. Il ne reste dans chaque classe de vue que les méthodes que macOS exige,
  chacune d'une ligne qui délègue : aucun code de saisie en double, et Kana n'en écrit aucun.
  Ghostty sépare de la même façon la logique (dans le cœur : `ghostty_surface_text`,
  `ghostty_surface_preedit`) de la vue macOS.
  État à l'étape 2 (13/10/2026) : `key`, `text`, `mouse`, `scroll` sont sur `Surface`
  (`src/input.rs`) ; le texte en composition reste dans le composant de saisie partagé
  (`src/appkit_input.rs`), parce que rien dans le terminal ne le lit encore.
- **Le dessin : un contexte partagé et un dessinateur par pane.** Un `RenderContext` (device
  Metal, pipeline, atlas de glyphes par échelle d'écran) et un `PaneRenderer` qui dessine un pane
  dans le rectangle que l'hôte lui donne. Kova.app garde un seul layer par fenêtre et dessine
  ses panes dedans, comme aujourd'hui ; Kana donne un layer qui ne contient qu'un pane.
  Pourquoi pas un layer par pane comme Ghostty : Kova dessine aujourd'hui toute la fenêtre en
  une seule image (un seul `nextDrawable` et un seul `presentDrawable`,
  `kova/src/renderer/mod.rs:1092`, `:1444`), overlays compris. Garder ce modèle, c'est ne rien
  changer à ce que Kova.app affiche, donc ne rien risquer sur un comportement déjà validé ;
  passer à un layer par pane changerait la façon dont la fenêtre se compose, sans gain prouvé
  (aucune mesure n'a été faite dans un sens ou dans l'autre). Le code est déjà presque découpé ainsi : un cache de vertices et une
  zone de découpe par pane (`mod.rs:927`, `:1317-1325`). Le gain de Ghostty, ne pas redessiner
  un pane qui ne change pas quand on fait défiler les colonnes, s'obtient aussi : le cache
  contient aujourd'hui la position du pane (`mod.rs:827`), il suffit de dessiner en coordonnées
  locales et de passer la position à part.
  **Corrigé le 13/10/2026 (étape 2)** : pas aussi simple. Le build arrondit chaque position
  au pixel entier dans l'espace du drawable (`(ox + col_idx as f32 * cell_w).round()`,
  `src/renderer/pane_renderer.rs`), et la position d'un pane peut être fractionnaire (largeurs
  de colonnes en poids, défilement au trackpad). Un build fait à l'origine puis translaté place
  les glyphes jusqu'à un demi-pixel à côté ; le test `a_shifted_build_is_not_a_build_in_place`
  fige ce contre-exemple. Le cache reste donc indexé sur le rectangle absolu. Variante non
  évaluée : construire avec la partie fractionnaire de la position et translater de la partie
  entière, ce qui garde les arrondis ; restent les sommes non arrondies (texte de la barre de
  statut, inclinaison de l'italique) qui tomberaient sur d'autres f32, effet à l'écran non mesuré.
- **L'horloge reste à l'hôte** : le pane dit s'il doit être redessiné, l'hôte décide quand.
  Kova.app garde son timer (`kova/src/app.rs:226-232`). Le clignotement du curseur, compté en
  images aujourd'hui (`mod.rs:1002`), passe en temps réel pour ne pas dépendre de l'hôte.
- **Le socket IPC pour Kite** : le transport (`kova/src/ipc.rs`) et les commandes qui portent sur un pane (`list-panes`, `send-keys`,
  `get-pane-content`, `count-pane-content`, `focus-pane`, `close-pane`, `notify`…) passent dans
  la bibliothèque, écrites une fois contre une interface d'hôte (« donne-moi les panes, ce
  pane, mets-le au premier plan »). Le transport n'est pas tout à fait autonome : il lit une
  constante du rendu, `crate::renderer::TAB_COLORS` (`kova/src/ipc.rs:24`), à déplacer avec lui.
  Fait à l'étape 2 : elle vit dans `src/tab_colors.rs`, et l'interface d'hôte est le trait
  `IpcHost` (`src/ipc/host.rs`).
  Les commandes d'onglets et de fenêtres restent à Kova.app, sauf deux que Kite envoie à
  l'instance qui tient le pane : `list-tabs` et `close-tab` (`kite/daemon/src/api.rs:655-664`,
  fermeture d'un projet depuis Kite). Kana y répond avec « un onglet par note », et `close-tab`
  y ferme le pane de la note.
- **Kana a son propre socket**, `/tmp/kova-<pid de Kana>.sock`, au même protocole. Kite trouve
  déjà toutes les instances (`kite/daemon/src/kova.rs:25-46`) et range les panes par socket.
  `KOVA_SOCKET` et `KOVA_PANE_ID`, donnés au shell (`kova/src/terminal/pty.rs:234-235`), pointent
  alors vers Kana, donc les hooks Claude qui marquent un pane (`~/.claude/hooks/kova-pane-status.sh`)
  marchent dans Kana sans changement. Enregistrer les panes de Kana dans le process Kova.app
  obligerait à faire passer le terminal d'un process à l'autre : plus compliqué, et Kana
  dépendrait de Kova ouvert.
- **Ce qui reste à Kova.app** : fenêtres, onglets, colonnes, séparateurs, overlays, signets,
  ancres, Cmd+J.

### Deux règles de Kova à lever explicitement

`kova/CLAUDE.md`, section Principes, dit « Pas de feature creep : tabs, splits, config, c'est
tout » et « Du code que Mickael a validé […] ne se modifie plus », parce que toucher au
comportement validé crée des régressions. Ce chantier ajoute une fonction à Kova (servir un
autre hôte) et déplace du code validé. La seconde règle vise le comportement : le refactoring
ne doit en changer aucun, et les tests ci-dessous sont là pour le prouver. Les deux restent à
lever par Mickael avant de commencer, et `kova/CLAUDE.md` à mettre à jour.

### D'abord les tests, ensuite le refactoring

Le code qui va bouger n'a presque pas de tests : aucun dans `kova/src/input.rs`, un seul dans
`kova/src/window/mod.rs` (2 682 lignes). La politique de Kova teste la logique pure et laisse
Metal et AppKit de côté (`kova/CLAUDE.md`, section Tests). D'où l'ordre, chaque étape gardant
Kova.app utilisable et testé :

1. Tests de sélection, mots, texte sélectionné et URL sur `TerminalState`, sans rien déplacer :
   les tests construisent déjà un terminal en mémoire (`kova/src/terminal/mod.rs:3317`).
2. Sortir le calcul « touche → octets » en fonction pure (`input.rs:53-174`) et figer toute la
   table : Ctrl+lettre, flèches en mode application, protocole kitty, Shift+Tab, Option.
3. Le choix composer un accent ou envoyer Option (`window/mod.rs:605-608`), Ctrl+L, Ctrl+F.
4. Les octets de souris SGR et le pas de défilement du trackpad.
5. Le passage pixel → cellule sans `NSEvent`, puis la sélection au clic et au glisser.
6. Une interface « écrire des octets » à la place du vrai PTY, puis les tests des commandes IPC.
7. Un constructeur de vertices avec un faux atlas de glyphes, puis des instantanés de vertices
   (plus stables qu'une comparaison d'images, que la police et la version de macOS feraient
   varier).
8. Ce qui ne s'automatise pas (vraies touches mortes françaises, ^ puis e, Option+$ ; place de
   la fenêtre des accents ; défilement à inertie ; triple-clic réel) : une courte liste de
   gestes, jouée avant et après le refactoring, dans un pane qui affiche les octets reçus
   (`od -c`) et relue par `get-pane-content`.

### Les gestes manuels de l'étape 8

Joués deux fois : une passe « avant » sur le Kova installé, une passe « après » sur chaque build
du refactoring, et les deux traces comparées avec `diff`. Le pane de mesure lance
`python3 scripts/key-probe.py <label>` (une ligne `od -c` par lecture du PTY, trace dans
`~/Downloads/kova-embedding/key-probe-<label>.log`, Ctrl+D seul pour arrêter) ; la souris passe
par `scripts/mouse-probe.py <label>` (q pour arrêter). Ce que ces gestes couvrent et que les tests ne
peuvent pas toucher : le choix que fait macOS entre `insertText:` et `doCommandBySelector:`, le
vrai clavier AZERTY, la vraie fenêtre des accents, le vrai trackpad.

Passe clavier sans kitty (`key-probe.py avant`), puis la même avec `--kitty` (`avant-kitty`) :

1. `^` puis `e`, `¨` puis `i`, `` ` `` puis `a` (touches mortes) ; `^` puis espace.
2. Option+$, Option+(, Option+e puis e.
3. Appui long sur `e`, choisir `é` au clavier : noter où s'ouvre la fenêtre des accents.
4. Ctrl+A, Ctrl+C, Ctrl+[, Ctrl+Option+A.
5. Flèches, Home, End, Page Up/Down, Suppr avant, F1 à F4, Shift+Tab, Entrée, Tab, Échap, Retour arrière.
6. Ctrl+flèche haut, Option+flèche haut, Shift+Entrée, Option+←, Option+→, Cmd+←, Cmd+→, Cmd+Retour arrière.

Hors du probe, dans un shell ordinaire :

7. Ctrl+L : l'écran se vide et le scrollback avec (molette vers le haut : rien) ; dans `vim`, Ctrl+L ne vide pas le scrollback.
8. Ctrl+F ouvre le filtre, Échap le ferme.
9. Ctrl+Option+→ et ← : la largeur virtuelle change, rien n'arrive au shell.

Souris (texte connu à l'écran, puis `pbpaste` après chaque geste) :

10. Clic simple : aucune sélection, presse-papiers inchangé.
11. Glisser sur deux lignes ; double-clic sur un chemin `~/src/main.rs:12` ; triple-clic réel sur une ligne ; double-clic puis glisser.
12. Glisser au-delà du bas du pane : le défilement automatique accélère avec la distance.
13. Cmd+survol d'une URL (soulignée), Cmd+clic (ouverte dans le navigateur).
14. Défilement trackpad avec inertie dans un long historique, puis dans `mouse-probe.py inertie` (rapports SGR 64/65).

### L'ordre du chantier

1. Les tests ci-dessus.
2. Le refactoring dans Kova, Kova.app restant le seul client : `Surface`, API d'entrée,
   `RenderContext` et `PaneRenderer`, IPC contre l'interface d'hôte. À chaque étape, tests verts
   et liste de gestes rejouée.
3. La cible bibliothèque : `[lib]` et `staticlib` dans `kova/Cargo.toml` (aucune aujourd'hui),
   plus une API C (créer un pane dans une vue, entrée, taille, lecture du contenu, fermeture).
4. Kana : la vue du pane, le lien de la bibliothèque dans `build.sh` et `Package.swift`, le
   socket, la table note → session (chemin et inode), les trois hooks (SessionStart,
   UserPromptSubmit, PreToolUse), la commande `kana edit`, la commande qui reçoit un nouvel
   identifiant de session, la surveillance du dossier des notes, et la fusion à trois versions si Mickael la valide.

## Non vérifié

- Que deux appels à `MTLCreateSystemDefaultDevice` rendent le même device, pour partager le
  contexte de rendu entre deux layers.
- Qu'une vue Rust fournie par la bibliothèque s'insère sans friction dans la fenêtre AppKit de
  Kana (premier répondeur, focus entre l'éditeur et le terminal).
- Que Kite supporte un serveur qui répond à `list-tabs` par un onglet par note.
- Que garder une image par fenêtre ou passer à un layer par pane change quoi que ce soit de
  mesurable : rien n'a été mesuré.

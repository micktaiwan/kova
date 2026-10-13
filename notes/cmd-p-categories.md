# Cmd+P : ranger les panes en catégories ? — réflexion du 12-13/10/2026

Issu d'un `/brain`. Rien n'est décidé ni codé : c'est une réflexion à reprendre.

## Le point de départ

Mickael, le 12/10/2026 : « je me demande si dans Cmd-P n'ai pas besoin de catégories, ou
regroupement par tags, parce que j'ouvre bcp de panes (j'en ai 53 là...) et des fois je suis
perdu ».

## L'état mesuré ce soir-là

`list-panes` et `list-tabs` sur le Kova vivant, le 12/10/2026 vers 20 h 40 : 53 panes dans
11 onglets d'une seule fenêtre. Deux onglets portent l'essentiel :

- `CTO` : 26 panes
- `self` : 13 panes
- les 9 autres (`lempire`, `pano`, `kraken`, `Mira`, `Kova`, `ImaginOS`, `nido`, `genealogie`,
  `Kana`) : 1 à 3 panes chacun

Cmd+P regroupe déjà par onglet (un en-tête par onglet suivi de ses panes, `src/window/switcher.rs`,
`open_pane_switcher`). Le problème n'est donc pas l'absence de groupes mais deux groupes de 26 et
13 lignes sans repère à l'intérieur.

## Ce que veut dire « perdu »

Deux lectures ont été posées :

- A : il cherche un pane précis et ne le trouve pas (ce que règlerait « taper filtre la liste »,
  point n°1 de `notes/ux-review-2026-10-10.md`, toujours pas fait).
- B : il ne sait plus lesquels de ses panes comptent encore.

**Mickael a tranché : c'est B.**

## Les contraintes qu'il a posées

- **Les onglets sont des projets**, pas des états ni des tags. Les catégories ne doivent donc pas
  être faites en détournant les onglets.
- Le geste qu'il imagine : dans Cmd+P, **Cmd+↑ / Cmd+↓ pour ranger un pane dans une catégorie**.
- Il n'est sûr ni de la solution, ni de son utilité.

## Ce qui existe déjà dans le code

- **Cmd+↑ / Cmd+↓ dans Cmd+P** déplace déjà le pane sélectionné d'un cran dans l'ordre de son
  onglet (`handle_pane_switcher_key` → `pane_switcher_move_selected`, `src/window/switcher.rs` ;
  `move_pane_in_order`, `src/pane.rs:1516`). Il s'arrête aux bords de l'onglet. Le geste qu'il
  imagine est donc déjà pris, et un rangement en catégories devra soit le partager (franchir la
  frontière d'une catégorie en descendant), soit prendre un autre raccourci.
- **Les ancres** (Cmd+Shift+A) et **les bookmarks** (Cmd+B) sont déjà deux classements
  transverses aux onglets, posés à la main ; les ancres ouvrent Cmd+P en tête.
- **Chaque terminal connaît sa dernière activité** (`last_activity_secs`, affichée dans la barre de
  statut du pane, `src/renderer/mod.rs:1920`). Non vérifié en détail : je ne l'ai pas vue dans le
  rendu des lignes de Cmd+P.

## L'objection levée pendant la discussion

Ranger par thème ne répond pas à B : un pane mort depuis trois jours reste mort dans la bonne
catégorie. Et ranger 53 panes à la main, c'est du travail que le système pourrait faire
(règle « faire travailler l'utilisateur final = c'est mal fait »). Piste opposée, sans rangement :
Kova grise ou relègue en bas les panes inactifs depuis plus d'un jour, à partir de
`last_activity_secs` qu'il a déjà.

Mickael n'a pas répondu sur cette piste : sa réponse a porté sur les onglets (projets, pas états).
Les catégories qu'il imagine seraient donc peut-être des **états** (actif / en pause / à fermer),
pas des thèmes. Non confirmé.

## Où reprendre

La question laissée ouverte : dans `CTO` (26 panes), combien sont vraiment finis ? Si c'est la
moitié, le besoin est du tri de fin de vie (fermer), pas du rangement. Si presque aucun, le besoin
est un sous-groupement à l'intérieur d'un projet.

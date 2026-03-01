# Horizontal scroll pour les splits

## Concept

Chaque tab maintient une `virtual_width` qui représente la largeur de l'espace dans lequel les splits se distribuent. Par défaut c'est la largeur de l'écran. Quand `virtual_width > screen_width`, le scroll horizontal s'active.

## Root vs Local splits

- **Cmd+E** (root split) : crée un split `root: true` au niveau racine de l'arbre. Seuls les root splits comptent pour le calcul automatique de `virtual_width`.
- **Cmd+D** (local split) : crée un split `root: false` qui subdivise le pane courant dans son espace alloué. N'affecte pas `virtual_width`.
- `equalize()` donne la même largeur à **toutes** les colonnes (root ou non), via `chain_count(true, false)`.

## Mécanique

- `virtual_width` : **méthode calculée** sur `Tab` — `max(screen_width, root_columns * min_split_width)`, sauf si un override manuel est défini.
- `virtual_width_override` : `f32`, par tab, 0.0 = auto. Quand > 0, `virtual_width = max(screen_width, override)`.
- `scroll_offset_x` : `f32`, par tab, initialisé à 0
- `min_split_width` : configurable (section `[splits]`, champ `min_width`), défaut 300.0 px
- `chain_count(horizontal, root_only)` : méthode unifiée pour compter les colonnes. `root_only=true` ne traverse que les splits root, `root_only=false` traverse tous les splits.

### Cmd+E (root split)

Ajoute une colonne root à l'arbre. `virtual_width()` augmente car `root_columns` croît. Le scroll se déclenche quand `root_columns * min_split_width > screen_width`.

### Cmd+D (split local)

Subdivise le pane courant. `virtual_width()` ne change pas (les splits locaux ne comptent pas dans `root_columns`). Toutes les colonnes gardent la même largeur via `equalize()`.

### Ctrl+Option+→/← (ajustement virtual width)

Ajuste directement `virtual_width_override` du tab actif par pas de 200pt. Intercepté dans `keyDown` (pas `performKeyEquivalent` qui ne fire pas sans Cmd). Quand l'override retombe à `screen_width` ou en dessous, il se désactive (0.0 = auto).

### Fermeture d'un pane

`Tab::scale_virtual_width(old_cols, new_cols)` réduit l'override proportionnellement au ratio ancien/nouveau nombre de colonnes. `clamp_scroll()` borne `scroll_offset_x`.

### Resize fenêtre

`scroll_offset_x` clampé à `max(0, virtual_width - screen_width)`.

## Scroll

### Trackpad

Le scroll horizontal natif du trackpad ajuste `scroll_offset_x`, borné entre 0 et `virtual_width - screen_width`.

### Navigation panes (Cmd+Option+flèches)

La navigation entre panes scroll automatiquement pour révéler le pane focusé.

## Rendu

- Tous les viewports sont décalés de `-scroll_offset_x` avant rendu
- Les panes hors écran sont naturellement clippées par Metal
- Les séparateurs suivent le même décalage

## Hit-testing

Clic souris, drag de séparateurs : ajouter `scroll_offset_x` aux coordonnées écran pour retrouver la position dans l'espace virtuel.

## Indicateur visuel

Global status bar : affiche le nombre de panes cachés à gauche/droite quand le scroll est actif (ex: `⟵ 2 | 3 ⟶`).

## Session

Le champ `root: bool` est sauvegardé/restauré dans `SavedTree` avec `#[serde(default)]` pour la backward compat (anciens fichiers session sans `root` → `false`).

## Config

```toml
[splits]
min_width = 300.0
```

## Fichiers impactés

- `config.rs` — `SplitsConfig { min_width: f32 }`
- `pane.rs` — `SplitTree` : `root: bool`, `chain_count()`, `equalize()`. `Tab` : `scroll_offset_x`, `virtual_width_override`, `virtual_width()`, `clamp_scroll()`, `scroll_to_reveal()`, `scale_virtual_width()`
- `window.rs` — `do_split` (root: false), `do_split_root` (root: true), `adjust_virtual_width()`, Ctrl+Option handler dans `keyDown`, layout, hit-testing, scroll events
- `session.rs` — `SavedTree` : champ `root` avec serde default
- `renderer/mod.rs` — global status bar avec indicateur de scroll

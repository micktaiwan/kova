# Horizontal scroll pour les splits

## Concept

Chaque tab maintient une `virtual_width` qui représente la largeur de l'espace dans lequel les splits se distribuent. Par défaut c'est la largeur de l'écran. Quand `virtual_width > screen_width`, le scroll horizontal s'active.

## Mécanique

- `virtual_width` : **méthode calculée** sur `Tab` — `max(screen_width, leaf_count * min_split_width)`. Pas de champ stocké, la valeur dérive directement de l'arbre de splits.
- `scroll_offset_x` : `f32`, par tab, initialisé à 0
- `min_split_width` : configurable (section `[splits]`, champ `min_width`), défaut 300.0 px

### Cmd+E (root split)

Ajoute une colonne à l'arbre. `virtual_width()` augmente automatiquement car `leaf_count` croît.

### Cmd+D (split local)

Subdivise le pane courant dans son espace alloué. `virtual_width()` peut augmenter si le nombre de colonnes dépasse le seuil.

### Fermeture d'un pane

`virtual_width()` diminue car `leaf_count` décroît. `clamp_scroll()` borne `scroll_offset_x` à `max(0, virtual_width - screen_width)`.

### Resize fenêtre

`scroll_offset_x` clampé à `max(0, virtual_width - screen_width)`.

## Scroll

### Trackpad

Le scroll horizontal natif du trackpad ajuste `scroll_offset_x`, borné entre 0 et `virtual_width - screen_width`.

### Clavier (Cmd+Option+flèches)

La navigation entre panes (déjà implémentée) scroll automatiquement pour révéler le pane focusé. Pas de raccourci dédié au scroll.

## Rendu

- Tous les viewports sont décalés de `-scroll_offset_x` avant rendu
- Les panes hors écran sont naturellement clippées par Metal
- Les séparateurs suivent le même décalage

## Hit-testing

Clic souris, drag de séparateurs : ajouter `scroll_offset_x` aux coordonnées écran pour retrouver la position dans l'espace virtuel.

## Indicateur visuel

Pas de scrollbar. Le nombre de splits hors écran sera affiché dans une future status bar globale.

## Config

```toml
[splits]
min_width = 300.0
```

## Fichiers impactés

- `config.rs` — `SplitsConfig { min_width: f32 }`
- `pane.rs` — `Tab` : `scroll_offset_x`, méthodes `virtual_width()`, `clamp_scroll()`, `scroll_to_reveal()`
- `window.rs` — layout, hit-testing, scroll events, navigation auto-reveal, viewport avec décalage

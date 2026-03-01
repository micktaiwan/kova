# Tab Bar Font Size — Design Notes

## Problème

La tab bar utilise `cell_size()` du renderer (= font size terminal) pour :
- la hauteur de la barre (`cell_h * 2.0`)
- la taille du texte des onglets

Pas de config séparée. On veut pouvoir configurer la taille de fonte des tabs indépendamment, par fenêtre.

## État actuel

- `TabBarConfig` ne contient que des couleurs (`bg_color`, `fg_color`, `active_bg`)
- `FontConfig` a un seul `size` global pour le terminal
- `tab_bar_height()` dans `window.rs:913` dépend de `renderer.cell_size()`
- Le renderer dessine les tabs via `build_tab_bar_vertices()` avec la même font que le terminal

## Options

### A — `font_size` dans `TabBarConfig` (config globale TOML)

Ajouter un champ `font_size: f64` dans `TabBarConfig`.

```toml
[tab_bar]
font_size = 13.0
```

- Le renderer maintient des metrics séparées pour la tab bar (ascent, descent, cell_h dédié)
- `tab_bar_height()` utilise ces metrics au lieu de `cell_size()`
- Simple, une seule valeur pour toutes les fenêtres
- Pas besoin de stocker d'état par fenêtre

### B — Override par fenêtre (runtime)

Chaque `KovaWindow` stocke un `tab_font_size: Option<f64>` qui override la config globale.

- Raccourcis dédiés (ex: `Cmd+Shift+=` / `Cmd+Shift+-`) pour ajuster par fenêtre
- Pas persisté entre sessions (volatile)
- Plus complexe : chaque fenêtre peut avoir des metrics de tab bar différentes

### C — Ratio relatif à la font terminal

Un `tab_font_ratio: f64` (ex: `0.8` = 80% de la taille terminal) dans la config.

```toml
[tab_bar]
font_ratio = 0.8
```

- Les tabs scalent automatiquement quand on zoom le terminal
- Comportement naturel si on change la font size avec Cmd+/-
- Moins de contrôle absolu

## Points techniques

### Atlas de glyphes

L'atlas est indexé par (glyph_id, font_size). Si la tab bar a une taille différente du terminal, les glyphes de tab seront des entrées séparées dans l'atlas. Pas de problème architectural, juste un peu plus de mémoire GPU.

### Recalcul du layout

Changer la font size des tabs modifie `tab_bar_height()`, ce qui impacte le rect du contenu terminal (`content_rect()`). Il faut :
1. Recalculer le layout
2. Redimensionner les PTY si les dimensions en cellules changent
3. Redessiner

C'est le même chemin que le resize de fenêtre, déjà géré.

### Implémentation suggérée (option A)

1. Ajouter `font_size: Option<f64>` dans `TabBarConfig` (None = même que terminal)
2. Dans le renderer, calculer `tab_cell_h` séparément si configuré
3. `tab_bar_height()` utilise `tab_cell_h` au lieu de `cell_h`
4. `build_tab_bar_vertices()` utilise les metrics tab pour positionner le texte

## Décision

À prendre. Option A est le point de départ naturel — on peut toujours ajouter l'override par fenêtre (B) par-dessus plus tard.

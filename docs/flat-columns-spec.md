# Flat Columns Spec

Refactor du système de splits horizontaux : remplacer l'arbre binaire imbriqué par une liste plate de colonnes. Les splits verticaux restent des arbres binaires à l'intérieur de chaque colonne.

**Motivation** : l'imbrication `[[A | B] | C]` crée un modèle mental confus (root vs local, ratios imbriqués, equalize partiel). L'utilisateur pense `[A | B | C]` — le code doit refléter ça.

## Nouveau data model

### Tab

```rust
pub struct Tab {
    pub columns: Vec<ColumnTree>,   // flat list, always >= 1
    pub column_weights: Vec<f32>,   // one per column, proportional (not normalized)
    // ... existing fields preserved:
    pub focused_pane: PaneId,
    pub virtual_width_override: f32,
    pub scroll_offset_x: f32,
    pub custom_name: Option<String>,
    pub minimized_stack: Vec<PaneId>,
    // ...
}
```

Largeur effective de la colonne `i` :

```
column_width[i] = total_width * (column_weights[i] / sum(column_weights))
```

### ColumnTree

```rust
pub enum ColumnTree {
    Leaf(Pane),
    VSplit {
        top: Box<ColumnTree>,
        bottom: Box<ColumnTree>,
        ratio: f32,           // fraction of height for top child (0.0–1.0)
        custom_ratio: bool,   // user-adjusted ratio
    },
}
```

Plus de `HSplit`. Plus de flag `root`. Plus de `custom_ratio` horizontal (les poids des colonnes sont toujours ajustables).

### Ce qui disparaît

- `SplitTree::HSplit` — remplacé par `Tab.columns` + `column_weights`
- `SplitTree::root` flag — plus nécessaire, toutes les colonnes sont au même niveau
- `SplitTree::custom_ratio` sur les HSplit — remplacé par les poids du Vec
- `chain_count(horizontal, root_only)` — remplacé par `columns.len()`
- `equalize()` dans sa forme actuelle — remplacé par un reset des poids
- `with_split()` pour les splits horizontaux — remplacé par insertion dans le Vec

## Opérations

### Créer un split horizontal

**Cmd+D** — insère une nouvelle colonne **juste après** la colonne du pane focusé.

```
[A | B], focus A → Cmd+D → [A | NEW | B]
[A | B], focus B → Cmd+D → [A | B | NEW]
```

**Cmd+E** — ajoute une nouvelle colonne **à la fin**.

```
[A | B], focus A → Cmd+E → [A | B | NEW]
```

Après insertion : la nouvelle colonne reçoit un poids = `sum(existing_weights) / n_existing` (= la moyenne). Les colonnes existantes gardent leurs poids, donc leurs proportions relatives sont préservées. La nouvelle colonne obtient exactement `1/(n+1)` de l'espace total.

### Créer un split vertical

**Cmd+Shift+D** — split vertical du pane focusé : insère un nouveau pane **juste en dessous** du pane focusé. Le pane focusé est remplacé par un `VSplit { old, new, ratio: 0.5 }`.

```
[A | B], focus A → Cmd+Shift+D →  [ A  | B ]
                                   [NEW |   ]
```

Si la colonne a déjà un VSplit et que le focus est sur un pane interne, le split s'insère après ce pane :

```
[ A  | B ]                         [ A  | B ]
[ C  |   ], focus A → Cmd+Shift+D → [NEW |   ]
                                   [ C  |   ]
```

**Cmd+Shift+E** — split vertical **en bas de la colonne** : wrape la colonne entière dans un VSplit avec le nouveau pane en bas.

```
[ A  | B ]                         [ A  | B ]
[ C  |   ], focus A → Cmd+Shift+E → [ C  |   ]
                                   [NEW |   ]
```

Même logique que Cmd+D/E pour l'horizontal : D = après le pane courant, E = à la fin.

### Supprimer un pane

Quand un pane est fermé :

1. **Pane seul dans sa colonne** (ColumnTree::Leaf) → retirer la colonne du Vec, retirer le poids correspondant. Redistribuer le poids retiré proportionnellement aux colonnes restantes.
2. **Pane dans un VSplit** → le frère remplace le VSplit (promotion). La colonne reste.
3. **Dernière colonne du tab** → fermer le tab.

### Equalize (nouveau raccourci)

Reset tous les `column_weights` à `1.0`. Optionnel : aussi reset les `ratio` des VSplits internes et mettre `custom_ratio = false`.

Raccourci proposé : `Cmd+Shift+=` (cohérent avec tmux `select-layout even-horizontal`).

### Virtual width

```
auto_virtual_width = max(columns.len() * min_split_width, screen_width)
```

Si `virtual_width_override > 0` :
```
virtual_width = max(virtual_width_override, screen_width)
```

Plus besoin de `root_only` dans le calcul — `columns.len()` suffit.

## Resize

### Mode 1 — Column resize (Cmd+Ctrl+Left/Right)

Déplace le séparateur entre deux colonnes adjacentes.

- **Cmd+Ctrl+Right** avec focus dans colonne `i` : cherche le séparateur à droite (entre `i` et `i+1`). Transfère du poids de `i+1` vers `i` (colonne `i` grandit).
- **Cmd+Ctrl+Left** : inverse.
- Si pas de séparateur dans cette direction : cherche l'autre côté (colonne `i` rétrécit).
- Poids minimum par colonne : correspond à `min_split_width` en pixels.

### Mode 1 — Vertical resize (Cmd+Ctrl+Up/Down)

Inchangé : ajuste le `ratio` du VSplit ancêtre dans la colonne. Même logique que l'actuel `adjust_ratio_directional` mais limité aux VSplits.

### Mode 2 — Virtual width global (Ctrl+Option+Left/Right)

Inchangé : ajuste `virtual_width_override`. Tous les poids restent identiques, les colonnes grossissent/rétrécissent proportionnellement.

### Mode 3 — Edge grow (Cmd+Ctrl+Option+Left/Right)

Grow/shrink d'une seule colonne. Les autres gardent leur taille pixel.

- Augmente `virtual_width_override`.
- Ajuste le poids de la colonne cible pour qu'elle absorbe l'espace ajouté.
- Les autres poids sont ajustés pour que leurs colonnes gardent la même largeur pixel.

### Mouse drag

- Hit-test sur les séparateurs verticaux (entre colonnes) et horizontaux (dans les VSplits).
- Drag vertical separator entre colonnes `i` et `i+1` : ajuste `column_weights[i]` et `column_weights[i+1]`.
- Drag horizontal separator : ajuste le `ratio` du VSplit concerné (inchangé).

### Max pane width invariant

Inchangé : aucun pane ne doit dépasser `screen_width` en largeur. Post-validation après chaque resize.

## Navigation

### Left/Right

Trouver la colonne `i` du pane focusé. Aller à la colonne `i-1` (Left) ou `i+1` (Right). Dans la colonne cible, choisir le pane le plus proche verticalement (par centre de viewport, comme actuellement).

### Up/Down

Naviguer dans le VSplit de la colonne courante. Si déjà au top/bottom, rester (pas de wrap).

### Swap (Cmd+Shift+Arrows)

- **Left/Right** : swap la colonne entière `i` avec la colonne `i±1` (swap dans le Vec + swap les poids).
- **Up/Down** : swap les panes dans le VSplit de la colonne (inchangé).

### Reparent (Cmd+Ctrl+Shift+Arrows)

- **Left/Right** : déplacer le pane focusé dans la colonne adjacente (en VSplit). Si la colonne source se retrouve vide, la retirer.
- **Up/Down** : déplacer le pane dans le VSplit voisin (inchangé).

## Séparateurs visuels

- Séparateurs verticaux : entre chaque colonne. Position = somme des largeurs des colonnes 0..i.
- Séparateurs horizontaux : dans les VSplits de chaque colonne (inchangé).

`collect_separators()` et `collect_separator_info()` itèrent sur les colonnes puis descendent dans chaque ColumnTree pour les séparateurs horizontaux.

## Session persistence

### SavedTree → SavedTab

```rust
enum SavedColumn {
    Leaf { cwd, env, ... },
    VSplit { top: Box<SavedColumn>, bottom: Box<SavedColumn>, ratio: f32, custom_ratio: bool },
}

struct SavedTab {
    columns: Vec<SavedColumn>,
    column_weights: Vec<f32>,
    focused_leaf_index: usize,
    // ...
}
```

### Migration

À la restauration, si l'ancien format `SavedTree` est détecté (présence de `HSplit`), le convertir en aplatissant tous les HSplits en colonnes. Les ratios binaires sont convertis en poids proportionnels.

## Fichiers impactés

| Fichier | Impact |
|---|---|
| `src/pane.rs` | **Majeur** — `SplitTree` → `ColumnTree`, refonte des ~30 méthodes |
| `src/window.rs` | **Majeur** — do_split, resize, navigation, mouse drag |
| `src/session.rs` | **Moyen** — SavedTree → SavedTab, migration ancien format |
| `src/renderer/mod.rs` | **Moyen** — viewport computation via `for_each_pane_with_viewport` |
| `src/config.rs` | **Mineur** — raccourcis (ajout equalize, fusion vsplit/vsplit_root) |
| `src/keybindings.rs` | **Mineur** — ajout equalize, suppression vsplit_root si fusionné |
| `src/recent_projects.rs` | **Mineur** — utilise SavedTree |
| `docs/split-specs.md` | Réécriture complète |

## Plan d'implémentation

### Étape 1 — ColumnTree + Tab

Nouveau enum `ColumnTree` (VSplit + Leaf). Ajouter `columns` et `column_weights` à `Tab`. Migrer les méthodes une par une : d'abord les traversals simples (`pane()`, `pane_mut()`, `for_each_pane()`, `contains()`, `first_pane()`, `last_pane()`), puis les opérations (split, remove, viewport, navigate).

### Étape 2 — Split & Remove

Réécrire `do_split` (Cmd+D → insert colonne), `do_split_root` (Cmd+E → push colonne), `do_close_pane_or_tab` avec la logique flat. Supprimer `with_split()` pour les horizontaux.

### Étape 3 — Viewport & Render

Adapter `for_each_pane_with_viewport` : itérer sur les colonnes avec les poids, puis descendre dans chaque ColumnTree pour les VSplits. Adapter le renderer.

### Étape 4 — Resize

Réécrire les 3 modes pour le modèle flat. Le mode 1 horizontal devient un ajustement de poids entre colonnes adjacentes. Les modes 2 et 3 s'adaptent avec les poids au lieu des ratios récursifs.

### Étape 5 — Navigation, Swap, Reparent

Simplifier avec l'index de colonne dans le Vec.

### Étape 6 — Session persistence

Nouveau format `SavedTab` + migration de l'ancien `SavedTree`.

### Étape 7 — Equalize shortcut

Ajouter le raccourci et l'action (trivial à ce stade).

### Étape 8 — Cleanup

Supprimer le code mort (`SplitTree::HSplit`, `root`, `chain_count`, ancien `equalize`). Mettre à jour `docs/split-specs.md`.

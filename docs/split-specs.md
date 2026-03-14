# Split System Specs

## Data Model

### Tab (colonnes plates)

Chaque tab possède une liste plate de colonnes. Plus d'imbrication horizontale.

```rust
pub struct Tab {
    pub columns: Vec<ColumnTree>,     // >= 1, liste plate
    pub column_weights: Vec<f32>,     // un poids par colonne (proportionnel, pas normalisé)
    pub focused_pane: PaneId,
    pub virtual_width_override: f32,
    pub scroll_offset_x: f32,
    // ...
}
```

Largeur effective de la colonne `i` :

```
column_width[i] = total_width * (column_weights[i] / sum(column_weights))
```

### ColumnTree (arbre vertical)

```rust
pub enum ColumnTree {
    Leaf(Pane),
    VSplit {
        top: Box<ColumnTree>,
        bottom: Box<ColumnTree>,
        ratio: f32,           // fraction de hauteur pour top (0.0–1.0)
        custom_ratio: bool,   // ajusté manuellement par l'utilisateur
    },
}
```

- Plus de `HSplit` — la dimension horizontale est gérée par `Tab.columns`.
- Plus de flag `root` — toutes les colonnes sont au même niveau.
- `custom_ratio` n'existe que sur les VSplits (vertical). Les poids des colonnes sont toujours ajustables.

### Viewport computation

Le viewport est calculé en deux niveaux :

1. **Colonnes** : itérer sur `columns` + `column_weights`, allouer la largeur de chaque colonne.
2. **Dans chaque colonne** : descendre récursivement dans le `ColumnTree` pour distribuer la hauteur via les `ratio` des VSplits.

Exception : panes minimisés → collapsent à `MINIMIZED_BAR_PX` (24px).

## Créer des splits

### Split horizontal — ajout de colonne

**Cmd+D** — insère une nouvelle colonne **juste après** la colonne du pane focusé.

```
[A | B], focus A → Cmd+D → [A | NEW | B]
[A | B], focus B → Cmd+D → [A | B | NEW]
```

**Cmd+E** — ajoute une nouvelle colonne **à la fin**.

```
[A | B], focus A → Cmd+E → [A | B | NEW]
```

Poids de la nouvelle colonne = `sum(existing_weights) / n_existing` (la moyenne). Les colonnes existantes gardent leurs poids — leurs proportions relatives sont préservées. La nouvelle colonne obtient `1/(n+1)` de l'espace total.

Le nouveau pane hérite du CWD du pane focusé (via `proc_pidinfo`).

### Split vertical — subdivision dans une colonne

**Cmd+Shift+D** — insère un nouveau pane **juste en dessous** du pane focusé dans sa colonne. Le pane focusé est remplacé par un `VSplit { old, new, ratio: 0.5 }`.

```
[A | B], focus A → Cmd+Shift+D →  [ A  | B ]
                                   [NEW |   ]
```

Si la colonne a déjà un VSplit et que le focus est sur un pane interne, le split s'insère après ce pane.

**Cmd+Shift+E** — split vertical **en bas de la colonne** : wrape la colonne entière dans un `VSplit { column, new, ratio }`.

```
[ A  | B ]                         [ A  | B ]
[ C  |   ], focus A → Cmd+Shift+E → [ C  |   ]
                                   [NEW |   ]
```

Même logique que Cmd+D/E pour l'horizontal : **D = après le pane courant, E = à la fin**.

## Supprimer un pane

Quand un pane est fermé (`exit` / `Cmd+W`) :

1. **Pane seul dans sa colonne** (ColumnTree::Leaf) → retirer la colonne et son poids du Vec. Le poids retiré est redistribué proportionnellement aux colonnes restantes.
2. **Pane dans un VSplit** → le frère remplace le VSplit (promotion). La colonne reste, son poids est inchangé.
3. **Dernière colonne du tab** → fermer le tab. Dernier tab → fermer la fenêtre.

Focus après suppression : pane suivant (même logique qu'actuellement).

## Resizing

Trois modes distincts, jamais mélangés.

### Mode 1 — Column/ratio resize (Cmd+Ctrl+Arrows)

**Horizontal (Cmd+Ctrl+Left/Right)** — déplace le séparateur entre colonnes adjacentes.

| Raccourci | Action |
|---|---|
| `Cmd+Ctrl+Right` | Séparateur à droite de la colonne `i` : transfère du poids de `i+1` vers `i` (colonne grandit) |
| `Cmd+Ctrl+Left` | Inverse |

- Si pas de séparateur dans la direction demandée : fallback sur l'autre côté (colonne rétrécit).
- Poids minimum par colonne : correspond à `min_split_width` en pixels.

**Vertical (Cmd+Ctrl+Up/Down)** — ajuste le `ratio` du VSplit ancêtre du pane focusé dans sa colonne.

- Clamp ratio à `[0.1, 0.9]`.
- Set `custom_ratio = true`.
- Bloqué si un enfant est complètement minimisé.

### Mode 2 — Virtual width global (Ctrl+Option+Left/Right)

Change `virtual_width_override` pour tout le tab. Tous les poids restent identiques — les colonnes grossissent/rétrécissent proportionnellement.

| Raccourci | Action |
|---|---|
| `Ctrl+Option+Right` | Augmente virtual width de 200pt × scale |
| `Ctrl+Option+Left` | Diminue (min = screen width, puis override reset à 0) |

Le scroll horizontal s'active quand `virtual_width > screen_width`. Auto-scroll pour garder le pane focusé visible.

### Mode 3 — Edge grow (Cmd+Ctrl+Option+Left/Right)

Grow/shrink d'une seule colonne. Les autres gardent leur taille pixel.

| Raccourci | Action |
|---|---|
| `Cmd+Ctrl+Option+Right` | Colonne focusée grandit : virtual width augmente, poids ajustés pour que les autres colonnes gardent leur largeur pixel |
| `Cmd+Ctrl+Option+Left` | Inverse. No-op si `virtual_width_override` est déjà 0 |

Seul l'axe horizontal pour l'instant.

### Equalize (Cmd+Shift+=)

Reset tous les `column_weights` à `1.0` (toutes les colonnes de même largeur). Reset aussi les `ratio` des VSplits et `custom_ratio = false` dans toutes les colonnes.

### Status bar feedback

Sur tout resize (3 modes) : la status bar affiche le mode actif, screen width et virtual width. Disparaît après 2 secondes.

### Mouse drag

**Hit-testing** : séparateurs verticaux (entre colonnes) et horizontaux (dans les VSplits).

**Drag séparateur vertical** entre colonnes `i` et `i+1` : ajuste `column_weights[i]` et `column_weights[i+1]` proportionnellement au delta pixel.

**Drag séparateur horizontal** dans un VSplit : ajuste le `ratio` du VSplit. Clamp à `[0.1, 0.9]`, set `custom_ratio = true`.

### Max pane width invariant

**Invariant** : aucun pane ne doit dépasser `screen_width` en largeur pixel. Vérifié après chaque resize via post-validation.

- **Mode 1** : les poids utilisateur sont préservés. Si un pane dépasse, `virtual_width_override` est réduit.
- **Modes 2 & 3** : les poids sont ajustés d'abord pour capper les colonnes trop larges. Si ça ne suffit pas, `virtual_width_override` est réduit en dernier recours.

## Navigation

### Left/Right (Cmd+Option+Left/Right)

Trouver la colonne `i` du pane focusé. Aller à la colonne `i-1` (Left) ou `i+1` (Right). Dans la colonne cible, choisir le pane le plus proche verticalement (par centre de viewport).

### Up/Down (Cmd+Option+Up/Down)

Naviguer dans le VSplit de la colonne courante. Si déjà au top/bottom, rester.

### Swap (Cmd+Shift+Arrows)

- **Left/Right** : swap la colonne entière `i` avec la colonne `i±1` (swap éléments dans le Vec + swap les poids).
- **Up/Down** : swap les panes dans le VSplit de la colonne.

### Reparent (Cmd+Ctrl+Shift+Arrows)

- **Left/Right** : déplacer le pane focusé dans la colonne adjacente (ajouté en VSplit en bas de la colonne cible). Si la colonne source se retrouve vide, la retirer.
- **Up/Down** : déplacer le pane dans le VSplit voisin de la même colonne.

## Séparateurs visuels

- **Séparateurs verticaux** (1px) : entre chaque colonne. Position = somme des largeurs des colonnes 0..i.
- **Séparateurs horizontaux** (1px) : dans les VSplits de chaque colonne.

## Virtual width

```
auto_virtual_width = max(columns.len() * min_split_width, screen_width)
```

Si `virtual_width_override > 0` :
```
virtual_width = max(virtual_width_override, screen_width)
```

## Session persistence

### Format de sauvegarde

```rust
enum SavedColumn {
    Leaf { cwd, env, ... },
    VSplit { top: Box<SavedColumn>, bottom: Box<SavedColumn>, ratio: f32, custom_ratio: bool },
}

struct SavedTab {
    columns: Vec<SavedColumn>,
    column_weights: Vec<f32>,
    focused_leaf_index: usize,
    virtual_width_override: f32,
    scroll_offset_x: f32,
    // ...
}
```

### Migration ancien format

À la restauration, si l'ancien format `SavedTree` est détecté (présence de `HSplit`), le convertir en aplatissant tous les HSplits en colonnes. Les ratios binaires sont convertis en poids proportionnels.

Algorithme de conversion : parcours récursif de l'ancien arbre. À chaque `HSplit`, les deux sous-arbres deviennent des entrées séparées dans le Vec de colonnes. Le ratio binaire est converti en poids : `left_weight = ratio`, `right_weight = 1.0 - ratio`. Les `VSplit` sont préservés tels quels à l'intérieur des colonnes.

## Configuration

```toml
[splits]
min_width = 300.0   # Largeur minimum d'une colonne en points

[keys]
# Colonnes
vsplit = "cmd+d"             # Nouvelle colonne après le pane focusé
vsplit_root = "cmd+e"        # Nouvelle colonne à la fin
# Vertical
hsplit = "cmd+shift+d"       # Split vertical après le pane focusé
hsplit_root = "cmd+shift+e"  # Split vertical en bas de la colonne
# Resize mode 1
resize_left = "cmd+ctrl+left"
resize_right = "cmd+ctrl+right"
resize_up = "cmd+ctrl+up"
resize_down = "cmd+ctrl+down"
# Resize mode 3 (edge grow)
edge_grow_left = "cmd+ctrl+option+left"
edge_grow_right = "cmd+ctrl+option+right"
# Resize mode 2 (virtual width) : Ctrl+Option+Arrows (hardcoded in keyDown)
# Equalize
equalize = "cmd+shift+="
```

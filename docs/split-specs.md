# Split System Specs

## Data Model

### Tab (grille plate)

Chaque tab possède une grille plate à deux niveaux : colonnes et lignes. Plus d'imbrication.

```rust
pub struct Tab {
    pub columns: Vec<Column>,         // >= 1, liste plate de colonnes
    pub column_weights: Vec<f32>,     // un poids par colonne (proportionnel, pas normalisé)
    pub custom_weights: Vec<bool>,    // true = colonne redimensionnée manuellement ("pinnée")
    pub focused_pane: PaneId,
    pub virtual_width_override: f32,
    pub scroll_offset_x: f32,
    // ...
}
```

### Column (lignes plates)

```rust
pub struct Column {
    pub panes: Vec<Pane>,             // >= 1, liste plate de panes (top → bottom)
    pub row_weights: Vec<f32>,        // un poids par ligne (proportionnel, pas normalisé)
    pub custom_row_weights: Vec<bool>,// true = ligne redimensionnée manuellement ("pinnée")
}
```

Largeur effective de la colonne `i` :

```
column_width[i] = total_width * (column_weights[i] / sum(column_weights))
```

Hauteur effective du pane `j` dans la colonne `i` :

```
row_height[j] = column_height * (row_weights[j] / sum(row_weights))
```

Exception : panes minimisés → collapsent à `MINIMIZED_BAR_PX` (24px). Les panes non-minimisés se partagent la hauteur restante proportionnellement.

- Plus de `HSplit` ni de `VSplit` — les deux axes sont des listes plates.
- `custom_weights` (colonnes) et `custom_row_weights` (lignes) indiquent un redimensionnement manuel. Voir section "Colonnes/lignes pinnées".

### Viewport computation

Le viewport est calculé en deux niveaux :

1. **Colonnes** : itérer sur `columns` + `column_weights`, allouer la largeur de chaque colonne.
2. **Dans chaque colonne** : itérer sur `panes` + `row_weights`, allouer la hauteur de chaque pane.

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

### Split vertical — ajout de ligne dans une colonne

**Cmd+Shift+D** — insère un nouveau pane **juste en dessous** du pane focusé dans sa colonne.

```
[A | B], focus A → Cmd+Shift+D →  [ A  | B ]
                                   [NEW |   ]
```

**Cmd+Shift+E** — ajoute un nouveau pane **en bas de la colonne**.

```
[ A  | B ]                         [ A  | B ]
[ C  |   ], focus A → Cmd+Shift+E → [ C  |   ]
                                   [NEW |   ]
```

Même logique que Cmd+D/E pour l'horizontal : **D = après le pane courant, E = à la fin**.

Poids de la nouvelle ligne = `sum(existing_row_weights) / n_existing` (la moyenne). Les lignes existantes gardent leurs poids.

## Supprimer un pane

Quand un pane est fermé (`exit` / `Cmd+W`) :

1. **Pane seul dans sa colonne** (1 pane, 1 colonne) → retirer la colonne et son poids du Vec. Le poids retiré est redistribué proportionnellement aux colonnes restantes.
2. **Pane dans une colonne multi-lignes** → retirer le pane et son `row_weight` du Vec. Le poids retiré est redistribué proportionnellement aux lignes restantes de la colonne.
3. **Dernier pane de la dernière colonne** → fermer le tab. Dernier tab → fermer la fenêtre.

Focus après suppression : pane suivant (même logique qu'actuellement).

## Resizing

Trois modes distincts, jamais mélangés.

### Mode 1 — Column/ratio resize (Cmd+Ctrl+Arrows)

**Horizontal (Cmd+Ctrl+Left/Right)** — déplace le bord contrôlé de la colonne focusée.

Le **bord contrôlé** est le bord droit de la colonne, sauf pour la dernière colonne (bord gauche).

| Position | `Cmd+Ctrl+Right` | `Cmd+Ctrl+Left` |
|---|---|---|
| Première ou milieu | Bord droit → : colonne grandit | Bord droit ← : colonne rétrécit |
| Dernière | Bord gauche → : colonne rétrécit | Bord gauche ← : colonne grandit |

- Poids minimum par colonne : correspond à `min_split_width` en pixels.
- Pas de fallback : tous les cas sont couverts par le bord contrôlé.

**Vertical (Cmd+Ctrl+Up/Down)** — déplace le bord contrôlé du pane focusé dans sa colonne.

Le **bord contrôlé** est le bord bas du pane, sauf pour le dernier pane de la colonne (bord haut). Même logique que l'horizontal.

| Position | `Cmd+Ctrl+Down` | `Cmd+Ctrl+Up` |
|---|---|---|
| Premier ou milieu | Bord bas ↓ : pane grandit | Bord bas ↑ : pane rétrécit |
| Dernier | Bord haut ↓ : pane rétrécit | Bord haut ↑ : pane grandit |

- Poids minimum par ligne : 5% du total.
- Bloqué si un pane voisin est complètement minimisé.

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

Reset tous les `column_weights` à `1.0` et `custom_weights` à `false` (toutes les colonnes de même largeur, aucune pinnée). Reset aussi tous les `row_weights` à `1.0` et `custom_row_weights` à `false` dans toutes les colonnes.

### Status bar feedback

Sur tout resize (3 modes) : la status bar affiche le mode actif, screen width et virtual width. Disparaît après 2 secondes.

### Colonnes/lignes pinnées (`custom_weight` / `custom_row_weight`)

Une colonne (ou ligne) avec `custom_weight = true` a été redimensionnée manuellement. Elle conserve son poids lors des redistributions automatiques. Les règles sont identiques pour les deux axes.

**Quand une colonne/ligne devient pinnée :**

- Drag souris d'un séparateur → seule la colonne/ligne **du côté direct** (celle qu'on pousse/tire explicitement) devient pinnée.
- Resize clavier mode 1 → la **colonne/ligne focusée** devient pinnée (c'est elle que l'utilisateur redimensionne intentionnellement). Les autres qui s'adaptent ne changent pas de statut.

**Quand une colonne/ligne est dépinnée :**

- Equalize (Cmd+Shift+=) → tout reset à `false`.
- Nouvelle colonne/ligne créée → la nouvelle a `custom_weight = false`. Les existantes gardent leur état.

**Persistance :** `custom_weight` et `custom_row_weight` sont sauvegardés/restaurés avec la session.

### Redistribution (colonnes et lignes)

L'algorithme de redistribution est **identique** pour les deux axes (colonnes horizontalement, lignes verticalement). Dans cette section, "élément" désigne une colonne ou une ligne selon l'axe.

#### Algorithme (drag souris)

Drag du séparateur entre éléments `i` et `i+1`, delta en pixels :

1. **Élément poussé** : l'élément du côté vers lequel le séparateur se déplace. Son poids diminue du delta. Il devient pinné.

2. **Côté libre** : l'espace est redistribué entre les éléments non-pinnés de l'autre côté.
   - Les non-pinnés reçoivent une part égale.
   - Les pinnés gardent leur poids inchangé.
   - Aucun ne change de statut pinné.

3. **Fallback** : si **tous** les éléments du côté libre sont pinnés, seul l'adjacent absorbe.

4. **Minimum** : aucun élément ne peut descendre en dessous de 5% du poids total.

#### Algorithme (clavier mode 1)

Le **bord contrôlé** est le bord extérieur de l'élément focusé (droit pour les colonnes, bas pour les lignes), sauf pour le **dernier** élément (bord gauche / haut).

**Grandir** (pousser le bord vers l'extérieur) :
- L'élément focusé grandit et devient pinné.
- La perte est redistribuée parmi les non-pinnés du côté extérieur.

**Rétrécir** (tirer le bord vers l'intérieur) :
- L'élément focusé rétrécit (reste pinné s'il l'était).
- Le gain est redistribué parmi les non-pinnés du côté extérieur.

#### Exemples (horizontaux)

- `A | B | C`, focus B, `Right` : B grandit, C (+ au-delà) cède
- `A | B | C`, focus B, `Left` : B rétrécit, C (+ au-delà) absorbe
- `A | B | C`, focus C (dernière), `Left` : C grandit (bord gauche ←), A et B cèdent

#### Exemples (verticaux)

```
A
B
C
```
- Focus B, `Down` : B grandit, C (+ au-delà) cède
- Focus B, `Up` : B rétrécit, C (+ au-delà) absorbe
- Focus C (dernière), `Up` : C grandit (bord haut ↑), A et B cèdent

### Mouse drag

**Curseur souris** : au survol d'un séparateur (±3px autour de la ligne de 1px), le curseur change :
- Séparateur vertical (entre colonnes) → `resizeLeftRight` (↔)
- Séparateur horizontal (entre lignes d'une colonne) → `resizeUpDown` (↕)

Le curseur revient à la flèche par défaut quand la souris quitte la zone de tolérance.

**Hit-testing** : séparateurs verticaux (entre colonnes) et horizontaux (entre lignes d'une colonne).

**Drag séparateur vertical** : voir "Redistribution" ci-dessus (axe colonnes).

**Drag séparateur horizontal** : voir "Redistribution" ci-dessus (axe lignes).

### Max pane width invariant

**Invariant** : aucun pane ne doit dépasser `screen_width` en largeur pixel. Vérifié après chaque resize via post-validation.

- **Mode 1** : les poids utilisateur sont préservés. Si un pane dépasse, `virtual_width_override` est réduit.
- **Modes 2 & 3** : les poids sont ajustés d'abord pour capper les colonnes trop larges. Si ça ne suffit pas, `virtual_width_override` est réduit en dernier recours.

## Navigation

### Left/Right (Cmd+Option+Left/Right)

Trouver la colonne `i` du pane focusé. Aller à la colonne `i-1` (Left) ou `i+1` (Right). Dans la colonne cible, choisir le pane le plus proche verticalement (par centre de viewport).

### Up/Down (Cmd+Option+Up/Down)

Naviguer dans la liste de panes de la colonne courante. Si déjà au premier/dernier, rester.

### Swap (Cmd+Shift+Arrows)

- **Left/Right** : swap la colonne entière `i` avec la colonne `i±1` (swap éléments dans le Vec + swap les poids).
- **Up/Down** : swap le pane focusé avec son voisin dans la colonne (swap dans le Vec + swap les row_weights).

### Reparent (Cmd+Ctrl+Shift+Arrows)

- **Left/Right** : déplacer le pane focusé dans la colonne adjacente (ajouté en bas de la colonne cible). Si la colonne source se retrouve vide, la retirer.
- **Up/Down** : déplacer le pane vers la colonne la plus proche au-dessus/en-dessous (si cross-column vertical layout le permet).

## Séparateurs visuels

- **Séparateurs verticaux** (1px) : entre chaque colonne. Position = somme des largeurs des colonnes 0..i.
- **Séparateurs horizontaux** (1px) : entre chaque pane d'une colonne multi-lignes.

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
struct SavedPane {
    cwd: String,
    env: HashMap<String, String>,
    // ...
}

struct SavedColumn {
    panes: Vec<SavedPane>,
    row_weights: Vec<f32>,
    custom_row_weights: Vec<bool>,
}

struct SavedTab {
    columns: Vec<SavedColumn>,
    column_weights: Vec<f32>,
    custom_weights: Vec<bool>,
    focused_leaf_index: usize,
    virtual_width_override: f32,
    scroll_offset_x: f32,
    // ...
}
```

### Migration ancien format

Deux anciens formats à supporter :

1. **v2 — `SavedTree` (HSplit/VSplit)** : aplatir les HSplits en colonnes, aplatir les VSplits en lignes. Ratios binaires → poids proportionnels.

2. **v3 — `SavedColumn` (enum Leaf/VSplit)** : aplatir les VSplits en lignes. Les colonnes sont déjà plates. Ratios binaires → poids proportionnels. `custom_ratio` → `custom_row_weight`.

Les deux migrations produisent le format v4 (listes plates colonnes + lignes).

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

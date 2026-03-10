# Inline Image Support (Kitty Graphics Protocol)

Design doc pour l'ajout du support d'images inline dans Kova via le Kitty Graphics Protocol.

## Protocole choisi : Kitty Graphics

Pourquoi Kitty plutôt que Sixel ou iTerm2 :

- **Kitty** : protocole moderne, bien documenté, séparation image/placement, chunked transfer, z-index, animations. Adopté par Ghostty, WezTerm, Konsole, foot.
- **Sixel** : ancien (1983), complexe à parser (state machine propre), pas de cache d'image, retransmission à chaque affichage. Rejeté.
- **iTerm2** : propriétaire, pas de cache, pas de z-index, pas de placements multiples. Non retenu.

Spec complète : https://sw.kovidgoyal.net/kitty/graphics-protocol/

## Format des escape sequences

```
ESC _ G <key=value,key=value,...> ; <base64-payload> ESC \
```

- `ESC _` = APC (Application Programming Command)
- `G` = identifiant du protocole graphique
- Payload en base64, séparé par `;`
- `ESC \` = ST (String Terminator)

### Clés principales

| Clé | Rôle | Valeurs |
|-----|------|---------|
| `a` | Action | `T` transmit+display (défaut), `t` transmit, `p` place, `d` delete, `q` query |
| `f` | Format pixels | `24` RGB, `32` RGBA (défaut), `100` PNG |
| `t` | Mode transmission | `d` direct (défaut), `f` fichier, `t` fichier temp, `s` shared memory |
| `i` | Image ID | 1..2^32 |
| `p` | Placement ID | 1..2^32 |
| `s` | Largeur source (pixels) | requis pour RGB/RGBA |
| `v` | Hauteur source (pixels) | requis pour RGB/RGBA |
| `c` | Colonnes d'affichage | integer |
| `r` | Lignes d'affichage | integer |
| `z` | Z-index | integer (négatif = sous le texte) |
| `m` | Chunked transfer | `1` = suite, `0` = dernier chunk |
| `q` | Mode silencieux | `1` = pas d'OK, `2` = aucune réponse |
| `o` | Compression | `z` = zlib deflate |
| `C` | Mouvement curseur | `1` = ne pas bouger le curseur |

### Chunked transfer

Les images sont envoyées par chunks de max 4096 bytes base64 :

```
ESC_G f=100,i=1,m=1; <chunk1> ESC\
ESC_G m=1;            <chunk2> ESC\
ESC_G m=0;            <chunk3> ESC\
```

Seul le premier chunk porte les clés de contrôle. L'image est placée à réception du dernier chunk (`m=0`).

### Réponse du terminal

```
ESC _ G i=<id> ; OK ESC \           -- succès
ESC _ G i=<id> ; EINVAL:message ESC\ -- erreur
```

Codes d'erreur : `ENOENT`, `EINVAL`, `ENOSPC`, `ETOODEEP`, `ECYCLE`, `ENOPARENT`.

### Delete

`a=d` avec clé `d=` pour cibler : `a` (all visible), `i` (par ID), `c` (au curseur), `p` (à position x,y), etc.
Minuscule = supprime le placement. Majuscule = supprime aussi les données image.

### Détection de support

```
ESC_G i=31,s=1,v=1,a=q,t=d,f=24;AAAA ESC\  ESC[c
```

Si le terminal supporte le protocole, il répond avec l'APC avant la réponse DA1.

## Architecture d'implémentation dans Kova

### Vue d'ensemble

```
APC sequence
    │
    ▼
┌──────────┐    ┌──────────────┐    ┌─────────────────┐
│  Parser   │───▶│TerminalState │───▶│  ImageManager   │
│(parser.rs)│    │  (mod.rs)    │    │(image_manager.rs)│
└──────────┘    └──────────────┘    └────────┬────────┘
                                             │
                                    ┌────────▼────────┐
                                    │    Renderer      │
                                    │   (mod.rs)       │
                                    │                  │
                                    │  glyph texture   │
                                    │  image texture ◄─┤
                                    └──────────────────┘
```

### Chantier 1 : Parser APC (`src/terminal/parser.rs`)

Le crate `vte` ne parse pas les séquences APC nativement. Il faut :

1. Intercepter `ESC _` dans le handler VTE (méthode `hook()` pour DCS, ou un handler APC custom)
2. Détecter le préfixe `G` (protocole graphique Kitty)
3. Parser les paires `key=value` (toutes single-letter, valeurs entières ou char)
4. Décoder le payload base64
5. Gérer le chunking : accumuler les chunks (buffer par image ID) jusqu'à `m=0`

**Fichiers impactés** :
- `src/terminal/parser.rs:106-115` — méthode `hook()`, point d'entrée
- `src/terminal/parser.rs:119-181` — `osc_dispatch()` pour référence de pattern

**Nouveau code** :
- Struct `ImageCommand` : action, image ID, format, dimensions, payload, placement params
- Parser de clés K/V (simple, ~50 lignes)
- Buffer de chunks (HashMap<image_id, Vec<u8>>)

### Chantier 2 : Terminal State (`src/terminal/mod.rs`)

**Contrainte critique** : les cellules font 32 bytes et sont optimisées pour la RAM. Ne PAS toucher au struct `Cell`.

Ajouter à `TerminalState` :

```rust
/// Images décodées en mémoire, indexées par image ID
pub image_store: HashMap<u32, StoredImage>,

/// Placements actifs : où afficher quelle image
pub image_placements: Vec<ImagePlacement>,
```

```rust
pub struct StoredImage {
    pub id: u32,
    pub width: u32,          // pixels
    pub height: u32,         // pixels
    pub data: Vec<u8>,       // RGBA décodé
    pub texture_dirty: bool, // besoin d'upload GPU
}

pub struct ImagePlacement {
    pub image_id: u32,
    pub placement_id: u32,
    pub col: u16,            // position grille (top-left)
    pub row: i32,            // peut être négatif (scrollback)
    pub cols: u16,           // taille d'affichage en cellules
    pub rows: u16,
    pub z_index: i32,
    pub source_rect: Option<(u32, u32, u32, u32)>, // x, y, w, h dans l'image source
}
```

**Fichiers impactés** :
- `src/terminal/mod.rs:124-192` — struct `TerminalState`

### Chantier 3 : Image Manager (nouveau module `src/renderer/image_manager.rs`)

Gestion des textures Metal pour les images, séparé de l'atlas de glyphes.

Responsabilités :
- Décoder les PNG (via `image` crate ou CoreGraphics)
- Décompresser zlib si `o=z`
- Uploader les pixels en texture Metal (RGBA8Unorm)
- Cache de textures avec éviction (LRU par taille mémoire)
- Gérer le cycle de vie : upload quand `texture_dirty`, libérer quand image supprimée

```rust
pub struct ImageManager {
    /// Cache: image_id → Metal texture
    textures: HashMap<u32, MTLTexture>,
    /// Mémoire GPU utilisée (pour éviction)
    gpu_memory_used: usize,
    gpu_memory_limit: usize,  // ex: 256 MB
}

impl ImageManager {
    pub fn upload(&mut self, device: &MTLDevice, image: &StoredImage) -> &MTLTexture;
    pub fn get_texture(&self, image_id: u32) -> Option<&MTLTexture>;
    pub fn remove(&mut self, image_id: u32);
    pub fn evict_if_needed(&mut self);
}
```

**Stratégie texture** : une MTLTexture par image (pas d'atlas images). Raisons :
- Les images ont des tailles très variables (contrairement aux glyphes)
- Pas besoin de packing complexe
- Suppression individuelle facile
- Metal gère bien les centaines de textures

### Chantier 4 : Renderer (`src/renderer/mod.rs`)

**Approche** : ajouter une passe d'image entre les backgrounds et les glyphes.

Dans `build_vertices()` (lignes 610-812), après la passe backgrounds (ligne 691) :

```rust
// Pass 2.5: Image quads
for placement in &term.image_placements {
    if !placement_visible(placement, viewport) { continue; }
    let texture = self.image_manager.get_texture(placement.image_id);
    // Générer un quad qui couvre cols×rows cellules
    // tex_coords = [0,0] à [1,1] (texture entière) ou sub-rect
    // Utiliser un marqueur dans color.a pour signaler au shader
    self.push_image_quad(vertices, placement, viewport);
}
```

**Multi-texture Metal** :

Le pipeline Metal actuel n'utilise qu'une texture (l'atlas de glyphes). Pour les images :
- Bind la texture image en slot 1 (l'atlas reste en slot 0)
- Le fragment shader détecte via un marqueur dans le vertex quel slot sampler

Problème : on ne peut pas changer de texture entre les vertices d'un même draw call.

**Solutions** :
1. **Draw call séparé par image** : un draw call pour les glyphes (texture atlas), un par image (sa texture). Simple mais plus de draw calls.
2. **Atlas d'images** : packer les images dans un atlas séparé. Complexe mais un seul draw call.
3. **Texture array** : utiliser une `MTLTextureArray`. Propre mais limité en nombre de layers.

**Recommandation** : option 1 (draw calls séparés). Simple, et on aura rarement plus de ~10 images visibles simultanément. Pas de goulot d'étranglement.

**Fichiers impactés** :
- `src/renderer/mod.rs:610-812` — `build_vertices()`
- `src/renderer/mod.rs:550-598` — soumission GPU (ajouter draw calls images)
- `src/renderer/mod.rs:110-161` — struct `Renderer` (ajouter `ImageManager`)

### Chantier 5 : Shaders (`shaders/terminal.metal`)

Pour les draw calls d'images, on peut réutiliser le même vertex shader. Le fragment shader image est trivial :

```metal
fragment float4 image_fragment(
    VertexOut in [[stage_in]],
    texture2d<float> image [[texture(0)]],
    sampler s [[sampler(0)]])
{
    return image.sample(s, in.tex_coords);
}
```

Alternative : un seul fragment shader avec un flag :

```metal
// Utiliser color.a == 0.5 comme marqueur "c'est une image"
if (in.color.a > 0.4 && in.color.a < 0.6) {
    return atlas.sample(s, in.tex_coords);  // ici atlas = image texture
}
```

**Recommandation** : pipeline séparé (un pour glyphes, un pour images). Plus propre, pas de hacks de marqueur.

**Fichiers impactés** :
- `shaders/terminal.metal` — ajouter `image_fragment` (~5 lignes)
- `src/renderer/pipeline.rs:6-46` — créer un second pipeline state pour images

## Scope MVP (Phase 1)

Support minimal pour que `icat`, `yazi`, et les outils Kitty fonctionnent :

| Feature | Inclus | Exclu |
|---------|--------|-------|
| `a=T` (transmit+display) | oui | |
| `a=t` + `a=p` (transmit puis place) | oui | |
| `a=d` (delete) | oui (par ID, all) | delete par position/z-index |
| `a=q` (query) | oui | |
| `f=100` (PNG) | oui | |
| `f=24/32` (raw RGB/RGBA) | oui | |
| `o=z` (zlib) | oui | |
| `t=d` (direct) | oui | |
| `t=f/t/s` (file/temp/shm) | | phase 2 |
| Chunked transfer (`m=`) | oui | |
| Placement sizing (`c=`, `r=`) | oui | |
| Z-index (`z=`) | | phase 2 |
| Sub-rect source (`x,y,w,h`) | | phase 2 |
| Unicode placeholders (`U=1`) | | phase 2 |
| Animations (`a=f`, `a=a`) | | phase 3 |
| Relative placements (`P,Q,H,V`) | | phase 3 |

## Dépendances Rust

```toml
# Décodage PNG
png = "0.17"        # ou image = "0.25" (plus lourd mais multi-format)

# Décompression zlib
flate2 = "1.0"

# Base64
base64 = "0.22"
```

Alternative : utiliser CoreGraphics (déjà disponible) pour décoder les PNG sans dépendance supplémentaire.

## Considérations

### RAM

- Chaque image stockée en RGBA = `width × height × 4` bytes en CPU + autant en GPU
- Une image 1920×1080 = ~8 MB (CPU) + ~8 MB (GPU)
- Limiter le cache GPU (ex: 256 MB) avec éviction LRU
- Les images dans le scrollback : supprimer les données pixel, garder seulement les métadonnées (ou rien)

### Scrollback

Les placements sont liés à une position grille (row, col). Quand les lignes scrollent :
- Les placements doivent être décalés (row -= 1 à chaque scroll)
- Les placements qui sortent du scrollback sont supprimés
- Phase 2 : les Unicode placeholders résolvent ça plus élégamment

### Sécurité

- Mode fichier (`t=f`) : vérifier que c'est un fichier régulier, pas un device/socket/FIFO
- Refuser les paths sous `/proc`, `/sys`, `/dev`
- Mode temp (`t=t`) : vérifier que le path contient `tty-graphics-protocol` et est dans un répertoire temp connu
- Limiter la taille totale des images en mémoire

## Ordre d'implémentation suggéré

1. **Parser APC** : intercepter les séquences, parser les clés, accumuler les chunks
2. **Image store** : stocker les images décodées dans `TerminalState`
3. **Image manager** : upload en texture Metal
4. **Renderer** : draw call séparé pour les images
5. **Query response** : répondre à `a=q` pour la détection de support
6. **Delete** : supprimer par ID et all
7. **Tests** : vérifier avec `kitten icat image.png`

## Références

- [Kitty Graphics Protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/)
- [Ghostty source (Zig)](https://github.com/ghostty-org/ghostty) — implémentation de référence
- [WezTerm image support](https://wezfurlong.org/wezterm/imgcat.html) — autre implémentation Rust

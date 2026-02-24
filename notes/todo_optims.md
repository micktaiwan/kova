# Kova — Optimisations mémoire

Baseline mesurée : ~116 MB RSS pour un seul pane (2026-02-24).

## Décomposition estimée

| Poste | Estimation | Fichier | Notes |
|-------|-----------|---------|-------|
| Vertex buffers Metal (×2) | **32 MB** | `renderer/mod.rs:47` | `MAX_VERTEX_BYTES = 16 MB` × 2 double-buffered |
| Scrollback (×N panes) | **~22 MB/pane** | `terminal/mod.rs:69` | 10 000 lignes × 80 cols × 28 B/cell |
| Atlas buf CPU + texture GPU | **1–10 MB** | `renderer/glyph_atlas.rs:31` | Double stockage CPU+GPU, croît sans rétrécir |
| Fallback fonts cache | **1–5 MB** | `renderer/glyph_atlas.rs:40` | Un `CTFont` par char unique, jamais purgé |
| Runtime Rust + libs | **5–10 MB** | — | Baseline incompressible |
| Vec temporaires/frame | **~2 MB** | `renderer/mod.rs:446` | `Vec<Vertex>` alloué/libéré chaque frame |

## Optimisations à faire

### 1. Réduire les vertex buffers — gain ~24 MB
- `MAX_VERTEX_BYTES` = 16 MB est très généreux
- Un terminal 200×50 produit ~2.4 MB de vertices max
- **Action** : réduire à 4 MB (×2 = 8 MB au lieu de 32 MB)

### 2. Compacter la struct Cell — gain ~75% sur le scrollback
- Actuellement : `char` (4B) + `fg: [f32; 3]` (12B) + `bg: [f32; 3]` (12B) = **28 bytes**
- Avec palette : `char` (4B) + `fg_idx: u8` + `bg_idx: u8` = **6 bytes**
- Stocker les couleurs comme indices dans une palette globale (256 couleurs ANSI + quelques customs)
- **Gain** : scrollback 10k lignes × 80 cols passe de 22 MB à ~5 MB par pane

### 3. Dropper le atlas_buf CPU après upload — gain variable
- `atlas_buf: Vec<u8>` est gardé en permanence pour les updates partielles
- Alternative : ne garder qu'un petit buffer de travail (1 cell) et recréer la texture GPU complète lors du grow
- Ou utiliser `MTLTexture.getBytes` pour relire la texture si besoin
- **Tradeoff** : complexité vs mémoire

### 4. Limiter le cache de fallback fonts — gain 1–5 MB
- `fallback_fonts: HashMap<char, CFRetained<CTFont>>` croît indéfiniment
- Beaucoup de chars partagent la même police fallback
- **Action** : cacher par font name (pas par char) avec un `HashMap<String, CFRetained<CTFont>>`
- Ou LRU avec cap à ~20 fonts

### 5. Réutiliser le Vec<Vertex> entre frames — gain en GC pressure
- Actuellement un nouveau `Vec` est alloué à chaque `build_vertices`
- **Action** : garder un `Vec<Vertex>` persistant dans le Renderer, `.clear()` à chaque frame
- Pas de gain RSS direct mais réduit la fragmentation mémoire

### 6. Scrollback : compression ou lazy storage
- Les lignes vides/blanches pourraient être stockées comme sentinelles
- Les lignes identiques consécutives pourraient être dédupliquées
- Plus ambitieux : compresser les vieilles lignes de scrollback (zstd)
- **Tradeoff** : complexité significative, à faire seulement si le scrollback est le bottleneck confirmé

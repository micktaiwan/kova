# Kova — Optimisations mémoire

Baseline mesurée : ~116 MB RSS pour un seul pane (2026-02-24).

## Décomposition estimée

| Poste | Estimation | Fichier | Notes |
|-------|-----------|---------|-------|
| Vertex buffers Metal (×2) | **32 MB** | `renderer/mod.rs:47` | `MAX_VERTEX_BYTES = 16 MB` × 2 double-buffered |
| Scrollback (×N panes) | **~25 MB/pane** | `terminal/mod.rs:69` | 10 000 lignes × 80 cols × 32 B/cell (avant trim) |
| Atlas buf CPU + texture GPU | **1–10 MB** | `renderer/glyph_atlas.rs:31` | Double stockage CPU+GPU, croît sans rétrécir |
| Fallback fonts cache | **1–5 MB** | `renderer/glyph_atlas.rs:40` | Un `CTFont` par char unique, jamais purgé |
| Runtime Rust + libs | **5–10 MB** | — | Baseline incompressible |
| Vec temporaires/frame | **~2 MB** | `renderer/mod.rs:446` | `Vec<Vertex>` alloué/libéré chaque frame |

## Optimisations à faire

### 1. Réduire les vertex buffers — gain ~24 MB
- `MAX_VERTEX_BYTES` = 16 MB est très généreux
- Un terminal 200×50 produit ~2.4 MB de vertices max
- **Action** : réduire à 4 MB (×2 = 8 MB au lieu de 32 MB)

### 2. ~~Compacter la struct Cell~~ ✅ DONE
- Fait : `fg`/`bg` passés de `[f32; 3]` (12B) à `[u8; 3]` (3B). Cell est maintenant **32 bytes** (48→32, -33%).
- Le champ `cluster: Option<Box<str>>` (16B) a été ajouté pour le support grapheme cluster (emoji flags, ZWJ).
- Prochaine étape éventuelle : palette-based indexing (`fg_idx: u8`) pour descendre à ~12 bytes, mais complexité élevée pour un gain marginal maintenant que trim trailing blanks est en place.

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

# Perf d'affichage (revue du 2026-10-08)

Revue du chemin de rendu d'un pane, vérifiée par agents (contradicteur + 3 tours d'audit).
Rien n'est mesuré : aucun chiffre de temps de frame avant/après.

## Fait le 2026-10-08

- **Cache par pane hors rafale DEC 2026** : un pane propre dont la `PaneDrawKey` n'a pas
  bougé est dessiné depuis son dernier build (`can_reuse_pane_cache`, `renderer/mod.rs`).
  Avant, toute frame dessinée (sortie d'un seul pane, clignotement du curseur toutes les
  0,5 s, RSS toutes les 2 s) reconstruisait tous les panes visibles. La clé couvre ce que la
  barre de statut affiche sans poser `dirty` (focus, blink, URL survolée, compteurs, process,
  bookmark…) ; `apply_ops` pose maintenant `dirty` après chaque lot, `reset_scroll` aussi.
  **Piège** : toute nouvelle donnée lue par `build_vertices` / `build_status_bar_vertices`
  qui change sans `dirty` doit entrer dans `PaneDrawKey`, sinon l'affichage se fige sans bruit.
- **Fonds fusionnés** : une suite de cellules de même fond = un quad (`bg_runs`).
- **Une copie de moins** : les vertices vont du cache au `MTLBuffer` sans `all_vertices`.
- **`cwd` sans syscall par tick** : `Pane::cached_cwd`, re-sondé au plus toutes les ~0,5 s
  ou quand l'OSC 7 change. Effet visible : la couleur bookmark d'un shell nu peut suivre un
  `cd` avec ~0,5 s de retard (le zshrc n'émet pas d'OSC 7).

## Reste à faire

- **Instancing** : un vertex de 48 octets × 6 par glyphe ou fond (`renderer/vertex.rs`,
  `drawPrimitives` sans `[[instance_id]]` dans `shaders/terminal.metal`). Une instance par
  cellule diviserait le volume envoyé au GPU. Réécrit le shader et le build : gros chantier.
- **Double lookup de glyphe** : 2 `HashMap<char>` SipHash par cellule non vide (passe de
  collecte puis passe de build, `glyph_atlas.rs:447`). Tableau direct pour l'ASCII ou hasher
  rapide.
- **Allocations par build** : `paste_block::plain_text` alloue 2 `String` par ligne,
  `visible_lines`, `bg_runs` et les `HashSet` de la passe 1 allouent par pane.
- **Buffers de vertices sans garde GPU** : `vertex_bufs` alterne sur 2 buffers sans
  sémaphore ni `addCompletedHandler` ; le CPU peut écrire dans un buffer que le GPU lit encore
  (risque de frame corrompue, non observé). Le grow réalloue aussi les deux buffers en place.
- **Capture PTY ON par défaut** : un `write_all` non bufferisé par lecture de 4 Ko sur le
  thread lecteur (`terminal/pty.rs`), voir aussi `pane-open-perf.md`.
- **Mesurer** : aucun bench ni test ne construit de `MTLDevice` ; un `GlyphAtlas` en test
  semble faisable (il ne demande qu'un device), non essayé.

# Kova — Memory optimizations

Measured baseline: ~116 MB RSS for a single pane (2026-02-24).

## Estimated breakdown

| Item | Estimate | File | Notes |
|-------|-----------|---------|-------|
| Metal vertex buffers (×2) | **32 MB** | `renderer/mod.rs:47` | `MAX_VERTEX_BYTES = 16 MB` × 2 double-buffered |
| Scrollback (×N panes) | **~25 MB/pane** | `terminal/mod.rs:69` | 10,000 lines × 80 cols × 32 B/cell (before trim) |
| CPU atlas buf + GPU texture | **1–10 MB** | `renderer/glyph_atlas.rs:31` | Double CPU+GPU storage, grows without shrinking |
| Fallback fonts cache | **1–5 MB** | `renderer/glyph_atlas.rs:40` | One `CTFont` per unique char, never purged |
| Rust runtime + libs | **5–10 MB** | — | Incompressible baseline |
| Temporary Vecs/frame | **~2 MB** | `renderer/mod.rs:446` | `Vec<Vertex>` allocated/freed every frame |

## Optimizations to do

### 1. Shrink the vertex buffers — gain ~24 MB
- `MAX_VERTEX_BYTES` = 16 MB is very generous
- A 200×50 terminal produces ~2.4 MB of vertices at most
- **Action**: reduce to 4 MB (×2 = 8 MB instead of 32 MB)

### 2. ~~Compact the Cell struct~~ ✅ DONE
- Done: `fg`/`bg` changed from `[f32; 3]` (12B) to `[u8; 3]` (3B). Cell is now **32 bytes** (48→32, -33%).
- The `cluster: Option<Box<str>>` field (16B) was added for grapheme cluster support (emoji flags, ZWJ).
- Possible next step: palette-based indexing (`fg_idx: u8`) to get down to ~12 bytes, but high complexity for a marginal gain now that trim trailing blanks is in place.

### 3. Drop the CPU atlas_buf after upload — variable gain
- `atlas_buf: Vec<u8>` is kept permanently for partial updates
- Alternative: keep only a small working buffer (1 cell) and recreate the full GPU texture on grow
- Or use `MTLTexture.getBytes` to read the texture back if needed
- **Tradeoff**: complexity vs memory

### 4. Cap the fallback fonts cache — gain 1–5 MB
- `fallback_fonts: HashMap<char, CFRetained<CTFont>>` grows indefinitely
- Many chars share the same fallback font
- **Action**: cache by font name (not by char) with a `HashMap<String, CFRetained<CTFont>>`
- Or an LRU capped at ~20 fonts

### 5. Reuse the Vec<Vertex> across frames — gain in GC pressure
- Currently a new `Vec` is allocated on each `build_vertices`
- **Action**: keep a persistent `Vec<Vertex>` in the Renderer, `.clear()` every frame
- No direct RSS gain but reduces memory fragmentation

### 6. Scrollback: compression or lazy storage
- Empty/blank lines could be stored as sentinels
- Identical consecutive lines could be deduplicated
- More ambitious: compress old scrollback lines (zstd)
- **Tradeoff**: significant complexity, only worth doing if scrollback is the confirmed bottleneck

# Display performance (review of 2026-10-08)

Review of a pane's rendering path, verified by agents (an adversarial reviewer + 3 audit rounds).
Nothing is measured: no frame time figures before/after.

## Done on 2026-10-08

- **Per-pane cache outside DEC 2026 bursts**: a clean pane whose `PaneDrawKey` has not
  changed is drawn from its last build (`can_reuse_pane_cache`, `renderer/mod.rs`).
  Before, every drawn frame (output from a single pane, cursor blink every
  0.5 s, RSS every 2 s) rebuilt all visible panes. The key covers what the
  status bar shows without setting `dirty` (focus, blink, hovered URL, counters, process,
  bookmark…); `apply_ops` now sets `dirty` after each batch, `reset_scroll` too.
  **Trap**: any new data read by `build_vertices` / `build_status_bar_vertices`
  that changes without `dirty` must go into `PaneDrawKey`, otherwise the display silently freezes.
- **Merged backgrounds**: a run of cells with the same background = one quad (`bg_runs`).
- **One copy fewer**: vertices go from the cache to the `MTLBuffer` without `all_vertices`.
- **`cwd` without a syscall per tick**: `Pane::cached_cwd`, re-probed at most every ~0.5 s
  or when OSC 7 changes. Visible effect: the bookmark color of a bare shell can follow a
  `cd` with a ~0.5 s delay (when the zshrc does not emit OSC 7).

## Still to do

- **Instancing**: a 48-byte vertex × 6 per glyph or background (`renderer/vertex.rs`,
  `drawPrimitives` without `[[instance_id]]` in `shaders/terminal.metal`). One instance per
  cell would divide the volume sent to the GPU. Rewrites the shader and the build: big job.
- **Double glyph lookup**: 2 SipHash `HashMap<char>` per non-empty cell (collection
  pass then build pass, `glyph_atlas.rs:447`). Direct array for ASCII or a fast
  hasher.
- **Allocations per build**: `paste_block::plain_text` allocates 2 `String` per line,
  `visible_lines`, `bg_runs` and the pass-1 `HashSet`s allocate per pane.
- **Vertex buffers without a GPU guard**: `vertex_bufs` alternates between 2 buffers without
  a semaphore or `addCompletedHandler`; the CPU can write into a buffer the GPU is still reading
  (risk of a corrupted frame, not observed). The grow also reallocates both buffers in place.
- **PTY capture ON by default**: one unbuffered `write_all` per 4 KB read on the
  reader thread (`terminal/pty.rs`), see also `pane-open-perf.md`.
- **Measure**: no bench or test builds an `MTLDevice`; a `GlyphAtlas` in a test
  seems feasible (it only needs a device), not tried.

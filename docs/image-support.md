# Inline Image Support (Kitty Graphics Protocol)

Design doc for adding inline image support to Kova via the Kitty Graphics Protocol.

## Chosen protocol: Kitty Graphics

Why Kitty rather than Sixel or iTerm2:

- **Kitty**: modern protocol, well documented, image/placement separation, chunked transfer, z-index, animations. Adopted by Ghostty, WezTerm, Konsole, foot.
- **Sixel**: old (1983), complex to parse (its own state machine), no image cache, retransmission on every display. Rejected.
- **iTerm2**: proprietary, no cache, no z-index, no multiple placements. Not chosen.

Full spec: https://sw.kovidgoyal.net/kitty/graphics-protocol/

## Escape sequence format

```
ESC _ G <key=value,key=value,...> ; <base64-payload> ESC \
```

- `ESC _` = APC (Application Programming Command)
- `G` = graphics protocol identifier
- Base64 payload, separated by `;`
- `ESC \` = ST (String Terminator)

### Main keys

| Key | Role | Values |
|-----|------|---------|
| `a` | Action | `T` transmit+display (default), `t` transmit, `p` place, `d` delete, `q` query |
| `f` | Pixel format | `24` RGB, `32` RGBA (default), `100` PNG |
| `t` | Transmission mode | `d` direct (default), `f` file, `t` temp file, `s` shared memory |
| `i` | Image ID | 1..2^32 |
| `p` | Placement ID | 1..2^32 |
| `s` | Source width (pixels) | required for RGB/RGBA |
| `v` | Source height (pixels) | required for RGB/RGBA |
| `c` | Display columns | integer |
| `r` | Display rows | integer |
| `z` | Z-index | integer (negative = below the text) |
| `m` | Chunked transfer | `1` = more, `0` = last chunk |
| `q` | Quiet mode | `1` = no OK, `2` = no response at all |
| `o` | Compression | `z` = zlib deflate |
| `C` | Cursor movement | `1` = don't move the cursor |

### Chunked transfer

Images are sent in chunks of at most 4096 base64 bytes:

```
ESC_G f=100,i=1,m=1; <chunk1> ESC\
ESC_G m=1;            <chunk2> ESC\
ESC_G m=0;            <chunk3> ESC\
```

Only the first chunk carries the control keys. The image is placed when the last chunk (`m=0`) arrives.

### Terminal response

```
ESC _ G i=<id> ; OK ESC \           -- success
ESC _ G i=<id> ; EINVAL:message ESC\ -- error
```

Error codes: `ENOENT`, `EINVAL`, `ENOSPC`, `ETOODEEP`, `ECYCLE`, `ENOPARENT`.

### Delete

`a=d` with key `d=` to target: `a` (all visible), `i` (by ID), `c` (at cursor), `p` (at position x,y), etc.
Lowercase = deletes the placement. Uppercase = also deletes the image data.

### Support detection

```
ESC_G i=31,s=1,v=1,a=q,t=d,f=24;AAAA ESC\  ESC[c
```

If the terminal supports the protocol, it answers with the APC before the DA1 response.

## Implementation architecture in Kova

### Overview

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

### Workstream 1: APC parser (`src/terminal/parser.rs`)

The `vte` crate doesn't parse APC sequences natively. We need to:

1. Intercept `ESC _` in the VTE handler (`hook()` method for DCS, or a custom APC handler)
2. Detect the `G` prefix (Kitty graphics protocol)
3. Parse the `key=value` pairs (all single-letter, integer or char values)
4. Decode the base64 payload
5. Handle chunking: accumulate chunks (buffer per image ID) until `m=0`

**Files affected**:
- `src/terminal/parser.rs:106-115` — `hook()` method, entry point
- `src/terminal/parser.rs:119-181` — `osc_dispatch()` as a pattern reference

**New code**:
- `ImageCommand` struct: action, image ID, format, dimensions, payload, placement params
- K/V key parser (simple, ~50 lines)
- Chunk buffer (HashMap<image_id, Vec<u8>>)

### Workstream 2: Terminal State (`src/terminal/mod.rs`)

**Critical constraint**: cells are 32 bytes and optimized for RAM. Do NOT touch the `Cell` struct.

Add to `TerminalState`:

```rust
/// Decoded images in memory, indexed by image ID
pub image_store: HashMap<u32, StoredImage>,

/// Active placements: where to show which image
pub image_placements: Vec<ImagePlacement>,
```

```rust
pub struct StoredImage {
    pub id: u32,
    pub width: u32,          // pixels
    pub height: u32,         // pixels
    pub data: Vec<u8>,       // decoded RGBA
    pub texture_dirty: bool, // needs GPU upload
}

pub struct ImagePlacement {
    pub image_id: u32,
    pub placement_id: u32,
    pub col: u16,            // grid position (top-left)
    pub row: i32,            // can be negative (scrollback)
    pub cols: u16,           // display size in cells
    pub rows: u16,
    pub z_index: i32,
    pub source_rect: Option<(u32, u32, u32, u32)>, // x, y, w, h in the source image
}
```

**Files affected**:
- `src/terminal/mod.rs:124-192` — `TerminalState` struct

### Workstream 3: Image Manager (new module `src/renderer/image_manager.rs`)

Management of Metal textures for images, separate from the glyph atlas.

Responsibilities:
- Decode PNGs (via the `image` crate or CoreGraphics)
- Decompress zlib if `o=z`
- Upload pixels as a Metal texture (RGBA8Unorm)
- Texture cache with eviction (LRU by memory size)
- Handle the lifecycle: upload when `texture_dirty`, free when the image is deleted

```rust
pub struct ImageManager {
    /// Cache: image_id → Metal texture
    textures: HashMap<u32, MTLTexture>,
    /// GPU memory used (for eviction)
    gpu_memory_used: usize,
    gpu_memory_limit: usize,  // e.g. 256 MB
}

impl ImageManager {
    pub fn upload(&mut self, device: &MTLDevice, image: &StoredImage) -> &MTLTexture;
    pub fn get_texture(&self, image_id: u32) -> Option<&MTLTexture>;
    pub fn remove(&mut self, image_id: u32);
    pub fn evict_if_needed(&mut self);
}
```

**Texture strategy**: one MTLTexture per image (no image atlas). Reasons:
- Images have very variable sizes (unlike glyphs)
- No need for complex packing
- Easy individual deletion
- Metal handles hundreds of textures well

### Workstream 4: Renderer (`src/renderer/mod.rs`)

**Approach**: add an image pass between the backgrounds and the glyphs.

In `build_vertices()` (lines 610-812), after the backgrounds pass (line 691):

```rust
// Pass 2.5: Image quads
for placement in &term.image_placements {
    if !placement_visible(placement, viewport) { continue; }
    let texture = self.image_manager.get_texture(placement.image_id);
    // Generate a quad covering cols×rows cells
    // tex_coords = [0,0] to [1,1] (whole texture) or sub-rect
    // Use a marker in color.a to signal the shader
    self.push_image_quad(vertices, placement, viewport);
}
```

**Metal multi-texture**:

The current Metal pipeline uses only one texture (the glyph atlas). For images:
- Bind the image texture in slot 1 (the atlas stays in slot 0)
- The fragment shader detects through a marker in the vertex which slot to sample

Problem: you can't change texture between vertices of the same draw call.

**Solutions**:
1. **Separate draw call per image**: one draw call for glyphs (atlas texture), one per image (its texture). Simple but more draw calls.
2. **Image atlas**: pack images into a separate atlas. Complex but a single draw call.
3. **Texture array**: use an `MTLTextureArray`. Clean but limited in number of layers.

**Recommendation**: option 1 (separate draw calls). Simple, and we'll rarely have more than ~10 images visible at once. No bottleneck.

**Files affected**:
- `src/renderer/mod.rs:610-812` — `build_vertices()`
- `src/renderer/mod.rs:550-598` — GPU submission (add image draw calls)
- `src/renderer/mod.rs:110-161` — `Renderer` struct (add `ImageManager`)

### Workstream 5: Shaders (`shaders/terminal.metal`)

For image draw calls, the same vertex shader can be reused. The image fragment shader is trivial:

```metal
fragment float4 image_fragment(
    VertexOut in [[stage_in]],
    texture2d<float> image [[texture(0)]],
    sampler s [[sampler(0)]])
{
    return image.sample(s, in.tex_coords);
}
```

Alternative: a single fragment shader with a flag:

```metal
// Use color.a == 0.5 as an "this is an image" marker
if (in.color.a > 0.4 && in.color.a < 0.6) {
    return atlas.sample(s, in.tex_coords);  // here atlas = image texture
}
```

**Recommendation**: separate pipeline (one for glyphs, one for images). Cleaner, no marker hacks.

**Files affected**:
- `shaders/terminal.metal` — add `image_fragment` (~5 lines)
- `src/renderer/pipeline.rs:6-46` — create a second pipeline state for images

## MVP scope (Phase 1)

Minimal support so that `icat`, `yazi`, and the Kitty tools work:

| Feature | Included | Excluded |
|---------|--------|-------|
| `a=T` (transmit+display) | yes | |
| `a=t` + `a=p` (transmit then place) | yes | |
| `a=d` (delete) | yes (by ID, all) | delete by position/z-index |
| `a=q` (query) | yes | |
| `f=100` (PNG) | yes | |
| `f=24/32` (raw RGB/RGBA) | yes | |
| `o=z` (zlib) | yes | |
| `t=d` (direct) | yes | |
| `t=f/t/s` (file/temp/shm) | | phase 2 |
| Chunked transfer (`m=`) | yes | |
| Placement sizing (`c=`, `r=`) | yes | |
| Z-index (`z=`) | | phase 2 |
| Sub-rect source (`x,y,w,h`) | | phase 2 |
| Unicode placeholders (`U=1`) | | phase 2 |
| Animations (`a=f`, `a=a`) | | phase 3 |
| Relative placements (`P,Q,H,V`) | | phase 3 |

## Rust dependencies

```toml
# PNG decoding
png = "0.17"        # or image = "0.25" (heavier but multi-format)

# zlib decompression
flate2 = "1.0"

# Base64
base64 = "0.22"
```

Alternative: use CoreGraphics (already available) to decode PNGs without an extra dependency.

## Considerations

### RAM

- Each image stored as RGBA = `width × height × 4` bytes on the CPU + as much on the GPU
- A 1920×1080 image = ~8 MB (CPU) + ~8 MB (GPU)
- Cap the GPU cache (e.g. 256 MB) with LRU eviction
- Images in the scrollback: drop the pixel data, keep only the metadata (or nothing)

### Scrollback

Placements are tied to a grid position (row, col). When lines scroll:
- Placements must be shifted (row -= 1 on each scroll)
- Placements that leave the scrollback are deleted
- Phase 2: Unicode placeholders solve this more elegantly

### Security

- File mode (`t=f`): check that it's a regular file, not a device/socket/FIFO
- Reject paths under `/proc`, `/sys`, `/dev`
- Temp mode (`t=t`): check that the path contains `tty-graphics-protocol` and is in a known temp directory
- Cap the total size of images in memory

## Suggested implementation order

1. **APC parser**: intercept the sequences, parse the keys, accumulate the chunks
2. **Image store**: store decoded images in `TerminalState`
3. **Image manager**: upload as a Metal texture
4. **Renderer**: separate draw call for images
5. **Query response**: answer `a=q` for support detection
6. **Delete**: delete by ID and all
7. **Tests**: check with `kitten icat image.png`

## References

- [Kitty Graphics Protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/)
- [Ghostty source (Zig)](https://github.com/ghostty-org/ghostty) — reference implementation
- [WezTerm image support](https://wezfurlong.org/wezterm/imgcat.html) — another Rust implementation

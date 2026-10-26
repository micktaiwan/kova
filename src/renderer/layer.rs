//! One pane drawn alone into its own `CAMetalLayer`: the library's pane view
//! (`crate::embed`), for a host other than Kova.app.
//!
//! Kova.app keeps its own path (`Renderer::render_panes`): one layer per
//! window, every pane and the window chrome in one image. Here a layer holds a
//! single pane and nothing else — no tab bar, no global status bar — and the
//! pane is built by the very same `PaneRenderer` Kova.app uses. Two things of
//! Kova.app's window come along, drawn by the same code: the Cmd+hover URL
//! underline, and the Ctrl+F filter overlay (`build_filter_overlay`), which
//! takes the whole layer while it is open, as it takes Kova.app's focused pane.
//!
//! What every library pane of the process shares is a `SharedRender` per
//! display scale: the Metal device, command queue, pipeline, glyph atlas
//! (`RenderContext`) and the pane style. It is created with the first pane
//! drawn at that scale and freed with the last one (each `LayerRenderer`
//! holds a strong reference, the table below only weak ones).
//!
//! A shared atlas is safe for the panes' caches: a build that rasterizes new
//! glyphs bumps the atlas generation, which is part of every pane's draw key,
//! so every other pane rebuilds on its next frame instead of reusing UVs into
//! a texture that moved.
//!
//! The clock stays with the host: `wants_frame` says whether something changed,
//! `draw` presents a frame when asked. Nothing here runs on its own.

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_metal::*;
use objc2_quartz_core::{CAMetalDrawable, CAMetalLayer};
use std::cell::RefCell;
use std::ptr::NonNull;
use std::rc::{Rc, Weak};
use std::sync::atomic::Ordering;
use std::time::Instant;

use super::blink::CursorBlink;
use super::context::RenderContext;
use super::pane_renderer::{PaneDrawKey, PaneRenderer, PaneStyle};
use super::vertex::Vertex;
use super::{FilterRenderData, PaneRenderData};
use crate::config::Config;

/// The pixel format of every library layer (the same as Kova.app's).
pub const PIXEL_FORMAT: MTLPixelFormat = MTLPixelFormat::BGRA8Unorm;

/// First size of a pane's vertex buffers; they grow to the next power of two
/// when a frame needs more.
const INITIAL_VERTEX_BYTES: usize = 1024 * 1024;

/// What every library pane drawn at one display scale shares.
pub struct SharedRender {
    device: Retained<ProtocolObject<dyn MTLDevice>>,
    ctx: RenderContext,
    style: PaneStyle,
    scale: f64,
}

thread_local! {
    /// Weak references only: a context lives as long as a `LayerRenderer`
    /// holds it. Per thread because Metal objects and the panes are used
    /// from the main thread only (`crate::capi` states it).
    static SHARED: RefCell<Vec<Weak<RefCell<SharedRender>>>> = const { RefCell::new(Vec::new()) };
}

/// The context of the panes drawn at `scale`, created if none is alive. A new
/// context reuses the device of any context still alive, so every library
/// pane of the process draws with one device.
fn shared_for(scale: f64, config: &Config) -> Option<Rc<RefCell<SharedRender>>> {
    SHARED.with(|table| {
        let mut table = table.borrow_mut();
        table.retain(|w| w.strong_count() > 0);
        let alive: Vec<Rc<RefCell<SharedRender>>> = table.iter().filter_map(Weak::upgrade).collect();
        if let Some(found) = alive.iter().find(|s| s.borrow().scale == scale) {
            return Some(found.clone());
        }
        let device = match alive.first() {
            Some(other) => other.borrow().device.clone(),
            None => MTLCreateSystemDefaultDevice()?,
        };
        let ctx = RenderContext::new(&device, PIXEL_FORMAT, scale, config);
        let shared = Rc::new(RefCell::new(SharedRender {
            device,
            ctx,
            style: PaneStyle::from_config(config),
            scale,
        }));
        table.push(Rc::downgrade(&shared));
        Some(shared)
    })
}

/// How many shared contexts are alive on this thread (one per scale in use).
#[cfg(test)]
pub fn live_shared_contexts() -> usize {
    SHARED.with(|table| table.borrow().iter().filter(|w| w.strong_count() > 0).count())
}

/// What a library pane shows over its terminal, besides the grid itself.
#[derive(Default, Clone, Copy)]
pub struct PaneExtras<'a> {
    /// The URL under the pointer while Cmd is held: its segments
    /// (`(visible_row, col_start, col_end)`) and its text, shown in the
    /// status bar as Kova.app does.
    pub hovered_url: Option<(&'a [(usize, u16, u16)], &'a str)>,
    /// The Ctrl+F filter, when it is open.
    pub filter: Option<&'a FilterRenderData>,
}

/// Draws one pane into one layer. Owned by the library pane; dropping it
/// releases its share of the context.
pub struct LayerRenderer {
    shared: Rc<RefCell<SharedRender>>,
    pane: PaneRenderer,
    blink: CursorBlink,
    cursor_blink_frames: u32,
    /// Ticks per second the blink period is counted in (`cursor_blink_frames`
    /// is a number of these, see `blink`). Whatever rate the host draws at,
    /// the blink keeps this period.
    frame_rate: u32,
    viewport_buf: Retained<ProtocolObject<dyn MTLBuffer>>,
    last_viewport: [f32; 2],
    vertex_bufs: [Retained<ProtocolObject<dyn MTLBuffer>>; 2],
    vertex_buf_capacity: usize,
    vertex_buf_idx: usize,
}

fn vertex_buffer(device: &ProtocolObject<dyn MTLDevice>, bytes: usize) -> Option<Retained<ProtocolObject<dyn MTLBuffer>>> {
    device.newBufferWithLength_options(
        bytes,
        MTLResourceOptions(MTLResourceOptions::CPUCacheModeDefaultCache.0 | MTLResourceOptions::StorageModeShared.0),
    )
}

impl LayerRenderer {
    /// A renderer for a layer on a display of `scale`; `None` without a Metal
    /// device.
    pub fn new(scale: f64, config: &Config) -> Option<Self> {
        let shared = shared_for(scale, config)?;
        let device = shared.borrow().device.clone();
        let viewport = [0.0f32; 2];
        let viewport_buf = unsafe {
            device.newBufferWithBytes_length_options(
                NonNull::new(viewport.as_ptr() as *mut _)?,
                std::mem::size_of_val(&viewport),
                MTLResourceOptions::CPUCacheModeDefaultCache,
            )
        }?;
        Some(LayerRenderer {
            vertex_bufs: [vertex_buffer(&device, INITIAL_VERTEX_BYTES)?, vertex_buffer(&device, INITIAL_VERTEX_BYTES)?],
            shared,
            pane: PaneRenderer::default(),
            blink: CursorBlink::new(),
            cursor_blink_frames: config.terminal.cursor_blink_frames,
            frame_rate: config.terminal.fps,
            viewport_buf,
            last_viewport: viewport,
            vertex_buf_capacity: INITIAL_VERTEX_BYTES,
            vertex_buf_idx: 0,
        })
    }

    /// The device the layer must be given.
    pub fn device(&self) -> Retained<ProtocolObject<dyn MTLDevice>> {
        self.shared.borrow().device.clone()
    }

    pub fn scale(&self) -> f64 {
        self.shared.borrow().scale
    }

    /// Cell size in drawable pixels.
    pub fn cell_size(&self) -> (f32, f32) {
        self.shared.borrow().ctx.cell_size()
    }

    /// Horizontal padding before the first column, in drawable pixels.
    pub fn h_padding(&self) -> f32 {
        self.shared.borrow().ctx.h_padding()
    }

    /// Columns and rows that fit a drawable of `width`×`height` pixels: the
    /// same arithmetic as Kova.app's `viewport_to_grid` (padding on both
    /// sides, one row for the status bar when it is on).
    pub fn grid_for(&self, width: f32, height: f32) -> (u16, u16) {
        let shared = self.shared.borrow();
        let (cell_w, cell_h) = shared.ctx.cell_size();
        grid_for(width, height, cell_w, cell_h, shared.ctx.h_padding(), shared.style.status_bar_enabled)
    }

    fn cursor_epoch(pane: &PaneRenderData) -> Option<u32> {
        pane.is_focused.then(|| pane.terminal.read().cursor_move_epoch.load(Ordering::Relaxed))
    }

    fn draw_key(&self, pane: &PaneRenderData, extras: &PaneExtras, blink_on: bool) -> PaneDrawKey {
        let (segments, text) = split_hover(extras);
        PaneRenderer::draw_key(pane, segments, text, blink_on, self.shared.borrow().ctx.atlas.generation)
    }

    /// Whether the next `draw` would present a frame: the terminal changed,
    /// something its build reads moved (cursor blink, focus, size, the atlas
    /// another pane grew, the hovered URL…), the shell is still loading, a
    /// synchronized update is in progress, or the filter is open (redrawn on
    /// every frame, as Kova.app does). Never consumes anything.
    pub fn wants_frame(&self, pane: &PaneRenderData, extras: &PaneExtras) -> bool {
        let blink_on = self.blink.peek(Instant::now(), Self::cursor_epoch(pane), self.cursor_blink_frames, self.frame_rate);
        self.wants_frame_with(pane, extras, blink_on)
    }

    fn wants_frame_with(&self, pane: &PaneRenderData, extras: &PaneExtras, blink_on: bool) -> bool {
        if extras.filter.is_some() {
            return true;
        }
        if !pane.shell_ready || !self.pane.is_built() || PaneRenderer::in_sync_window(pane) {
            return true;
        }
        if pane.terminal.read().dirty.load(Ordering::Relaxed) {
            return true;
        }
        self.pane.key_changed(&self.draw_key(pane, extras, blink_on))
    }

    /// Draw `pane` into `layer` when `wants_frame` (or `force`) says so, and
    /// present it. The pane fills the whole drawable. Returns whether a frame
    /// was presented (false: nothing changed, or no drawable was available).
    pub fn draw(&mut self, layer: &CAMetalLayer, pane: &PaneRenderData, extras: &PaneExtras, force: bool) -> bool {
        let (blink_on, _) = self.blink.sample(Instant::now(), Self::cursor_epoch(pane), self.cursor_blink_frames, self.frame_rate);
        if !force && !self.wants_frame_with(pane, extras, blink_on) {
            return false;
        }
        let Some(drawable) = layer.nextDrawable() else { return false };
        let size = layer.drawableSize();
        let (viewport_w, viewport_h) = (size.width as f32, size.height as f32);
        let pane = PaneRenderData {
            viewport: super::PaneViewport { x: 0.0, y: 0.0, width: viewport_w, height: viewport_h },
            terminal: pane.terminal.clone(),
            input_chars: pane.input_chars.clone(),
            custom_title: pane.custom_title.clone(),
            fg_process: pane.fg_process.clone(),
            ..*pane
        };

        // Same order as Kova.app's frame: the sync state is read once, the
        // dirty flag consumed only now that a drawable is in hand.
        let in_sync = PaneRenderer::in_sync_window(&pane);
        let dirty = !in_sync && pane.terminal.read().dirty.swap(false, Ordering::Relaxed);

        let mut shared = self.shared.borrow_mut();
        let SharedRender { ctx, style, .. } = &mut *shared;
        let mut overlay: Vec<Vertex> = Vec::new();
        if let Some(filter) = extras.filter {
            // The overlay covers the pane, which is not built under it (as
            // Kova.app skips its focused pane): the cache is dropped so the
            // grid is rebuilt once the filter closes.
            self.pane = PaneRenderer::default();
            super::build_filter_overlay(
                ctx,
                &mut overlay,
                &pane.viewport,
                filter,
                style.list_selected_bg,
            );
        } else {
            let (segments, text) = split_hover(extras);
            let mut zones = Vec::new();
            // Retried when the build rasterized new glyphs: UVs computed before the
            // atlas changed would point into the wrong place (as in Kova.app's
            // `rebuild_pane_draws`).
            for _pass in 0..3 {
                let generation = ctx.atlas.generation;
                let key = PaneRenderer::draw_key(&pane, segments, text, blink_on, generation);
                zones.clear();
                self.pane.prepare(ctx, style, &pane, segments, text, key, in_sync, dirty, &mut zones);
                if ctx.atlas.generation == generation {
                    break;
                }
                self.pane = PaneRenderer::default();
            }
        }
        ctx.sync_atlas_size();

        let verts: &[Vertex] = if extras.filter.is_some() { &overlay } else { self.pane.vertices() };
        let viewport = [viewport_w, viewport_h];
        if viewport != self.last_viewport {
            self.last_viewport = viewport;
            unsafe { *(self.viewport_buf.contents().as_ptr() as *mut [f32; 2]) = viewport };
        }

        let pass_desc = MTLRenderPassDescriptor::new();
        let color = unsafe { pass_desc.colorAttachments().objectAtIndexedSubscript(0) };
        color.setTexture(Some(&drawable.texture()));
        color.setLoadAction(MTLLoadAction::Clear);
        color.setClearColor(MTLClearColor {
            red: style.bg_color[0] as f64,
            green: style.bg_color[1] as f64,
            blue: style.bg_color[2] as f64,
            alpha: 1.0,
        });
        color.setStoreAction(MTLStoreAction::Store);

        let Some(cmd_buf) = ctx.command_queue.commandBuffer() else {
            log::error!("Metal: failed to create command buffer, skipping frame");
            return false;
        };
        let Some(encoder) = cmd_buf.renderCommandEncoderWithDescriptor(&pass_desc) else {
            log::error!("Metal: failed to create render encoder, skipping frame");
            return false;
        };

        if !verts.is_empty() {
            let bytes = std::mem::size_of_val(verts);
            if bytes > self.vertex_buf_capacity {
                let capacity = bytes.next_power_of_two();
                let device = &ctx.atlas.device;
                for slot in self.vertex_bufs.iter_mut() {
                    match vertex_buffer(device, capacity) {
                        Some(buf) => *slot = buf,
                        None => {
                            log::error!("Metal: failed to grow the vertex buffer to {} bytes", capacity);
                            encoder.endEncoding();
                            return false;
                        }
                    }
                }
                self.vertex_buf_capacity = capacity;
            }
            let buf_idx = self.vertex_buf_idx;
            self.vertex_buf_idx = 1 - buf_idx;
            let vertex_buf = &self.vertex_bufs[buf_idx];
            // SAFETY: the buffer holds at least `bytes` (grown above).
            unsafe {
                std::ptr::copy_nonoverlapping(verts.as_ptr(), vertex_buf.contents().as_ptr() as *mut Vertex, verts.len())
            };
            encoder.setRenderPipelineState(&ctx.pipeline);
            unsafe {
                encoder.setVertexBuffer_offset_atIndex(Some(vertex_buf), 0, 0);
                encoder.setVertexBuffer_offset_atIndex(Some(&self.viewport_buf), 0, 1);
                encoder.setVertexBuffer_offset_atIndex(Some(&ctx.atlas_size_buf), 0, 2);
                encoder.setFragmentTexture_atIndex(Some(&*ctx.atlas.texture), 0);
            }
            encoder.setScissorRect(MTLScissorRect {
                x: 0,
                y: 0,
                width: viewport_w.max(0.0) as usize,
                height: viewport_h.max(0.0) as usize,
            });
            unsafe { encoder.drawPrimitives_vertexStart_vertexCount(MTLPrimitiveType::Triangle, 0, verts.len()) };
        }
        encoder.endEncoding();
        let mtl_drawable: &ProtocolObject<dyn MTLDrawable> = ProtocolObject::from_ref(&*drawable);
        cmd_buf.presentDrawable(mtl_drawable);
        cmd_buf.commit();
        true
    }
}

/// The hover of `extras` as the pane build takes it: segments and text apart.
fn split_hover<'a>(extras: &PaneExtras<'a>) -> (Option<&'a [(usize, u16, u16)]>, Option<&'a str>) {
    match extras.hovered_url {
        Some((segments, text)) => (Some(segments), Some(text)),
        None => (None, None),
    }
}

/// Columns and rows of a pane `width`×`height` pixels wide, for cells of
/// `cell_w`×`cell_h` and `h_pad` pixels of padding on each side; one row less
/// with a status bar. At least one of each.
pub fn grid_for(width: f32, height: f32, cell_w: f32, cell_h: f32, h_pad: f32, status_bar: bool) -> (u16, u16) {
    let cols = ((width - 2.0 * h_pad) / cell_w).floor().max(1.0) as u16;
    let usable_h = if status_bar { height - cell_h } else { height };
    let rows = (usable_h / cell_h).floor().max(1.0) as u16;
    (cols, rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_matches_kova_app_arithmetic() {
        // 800×600 px, 10×20 cells, 20 px padding, status bar on.
        assert_eq!(grid_for(800.0, 600.0, 10.0, 20.0, 20.0, true), (76, 29));
        assert_eq!(grid_for(800.0, 600.0, 10.0, 20.0, 20.0, false), (76, 30));
        // Never below one cell, even for a collapsed view.
        assert_eq!(grid_for(0.0, 0.0, 10.0, 20.0, 20.0, true), (1, 1));
    }

    /// Two calls to `MTLCreateSystemDefaultDevice` hand back the same device.
    /// docs/embedding.md listed it as unverified; the library does not rely on
    /// it (a new context reuses the device of a live one), but the answer
    /// tells whether a host creating its own device would share ours.
    #[test]
    fn the_system_default_device_is_one_object() {
        let (Some(a), Some(b)) = (MTLCreateSystemDefaultDevice(), MTLCreateSystemDefaultDevice()) else {
            return; // no Metal device on this machine
        };
        let pa = Retained::as_ptr(&a) as *const ();
        let pb = Retained::as_ptr(&b) as *const ();
        eprintln!("MTLCreateSystemDefaultDevice: {:p} / {:p}, registryID {} / {}", pa, pb, a.registryID(), b.registryID());
        assert_eq!(a.registryID(), b.registryID());
        assert_eq!(pa, pb, "same object, not just the same GPU");
    }

    #[test]
    fn panes_at_one_scale_share_a_context_freed_with_the_last() {
        if MTLCreateSystemDefaultDevice().is_none() {
            return;
        }
        let config = Config::default();
        assert_eq!(live_shared_contexts(), 0);
        let a = LayerRenderer::new(2.0, &config).unwrap();
        let b = LayerRenderer::new(2.0, &config).unwrap();
        assert!(Rc::ptr_eq(&a.shared, &b.shared), "same scale, same context");
        assert_eq!(live_shared_contexts(), 1);
        let c = LayerRenderer::new(1.0, &config).unwrap();
        assert!(!Rc::ptr_eq(&a.shared, &c.shared), "another scale, another atlas");
        assert_eq!(Retained::as_ptr(&a.device()), Retained::as_ptr(&c.device()), "one device for both");
        assert_eq!(live_shared_contexts(), 2);
        drop(a);
        assert_eq!(live_shared_contexts(), 2, "b still holds the 2x context");
        drop(b);
        drop(c);
        assert_eq!(live_shared_contexts(), 0, "freed with the last pane");
    }
}

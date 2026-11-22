//! A texture that another thread writes while the game draws it.
//!
//! A producer (a mod's Wayland compositor, a video decoder) owns a [`LiveTexture`] and writes
//! frames into it from any thread; the renderer draws it wherever a draw call names it and uploads
//! only what changed since it last looked. The handle is an `Arc`: the renderer drops its GPU copy
//! one frame after no draw names the texture any more.
//!
//! Two sources:
//!
//! - **Pixels** ([`LiveTexture::write`]): the producer's CPU pixels are copied into the texture's
//!   own mirror, region by region, and the renderer uploads the damaged regions. This is the
//!   portable path (wl_shm buffers, software decoders).
//! - **Dmabuf** ([`LiveTexture::set_dmabuf`]): the producer hands over a Linux dmabuf (a GPU
//!   buffer by file descriptor) and the renderer imports it without a copy. The renderer falls back
//!   to nothing (the texture keeps its last pixels) when it cannot import that buffer.
//!
//! Neither path blocks the renderer on the producer for longer than one `memcpy` of the damage.
#![deny(unsafe_code)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

/// How a producer's bytes are laid out, per pixel, in memory order. Colours with an alpha byte
/// are premultiplied, as Wayland's are; the texture stores them straight (not premultiplied), as
/// the game's other textures are.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PixelFormat {
    /// B, G, R, A bytes: Wayland's `ARGB8888` on a little-endian machine.
    Bgra8,
    /// B, G, R, then an ignored byte: Wayland's `XRGB8888`. Stored opaque.
    Bgrx8,
    /// R, G, B, A bytes: Wayland's `ABGR8888`.
    Rgba8,
    /// R, G, B, then an ignored byte: Wayland's `XBGR8888`. Stored opaque.
    Rgbx8,
}

impl PixelFormat {
    /// Whether the fourth byte is padding, so the stored pixel is opaque.
    pub fn opaque(self) -> bool {
        matches!(self, Self::Bgrx8 | Self::Rgbx8)
    }
    /// Whether red comes before blue in memory.
    pub fn rgb_order(self) -> bool {
        matches!(self, Self::Rgba8 | Self::Rgbx8)
    }
}

/// A rectangle in texels: `x`, `y`, `width`, `height`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Region {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Region {
    pub const fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
    /// The whole of a `width` x `height` texture.
    pub const fn full(width: u32, height: u32) -> Self {
        Self::new(0, 0, width, height)
    }
    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }
    /// This region cut to a `width` x `height` texture.
    pub fn clamp(self, width: u32, height: u32) -> Self {
        let x = self.x.min(width);
        let y = self.y.min(height);
        Self::new(x, y, self.width.min(width - x), self.height.min(height - y))
    }
    /// The smallest region holding both.
    pub fn union(self, other: Self) -> Self {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return self;
        }
        let x0 = self.x.min(other.x);
        let y0 = self.y.min(other.y);
        let x1 = (self.x + self.width).max(other.x + other.width);
        let y1 = (self.y + self.height).max(other.y + other.height);
        Self::new(x0, y0, x1 - x0, y1 - y0)
    }
}

/// One plane of a dmabuf.
#[derive(Debug)]
pub struct DmabufPlane {
    /// The buffer's file descriptor; the texture owns it.
    pub fd: std::os::fd::OwnedFd,
    pub offset: u32,
    pub stride: u32,
}

/// A Linux dmabuf the renderer may import without a copy.
#[derive(Debug)]
pub struct Dmabuf {
    /// The producer's buffer this is, unique in the process: a renderer may keep its import and
    /// reuse it when the same buffer comes back (clients cycle two to four).
    pub id: u64,
    pub width: u32,
    pub height: u32,
    /// The DRM fourcc code (`DRM_FORMAT_ARGB8888`...).
    pub fourcc: u32,
    /// The DRM format modifier, shared by every plane.
    pub modifier: u64,
    pub planes: Vec<DmabufPlane>,
    /// Whether the producer flipped the buffer vertically (`y_invert`).
    pub y_inverted: bool,
}

/// What the renderer should take from the texture now ([`LiveTexture::sync`]).
pub enum Sync<'a> {
    /// Nothing changed since the generation the renderer passed.
    Unchanged,
    /// The texture is `width` x `height` (re)created or resized: upload all of `pixels`.
    Full {
        width: u32,
        height: u32,
        pixels: &'a [u8],
    },
    /// Upload `region` from `pixels` (the whole mirror, `width` texels wide, rows of
    /// `width * 4` bytes) into the existing texture.
    Damage {
        width: u32,
        region: Region,
        pixels: &'a [u8],
    },
    /// A new dmabuf replaced the source: import it (it stays the source until the next one).
    Dmabuf(&'a Arc<Dmabuf>),
}

/// The renderer's view of a texture while it syncs, holding its lock.
pub struct Synced<'a> {
    guard: MutexGuard<'a, State>,
    /// The generation the renderer has now seen: pass it to the next [`LiveTexture::sync`].
    pub generation: u64,
    from: u64,
}

impl Synced<'_> {
    /// What to upload or import.
    pub fn what(&self) -> Sync<'_> {
        let state = &*self.guard;
        if self.from == state.generation {
            return Sync::Unchanged;
        }
        if let Source::Dmabuf(buffer) = &state.source {
            return Sync::Dmabuf(buffer);
        }
        let full = Sync::Full {
            width: state.width,
            height: state.height,
            pixels: &state.pixels,
        };
        if self.from < state.resized_at {
            return full;
        }
        // The log holds every write from `log_since` on: older than that, only a full upload works.
        if self.from + 1 < state.log_since {
            return full;
        }
        let region = state
            .log
            .iter()
            .filter(|(generation, _)| *generation > self.from)
            .fold(Region::default(), |region, (_, damage)| {
                region.union(*damage)
            });
        Sync::Damage {
            width: state.width,
            region,
            pixels: &state.pixels,
        }
    }
}

enum Source {
    Pixels,
    Dmabuf(Arc<Dmabuf>),
}

struct State {
    width: u32,
    height: u32,
    /// RGBA8 rows, `width * 4` bytes each.
    pixels: Vec<u8>,
    source: Source,
    generation: u64,
    /// The generation of the last change of size or source: before it, only a full upload works.
    resized_at: u64,
    /// The damage of the last writes, by generation.
    log: Vec<(u64, Region)>,
    /// The first generation the log may be missing entries before.
    log_since: u64,
}

const LOG: usize = 16;

/// A texture another thread writes while the game draws it. Cheap to clone: clones are the same
/// texture.
#[derive(Clone)]
pub struct LiveTexture {
    inner: Arc<Inner>,
}

struct Inner {
    id: u64,
    state: Mutex<State>,
    /// Bumped on every change, so a renderer can skip the lock when nothing changed.
    generation: AtomicU64,
    /// The last generation the GPU has finished copying out of its source.
    consumed: AtomicU64,
}

impl std::fmt::Debug for LiveTexture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let [width, height] = self.size();
        f.debug_struct("LiveTexture")
            .field("id", &self.inner.id)
            .field("width", &width)
            .field("height", &height)
            .finish()
    }
}

impl PartialEq for LiveTexture {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}
impl Eq for LiveTexture {}

impl LiveTexture {
    /// A transparent `width` x `height` texture.
    pub fn new(width: u32, height: u32) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self {
            inner: Arc::new(Inner {
                id: NEXT.fetch_add(1, Ordering::Relaxed),
                state: Mutex::new(State {
                    width,
                    height,
                    pixels: vec![0; width as usize * height as usize * 4],
                    source: Source::Pixels,
                    generation: 1,
                    resized_at: 1,
                    log: Vec::new(),
                    log_since: 1,
                }),
                generation: AtomicU64::new(1),
                consumed: AtomicU64::new(0),
            }),
        }
    }

    /// The texture's identity, unique in the process.
    pub fn id(&self) -> u64 {
        self.inner.id
    }

    /// The width and height in texels.
    pub fn size(&self) -> [u32; 2] {
        let state = self.lock();
        [state.width, state.height]
    }

    /// The number of changes so far: a renderer that saw this generation has nothing to do.
    pub fn generation(&self) -> u64 {
        self.inner.generation.load(Ordering::Acquire)
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn bump(&self, state: &mut State) -> u64 {
        state.generation += 1;
        self.inner
            .generation
            .store(state.generation, Ordering::Release);
        state.generation
    }

    /// Copy the `damage` regions of a `width` x `height` frame (`stride` bytes per row of
    /// `bytes`, laid out as `format`) into the texture. A frame of another size resizes the
    /// texture and copies all of it, whatever `damage` says; empty `damage` means all of it.
    /// Returns false (and changes nothing) when `bytes` is too short for the frame.
    pub fn write(
        &self,
        width: u32,
        height: u32,
        stride: usize,
        format: PixelFormat,
        bytes: &[u8],
        damage: &[Region],
    ) -> bool {
        let row = width as usize * 4;
        if stride < row || (height > 0 && bytes.len() < stride * (height as usize - 1) + row) {
            return false;
        }
        let mut state = self.lock();
        let resized = state.width != width
            || state.height != height
            || !matches!(state.source, Source::Pixels);
        let full = [Region::full(width, height)];
        let damage = if resized || damage.is_empty() {
            &full[..]
        } else {
            damage
        };
        if resized {
            state.width = width;
            state.height = height;
            state.pixels.resize(row * height as usize, 0);
            state.source = Source::Pixels;
        }
        let mut total = Region::default();
        for region in damage {
            let region = region.clamp(width, height);
            if region.is_empty() {
                continue;
            }
            total = total.union(region);
            copy_region(&mut state.pixels, row, bytes, stride, format, region);
        }
        if total.is_empty() && !resized {
            return true;
        }
        let generation = self.bump(&mut state);
        if resized {
            state.resized_at = generation;
            state.log.clear();
            state.log_since = generation;
        } else {
            if state.log.len() == LOG {
                let (dropped, _) = state.log.remove(0);
                state.log_since = dropped + 1;
            }
            state.log.push((generation, total));
        }
        true
    }

    /// Make `buffer` the texture's source, imported by the renderer without a CPU copy. The
    /// texture takes the buffer's size. Returns the generation it is: once [`Self::consumed`]
    /// reaches it, the renderer has finished reading the buffer.
    pub fn set_dmabuf(&self, buffer: Dmabuf) -> u64 {
        let mut state = self.lock();
        state.width = buffer.width;
        state.height = buffer.height;
        state.source = Source::Dmabuf(Arc::new(buffer));
        let generation = self.bump(&mut state);
        state.resized_at = generation;
        state.log.clear();
        state.log_since = generation;
        generation
    }

    /// The last generation whose source the GPU has finished reading: a producer may reuse a
    /// dmabuf it gave at that generation or before.
    pub fn consumed(&self) -> u64 {
        self.inner.consumed.load(Ordering::Acquire)
    }

    /// For the renderer: the GPU has finished reading the source of `generation`.
    pub fn mark_consumed(&self, generation: u64) {
        self.inner.consumed.fetch_max(generation, Ordering::AcqRel);
    }

    /// For the renderer: what changed since `generation` (0 for a texture it hasn't seen),
    /// holding the texture's lock until the returned value drops. Producers wait meanwhile, so
    /// upload and drop it promptly.
    pub fn sync(&self, generation: u64) -> Synced<'_> {
        let guard = self.lock();
        Synced {
            generation: guard.generation,
            from: generation,
            guard,
        }
    }
}

/// A dmabuf layout the renderer can import: a DRM fourcc and format modifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DmabufFormat {
    pub fourcc: u32,
    pub modifier: u64,
}

/// What dmabufs the renderer imports: its GPU's DRM device and the layouts it reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DmabufCaps {
    /// The `dev_t` of the GPU's DRM node (its render node when it has one): where producers
    /// should allocate.
    pub device: u64,
    pub formats: Vec<DmabufFormat>,
}

static CAPS: Mutex<Option<DmabufCaps>> = Mutex::new(None);

/// For the renderer: publish what it imports, or `None` when it imports no dmabufs (not
/// Vulkan, a driver without the extensions). A new device publishes again.
pub fn set_dmabuf_caps(caps: Option<DmabufCaps>) {
    *CAPS.lock().unwrap_or_else(|p| p.into_inner()) = caps;
}

/// What dmabufs the renderer imports now; `None` before it started or when it can't.
pub fn dmabuf_caps() -> Option<DmabufCaps> {
    CAPS.lock().unwrap_or_else(|p| p.into_inner()).clone()
}

/// DRM fourcc codes of the 8-bit RGB layouts.
pub mod fourcc {
    const fn code(c: &[u8; 4]) -> u32 {
        u32::from_le_bytes(*c)
    }
    pub const ARGB8888: u32 = code(b"AR24");
    pub const XRGB8888: u32 = code(b"XR24");
    pub const ABGR8888: u32 = code(b"AB24");
    pub const XBGR8888: u32 = code(b"XB24");
}

/// Copy `region` from a producer's frame into the RGBA mirror.
fn copy_region(
    mirror: &mut [u8],
    row: usize,
    bytes: &[u8],
    stride: usize,
    format: PixelFormat,
    region: Region,
) {
    let x0 = region.x as usize * 4;
    let x1 = x0 + region.width as usize * 4;
    for y in region.y as usize..(region.y + region.height) as usize {
        let src = &bytes[y * stride + x0..y * stride + x1];
        let dst = &mut mirror[y * row + x0..y * row + x1];
        let (rgb, opaque) = (format.rgb_order(), format.opaque());
        for (d, s) in dst
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(src.as_chunks::<4>().0)
        {
            let [r, g, b] = if rgb {
                [s[0], s[1], s[2]]
            } else {
                [s[2], s[1], s[0]]
            };
            let a = if opaque { 255 } else { s[3] };
            *d = if a == 255 {
                [r, g, b, 255]
            } else if a == 0 {
                [0, 0, 0, 0]
            } else {
                let straight =
                    |c: u8| ((u32::from(c) * 255 + u32::from(a) / 2) / u32::from(a)).min(255) as u8;
                [straight(r), straight(g), straight(b), a]
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sync_kind(texture: &LiveTexture, from: u64) -> (u64, String) {
        let synced = texture.sync(from);
        let kind = match synced.what() {
            Sync::Unchanged => "unchanged".to_string(),
            Sync::Full { width, height, .. } => format!("full {width}x{height}"),
            Sync::Damage { region, .. } => format!(
                "damage {} {} {} {}",
                region.x, region.y, region.width, region.height
            ),
            Sync::Dmabuf(_) => "dmabuf".to_string(),
        };
        (synced.generation, kind)
    }

    #[test]
    fn a_renderer_uploads_only_the_damage_it_has_not_seen() {
        let texture = LiveTexture::new(4, 4);
        let (seen, kind) = sync_kind(&texture, 0);
        assert_eq!(kind, "full 4x4");
        let frame = vec![0x80u8; 4 * 4 * 4];
        texture.write(
            4,
            4,
            16,
            PixelFormat::Rgba8,
            &frame,
            &[Region::new(1, 1, 1, 1)],
        );
        texture.write(
            4,
            4,
            16,
            PixelFormat::Rgba8,
            &frame,
            &[Region::new(3, 2, 1, 2)],
        );
        let (seen, kind) = sync_kind(&texture, seen);
        assert_eq!(kind, "damage 1 1 3 3");
        assert_eq!(sync_kind(&texture, seen).1, "unchanged");
    }

    #[test]
    fn a_new_size_or_a_lost_log_means_a_full_upload() {
        let texture = LiveTexture::new(2, 2);
        let (seen, _) = sync_kind(&texture, 0);
        let frame = vec![0u8; 3 * 3 * 4];
        texture.write(
            3,
            3,
            12,
            PixelFormat::Rgba8,
            &frame,
            &[Region::new(0, 0, 1, 1)],
        );
        let (seen, kind) = sync_kind(&texture, seen);
        assert_eq!(kind, "full 3x3");
        for _ in 0..LOG + 1 {
            texture.write(
                3,
                3,
                12,
                PixelFormat::Rgba8,
                &frame,
                &[Region::new(0, 0, 1, 1)],
            );
        }
        assert_eq!(sync_kind(&texture, seen).1, "full 3x3");
    }

    #[test]
    fn wayland_byte_orders_are_stored_as_rgba() {
        let texture = LiveTexture::new(2, 1);
        // XRGB8888 in memory: B G R X, padded rows of 12 bytes.
        let frame = [1, 2, 3, 0, 4, 5, 6, 9, 0xEE, 0xEE, 0xEE, 0xEE];
        assert!(texture.write(2, 1, 12, PixelFormat::Bgrx8, &frame, &[]));
        let synced = texture.sync(0);
        let Sync::Full { pixels, .. } = synced.what() else {
            panic!("first sync is full");
        };
        assert_eq!(pixels, &[3, 2, 1, 255, 6, 5, 4, 255]);
    }

    #[test]
    fn premultiplied_colour_is_stored_straight() {
        let texture = LiveTexture::new(1, 1);
        // Half-covered red, premultiplied: R 128 at alpha 128 is full red.
        assert!(texture.write(1, 1, 4, PixelFormat::Rgba8, &[128, 0, 0, 128], &[]));
        let synced = texture.sync(0);
        let Sync::Full { pixels, .. } = synced.what() else {
            panic!("first sync is full");
        };
        assert_eq!(pixels, &[255, 0, 0, 128]);
    }

    #[test]
    fn a_short_frame_is_refused() {
        let texture = LiveTexture::new(2, 2);
        let generation = texture.generation();
        assert!(!texture.write(2, 2, 8, PixelFormat::Rgba8, &[0; 15], &[]));
        assert_eq!(texture.generation(), generation);
    }
}

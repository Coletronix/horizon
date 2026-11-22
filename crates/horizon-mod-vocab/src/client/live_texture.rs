//! Live textures: a texture a native mod writes from any thread (a Wayland window it composites,
//! a video it decodes) and draws with [`crate::DrawContext::live_texture`]. The handle is
//! process-local, shared with the renderer: wasm mods have none.

// no wire: a process-local handle shared with the renderer; native mods only.
pub use horizon_live_texture::{
    Dmabuf as LiveDmabuf, DmabufPlane as LiveDmabufPlane, LiveTexture, PixelFormat as LivePixelFormat,
    Region as LiveRegion,
};

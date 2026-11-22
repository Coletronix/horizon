//! Live textures in the GUI: a texture another thread writes (a mod's Wayland window, a video),
//! drawn at its place in the HUD's ordered runs. The renderer's shared cache
//! (`gfx::live_textures`) holds and uploads the textures; the HUD keeps a bind group for each.
use super::*;
use horizon_live_texture::LiveTexture;

/// The HUD's bind group for one live texture, made with the cache entry's `version`.
pub(super) struct LiveGroup {
    group: wgpu::BindGroup,
    version: u64,
}

impl HudBatch {
    /// Draw part `uv` (`[u0, v0, u1, v1]`, 0 to 1 across the texture) of `texture` stretched into
    /// `[x, y, w, h]` GUI pixels, every texel multiplied by `tint` (ABGR, as the batch's colours).
    pub fn live_texture(&mut self, texture: &LiveTexture, rect: [f32; 4], uv: [f32; 4], tint: u32) {
        let [x, y, w, h] = rect;
        if !(w > 0.0 && h > 0.0) {
            return;
        }
        let start = self.vertices.len();
        let [u0, v0, u1, v1] = uv;
        self.quad(
            [[x, y], [x + w, y], [x + w, y + h], [x, y + h]],
            [[u0, v0], [u1, v0], [u1, v1], [u0, v1]],
            tint,
        );
        self.blend_since(start, Blend::Live(texture.id()));
        if !self.live.iter().any(|t| t.id() == texture.id()) {
            self.live.push(texture.clone());
        }
    }
}

impl HudRenderer {
    pub(super) fn draw_live(
        &self,
        pass: &mut wgpu::RenderPass<'_>,
        id: u64,
        range: std::ops::Range<u32>,
        coverage: bool,
    ) {
        if let Some(live) = self.live.get(&id) {
            pass.set_bind_group(0, &live.group, &[]);
            pass.set_pipeline(self.pipeline.get(coverage));
            pass.draw(range, 0..1);
            pass.set_bind_group(0, &self.group, &[]);
        }
    }

    /// Bind the live textures `batch` draws, as the renderer's cache holds them this frame
    /// (synced before this), and forget the others.
    pub fn bind_live(
        &mut self,
        device: &wgpu::Device,
        cache: &crate::gfx::live_textures::LiveTextures,
        batch: &HudBatch,
    ) {
        self.live
            .retain(|id, _| batch.live.iter().any(|t| t.id() == *id));
        for texture in &batch.live {
            let Some(entry) = cache.get(texture.id()) else {
                self.live.remove(&texture.id());
                continue;
            };
            if self
                .live
                .get(&texture.id())
                .is_some_and(|g| g.version == entry.version)
            {
                continue;
            }
            let group = self.live_group(device, &entry.view);
            self.live.insert(
                texture.id(),
                LiveGroup {
                    group,
                    version: entry.version,
                },
            );
        }
    }

    fn live_group(&self, device: &wgpu::Device, view: &wgpu::TextureView) -> wgpu::BindGroup {
        let sampler = crate::gfx::live_textures::sampler(device);
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("GUI live texture"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.uniform.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(&self.atlas_view),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 5,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 6,
                    resource: wgpu::BindingResource::TextureView(&self.atlas_view),
                },
            ],
        })
    }
}

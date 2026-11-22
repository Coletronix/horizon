//! Live textures in the GUI: a texture another thread writes (a mod's Wayland window, a video),
//! drawn at its place in the HUD's ordered runs. The renderer keeps one GPU texture per live
//! texture a frame names and uploads only the regions changed since it last looked; a texture no
//! frame names is dropped.
use super::*;
use horizon_live_texture::{LiveTexture, Sync};

/// The GPU side of one live texture.
pub(super) struct LiveGpu {
    texture: wgpu::Texture,
    group: wgpu::BindGroup,
    size: [u32; 2],
    generation: u64,
}

impl HudBatch {
    /// Draw part `uv` (`[u0, v0, u1, v1]`, 0 to 1 across the texture) of `texture` stretched into
    /// `[x, y, w, h]` GUI pixels, every texel multiplied by `tint` (ARGB).
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

    /// Upload what changed in the live textures `batch` draws, dropping the ones it doesn't.
    pub(super) fn prepare_live(
        &mut self,
        device: &wgpu::Device,
        uploads: &mut crate::gfx::upload::Uploads,
        encoder: &mut wgpu::CommandEncoder,
        batch: &HudBatch,
    ) {
        self.live
            .retain(|id, _| batch.live.iter().any(|t| t.id() == *id));
        let max = device.limits().max_texture_dimension_2d;
        for texture in &batch.live {
            let seen = self.live.get(&texture.id()).map_or(0, |l| l.generation);
            if seen == texture.generation() {
                continue;
            }
            let synced = texture.sync(seen);
            match synced.what() {
                Sync::Unchanged => {}
                Sync::Full {
                    width,
                    height,
                    pixels,
                } => {
                    if width == 0 || height == 0 || width > max || height > max {
                        self.live.remove(&texture.id());
                        continue;
                    }
                    let reuse = self
                        .live
                        .get(&texture.id())
                        .is_some_and(|l| l.size == [width, height]);
                    if !reuse {
                        let gpu = self.live_gpu(device, [width, height]);
                        self.live.insert(texture.id(), gpu);
                    }
                    let gpu = self.live.get_mut(&texture.id()).expect("just inserted");
                    uploads.texture(encoder, gpu.texture.as_image_copy(), width, height, pixels);
                    gpu.generation = synced.generation;
                }
                Sync::Damage {
                    width,
                    region,
                    pixels,
                } => {
                    let Some(gpu) = self.live.get_mut(&texture.id()) else {
                        continue;
                    };
                    if !region.is_empty() {
                        let row = width as usize * 4;
                        let x0 = region.x as usize * 4;
                        let x1 = x0 + region.width as usize * 4;
                        let mut bytes =
                            Vec::with_capacity(region.width as usize * region.height as usize * 4);
                        for y in region.y as usize..(region.y + region.height) as usize {
                            bytes.extend_from_slice(&pixels[y * row + x0..y * row + x1]);
                        }
                        uploads.texture(
                            encoder,
                            wgpu::TexelCopyTextureInfo {
                                texture: &gpu.texture,
                                mip_level: 0,
                                origin: wgpu::Origin3d {
                                    x: region.x,
                                    y: region.y,
                                    z: 0,
                                },
                                aspect: wgpu::TextureAspect::All,
                            },
                            region.width,
                            region.height,
                            &bytes,
                        );
                    }
                    gpu.generation = synced.generation;
                }
                // Imported without a copy once the renderer can import dmabufs; until then the
                // texture keeps the last pixels it had.
                Sync::Dmabuf(_) => {}
            }
        }
    }

    fn live_gpu(&self, device: &wgpu::Device, size: [u32; 2]) -> LiveGpu {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("GUI live texture"),
            size: wgpu::Extent3d {
                width: size[0],
                height: size[1],
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&Default::default());
        // Windows are drawn at about their own size: linear keeps scaled text readable.
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("GUI live texture"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            ..Default::default()
        });
        let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("GUI live texture"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.uniform.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&view),
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
        });
        LiveGpu {
            texture,
            group,
            size,
            generation: 0,
        }
    }
}

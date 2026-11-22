//! The GPU side of live textures (`horizon_live_texture`): one texture per live texture a frame
//! draws, in the HUD or the world, shared by both. Each frame uploads only what changed since the
//! texture was last seen, and drops the textures the frame didn't name.
use horizon_live_texture::{LiveTexture, Sync};
use std::collections::HashMap;

/// One live texture on the GPU.
pub struct Entry {
    texture: wgpu::Texture,
    pub view: wgpu::TextureView,
    size: [u32; 2],
    /// The live texture's generation this holds.
    generation: u64,
    /// Bumped when `texture` is replaced (a new size): bind groups made with an older view are
    /// stale.
    pub version: u64,
}

/// The live textures on the GPU, by id.
#[derive(Default)]
pub struct LiveTextures {
    entries: HashMap<u64, Entry>,
    versions: u64,
}

impl LiveTextures {
    /// Bring the GPU copies of `textures` (this frame's, repeats allowed) up to date and drop
    /// the others'.
    pub fn sync<'t>(
        &mut self,
        device: &wgpu::Device,
        uploads: &mut super::upload::Uploads,
        encoder: &mut wgpu::CommandEncoder,
        textures: impl IntoIterator<Item = &'t LiveTexture>,
    ) {
        let textures: Vec<&LiveTexture> = textures.into_iter().collect();
        self.entries
            .retain(|id, _| textures.iter().any(|t| t.id() == *id));
        let max = device.limits().max_texture_dimension_2d;
        for texture in textures {
            let seen = self.entries.get(&texture.id()).map_or(0, |e| e.generation);
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
                        self.entries.remove(&texture.id());
                        continue;
                    }
                    if self
                        .entries
                        .get(&texture.id())
                        .is_none_or(|e| e.size != [width, height])
                    {
                        self.versions += 1;
                        let entry = new_entry(device, [width, height], self.versions);
                        self.entries.insert(texture.id(), entry);
                    }
                    let entry = self.entries.get_mut(&texture.id()).expect("just inserted");
                    uploads.texture(encoder, entry.texture.as_image_copy(), width, height, pixels);
                    entry.generation = synced.generation;
                }
                Sync::Damage {
                    width,
                    region,
                    pixels,
                } => {
                    let Some(entry) = self.entries.get_mut(&texture.id()) else {
                        continue;
                    };
                    if !region.is_empty() {
                        let row = width as usize * 4;
                        let (x0, x1) = (region.x as usize * 4, (region.x + region.width) as usize * 4);
                        let mut bytes = Vec::with_capacity((x1 - x0) * region.height as usize);
                        for y in region.y as usize..(region.y + region.height) as usize {
                            bytes.extend_from_slice(&pixels[y * row + x0..y * row + x1]);
                        }
                        uploads.texture(
                            encoder,
                            wgpu::TexelCopyTextureInfo {
                                texture: &entry.texture,
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
                    entry.generation = synced.generation;
                }
                // Imported without a copy once the renderer imports dmabufs; until then the
                // texture keeps the last pixels it had.
                Sync::Dmabuf(_) => {}
            }
        }
    }

    /// The GPU copy of live texture `id`, if this frame has one.
    pub fn get(&self, id: u64) -> Option<&Entry> {
        self.entries.get(&id)
    }
}

fn new_entry(device: &wgpu::Device, size: [u32; 2], version: u64) -> Entry {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("Live texture"),
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
    Entry {
        texture,
        view,
        size,
        generation: 0,
        version,
    }
}

/// The sampler live textures draw with: windows are drawn near their own size, so linear keeps
/// scaled text readable.
pub fn sampler(device: &wgpu::Device) -> wgpu::Sampler {
    device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("Live texture"),
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        address_mode_u: wgpu::AddressMode::ClampToEdge,
        address_mode_v: wgpu::AddressMode::ClampToEdge,
        ..Default::default()
    })
}

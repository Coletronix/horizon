//! The GPU side of live textures (`horizon_live_texture`): one texture per live texture a frame
//! draws, in the HUD or the world, shared by both. Each frame uploads only what changed since the
//! texture was last seen, and drops the textures the frame didn't name.
//!
//! A dmabuf source is imported (`horizon_dmabuf`) and copied into the texture on the GPU, no CPU
//! copy: the copy also makes padding alpha opaque and flips a buffer stored upside down, and once
//! the GPU has done it the producer may reuse the buffer ([`LiveTexture::consumed`]). Imports are
//! kept per buffer, as producers cycle a few.
use horizon_live_texture::{Dmabuf, LiveTexture, Sync};
use std::collections::HashMap;
use std::sync::Arc;

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
    /// The producer's dmabufs imported so far, by buffer id, most recently used last.
    imports: Vec<(u64, wgpu::BindGroup)>,
}

/// Imports kept per live texture: producers cycle two to four buffers.
const IMPORTS: usize = 4;

/// The live textures on the GPU, by id.
#[derive(Default)]
pub struct LiveTextures {
    entries: HashMap<u64, Entry>,
    versions: u64,
    blit: Option<Blit>,
    /// Dmabuf copies recorded this frame, marked consumed once the GPU has run them.
    copied: Vec<(LiveTexture, u64)>,
    /// Producers' dmabufs the device couldn't import, logged once each.
    refused: std::collections::HashSet<u64>,
}

impl LiveTextures {
    /// Bring the GPU copies of `textures` (this frame's, repeats allowed) up to date and drop
    /// the others'.
    pub fn sync<'t>(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        uploads: &mut super::upload::Uploads,
        encoder: &mut wgpu::CommandEncoder,
        textures: impl IntoIterator<Item = &'t LiveTexture>,
    ) {
        // Last frame's copies were in the last submission: once it has run, their buffers are
        // free. This frame's are marked at the next frame's sync.
        if !self.copied.is_empty() {
            let copied = std::mem::take(&mut self.copied);
            queue.on_submitted_work_done(move || {
                for (texture, generation) in copied {
                    texture.mark_consumed(generation);
                }
            });
        }
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
                Sync::Dmabuf(buffer) => {
                    let buffer = Arc::clone(buffer);
                    let generation = synced.generation;
                    drop(synced);
                    if self.copy_dmabuf(device, encoder, texture.id(), &buffer, max) {
                        self.copied.push((texture.clone(), generation));
                    }
                    if let Some(entry) = self.entries.get_mut(&texture.id()) {
                        entry.generation = generation;
                    }
                }
            }
        }
    }

    /// Copy `buffer` into live texture `id`'s GPU texture, importing it unless it was before.
    /// False, keeping the texture as it was, when the device can't import it.
    fn copy_dmabuf(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        id: u64,
        buffer: &Dmabuf,
        max: u32,
    ) -> bool {
        let size = [buffer.width, buffer.height];
        if size.contains(&0) || size[0] > max || size[1] > max {
            return false;
        }
        let Some((_, opaque)) = horizon_dmabuf::texture_format(buffer.fourcc) else {
            return false;
        };
        if self.entries.get(&id).is_none_or(|e| e.size != size) {
            self.versions += 1;
            self.entries.insert(id, new_entry(device, size, self.versions));
        }
        let blit = self.blit.get_or_insert_with(|| Blit::new(device));
        let entry = self.entries.get_mut(&id).expect("just inserted");
        let at = entry.imports.iter().position(|(buffer_id, _)| *buffer_id == buffer.id);
        let group = match at {
            Some(at) => {
                let import = entry.imports.remove(at);
                entry.imports.push(import);
                &entry.imports.last().expect("just pushed").1
            }
            None => {
                let texture = match horizon_dmabuf::import(device, buffer) {
                    Ok(texture) => texture,
                    Err(error) => {
                        if self.refused.insert(buffer.id) {
                            log::warn!("Horizon: a live texture's dmabuf didn't import: {error}");
                        }
                        return false;
                    }
                };
                let view = texture.create_view(&Default::default());
                if entry.imports.len() == IMPORTS {
                    entry.imports.remove(0);
                }
                let group = blit.group(device, &view, opaque, buffer.y_inverted);
                entry.imports.push((buffer.id, group));
                &entry.imports.last().expect("just pushed").1
            }
        };
        blit.copy(encoder, &entry.view, group);
        true
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
        usage: wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_DST
            | wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let view = texture.create_view(&Default::default());
    Entry {
        texture,
        view,
        size,
        generation: 0,
        version,
        imports: Vec::new(),
    }
}

/// The pass copying an imported buffer into a live texture's own: a full-screen triangle.
struct Blit {
    layout: wgpu::BindGroupLayout,
    pipeline: wgpu::RenderPipeline,
    sampler: wgpu::Sampler,
}

const BLIT: &str = "
struct Out { @builtin(position) clip: vec4<f32>, @location(0) uv: vec2<f32> }
@group(0) @binding(0) var source: texture_2d<f32>;
@group(0) @binding(1) var source_sampler: sampler;
// x: 1 makes alpha opaque (a padding byte); y: 1 flips the buffer vertically.
@group(0) @binding(2) var<uniform> flags: vec4<u32>;
@vertex fn vs(@builtin(vertex_index) i: u32) -> Out {
    let uv = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    var out: Out;
    out.clip = vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
    out.uv = uv;
    return out;
}
@fragment fn fs(in: Out) -> @location(0) vec4<f32> {
    var uv = in.uv;
    if flags.y == 1u { uv.y = 1.0 - uv.y; }
    let c = textureSampleLevel(source, source_sampler, uv, 0.0);
    return vec4<f32>(c.rgb, select(c.a, 1.0, flags.x == 1u));
}
";

impl Blit {
    fn new(device: &wgpu::Device) -> Self {
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("Live texture copy"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::NonFiltering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("Live texture copy"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("Live texture copy"),
            source: wgpu::ShaderSource::Wgsl(BLIT.into()),
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("Live texture copy"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        // The copy is texel for texel: nearest.
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("Live texture copy"),
            ..Default::default()
        });
        Self {
            layout,
            pipeline,
            sampler,
        }
    }

    /// The bind group copying from `view`, a buffer whose flags don't change.
    fn group(
        &self,
        device: &wgpu::Device,
        view: &wgpu::TextureView,
        opaque: bool,
        flip: bool,
    ) -> wgpu::BindGroup {
        use wgpu::util::DeviceExt;
        let flags = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Live texture copy flags"),
            contents: bytemuck::cast_slice(&[u32::from(opaque), u32::from(flip), 0, 0]),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("Live texture copy"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: flags.as_entire_binding(),
                },
            ],
        })
    }

    fn copy(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        source: &wgpu::BindGroup,
    ) {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("Live texture copy"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, source, &[]);
        pass.draw(0..3, 0..1);
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

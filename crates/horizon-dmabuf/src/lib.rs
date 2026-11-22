//! Linux dmabufs into the renderer without a CPU copy.
//!
//! A producer (a mod's Wayland compositor) hands the renderer a GPU buffer by file descriptor
//! ([`horizon_live_texture::Dmabuf`]). This crate tells producers what the renderer can import
//! ([`caps`]: the GPU's DRM node and the single-plane formats and modifiers Vulkan reads), and
//! imports a buffer as a `wgpu::Texture` ([`import`]). Vulkan only, Linux only: elsewhere every
//! function answers that it can't.
//!
//! The unsafe Vulkan calls live here, as `horizon-hdr` keeps its own, so the renderer stays free
//! of unsafe code.

use horizon_live_texture::{Dmabuf, DmabufCaps};

/// The device features dmabuf import needs, when `adapter` has them.
pub fn features(adapter: &wgpu::Adapter) -> wgpu::Features {
    #[cfg(target_os = "linux")]
    {
        adapter.features() & wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = adapter;
        wgpu::Features::empty()
    }
}

/// The wgpu format a fourcc's buffer is read as, and whether its alpha byte is padding.
pub fn texture_format(fourcc: u32) -> Option<(TextureFormat, bool)> {
    use horizon_live_texture::fourcc::*;
    match fourcc {
        ARGB8888 => Some((TextureFormat::Bgra8, false)),
        XRGB8888 => Some((TextureFormat::Bgra8, true)),
        ABGR8888 => Some((TextureFormat::Rgba8, false)),
        XBGR8888 => Some((TextureFormat::Rgba8, true)),
        _ => None,
    }
}

/// The two byte orders an imported buffer is read in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextureFormat {
    Bgra8,
    Rgba8,
}

/// What dmabufs `device` imports, or `None` when it imports none.
pub fn caps(device: &wgpu::Device) -> Option<DmabufCaps> {
    #[cfg(target_os = "linux")]
    {
        vulkan::caps(device)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = device;
        None
    }
}

/// Import `buffer` into `device` as a sampled texture of its size. The texture reads the
/// buffer's memory: the producer must not reuse the buffer while the GPU reads it.
pub fn import(device: &wgpu::Device, buffer: &Dmabuf) -> Result<wgpu::Texture, String> {
    #[cfg(target_os = "linux")]
    {
        vulkan::import(device, buffer)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (device, buffer);
        Err("dmabufs are Linux's".into())
    }
}

#[cfg(target_os = "linux")]
mod vulkan {
    use super::{TextureFormat, texture_format};
    use ash::vk;
    use horizon_live_texture::{Dmabuf, DmabufCaps, DmabufFormat, fourcc};
    use wgpu::hal::api::Vulkan;

    fn vk_format(format: TextureFormat) -> vk::Format {
        match format {
            TextureFormat::Bgra8 => vk::Format::B8G8R8A8_UNORM,
            TextureFormat::Rgba8 => vk::Format::R8G8B8A8_UNORM,
        }
    }

    pub(super) fn caps(device: &wgpu::Device) -> Option<DmabufCaps> {
        if !device
            .features()
            .contains(wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF)
        {
            return None;
        }
        // Safety: only read from here, and dropped before this returns.
        let hal = unsafe { device.as_hal::<Vulkan>() }?;
        let instance = hal.shared_instance().raw_instance();
        let physical = hal.raw_physical_device();
        let node = drm_node(instance, physical)?;
        let mut formats = Vec::new();
        for code in [
            fourcc::ARGB8888,
            fourcc::XRGB8888,
            fourcc::ABGR8888,
            fourcc::XBGR8888,
        ] {
            let (format, _) = texture_format(code)?;
            for modifier in importable_modifiers(instance, physical, vk_format(format)) {
                formats.push(DmabufFormat {
                    fourcc: code,
                    modifier,
                });
            }
        }
        if formats.is_empty() {
            return None;
        }
        Some(DmabufCaps {
            device: node,
            formats,
        })
    }

    /// The GPU's DRM node as a `dev_t`: its render node, else its primary node.
    fn drm_node(instance: &ash::Instance, physical: vk::PhysicalDevice) -> Option<u64> {
        // Safety: the device's own physical device on its own instance.
        let offered = unsafe { instance.enumerate_device_extension_properties(physical) }.ok()?;
        if !offered
            .iter()
            .any(|e| e.extension_name_as_c_str() == Ok(ash::ext::physical_device_drm::NAME))
        {
            log::info!(
                "Horizon dmabuf: the GPU doesn't name its DRM node (VK_EXT_physical_device_drm)"
            );
            return None;
        }
        let mut drm = vk::PhysicalDeviceDrmPropertiesEXT::default();
        let mut properties = vk::PhysicalDeviceProperties2::default().push_next(&mut drm);
        // Safety: as above; the extension is offered, so its structure may be chained.
        unsafe { instance.get_physical_device_properties2(physical, &mut properties) };
        let (major, minor) = if drm.has_render == vk::TRUE {
            (drm.render_major, drm.render_minor)
        } else if drm.has_primary == vk::TRUE {
            (drm.primary_major, drm.primary_minor)
        } else {
            return None;
        };
        Some(libc::makedev(major as u32, minor as u32))
    }

    /// The single-plane modifiers of `format` that can be imported from a dmabuf and sampled.
    fn importable_modifiers(
        instance: &ash::Instance,
        physical: vk::PhysicalDevice,
        format: vk::Format,
    ) -> Vec<u64> {
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
        let mut properties = vk::FormatProperties2::default().push_next(&mut list);
        // Safety: the device's own physical device; the list asks for its count first.
        unsafe {
            instance.get_physical_device_format_properties2(physical, format, &mut properties)
        };
        let count = list.drm_format_modifier_count as usize;
        let mut modifiers = vec![vk::DrmFormatModifierPropertiesEXT::default(); count];
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default()
            .drm_format_modifier_properties(&mut modifiers);
        let mut properties = vk::FormatProperties2::default().push_next(&mut list);
        // Safety: as above, with room for every modifier.
        unsafe {
            instance.get_physical_device_format_properties2(physical, format, &mut properties)
        };
        modifiers
            .iter()
            .filter(|m| {
                m.drm_format_modifier_plane_count == 1
                    && m.drm_format_modifier_tiling_features
                        .contains(vk::FormatFeatureFlags::SAMPLED_IMAGE)
            })
            .map(|m| m.drm_format_modifier)
            .filter(|modifier| importable(instance, physical, format, *modifier))
            .collect()
    }

    fn importable(
        instance: &ash::Instance,
        physical: vk::PhysicalDevice,
        format: vk::Format,
        modifier: u64,
    ) -> bool {
        let mut drm = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
            .drm_format_modifier(modifier)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let mut external = vk::PhysicalDeviceExternalImageFormatInfo::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let info = vk::PhysicalDeviceImageFormatInfo2::default()
            .format(format)
            .ty(vk::ImageType::TYPE_2D)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(vk::ImageUsageFlags::SAMPLED)
            .push_next(&mut external)
            .push_next(&mut drm);
        let mut external_properties = vk::ExternalImageFormatProperties::default();
        let mut properties =
            vk::ImageFormatProperties2::default().push_next(&mut external_properties);
        // Safety: the device's own physical device; every chained structure is the extension's.
        let ok = unsafe {
            instance.get_physical_device_image_format_properties2(physical, &info, &mut properties)
        }
        .is_ok();
        ok && external_properties
            .external_memory_properties
            .external_memory_features
            .contains(vk::ExternalMemoryFeatureFlags::IMPORTABLE)
    }

    pub(super) fn import(device: &wgpu::Device, buffer: &Dmabuf) -> Result<wgpu::Texture, String> {
        let [plane] = buffer.planes.as_slice() else {
            return Err(format!(
                "{} planes; only single-plane buffers import",
                buffer.planes.len()
            ));
        };
        let (format, _) = texture_format(buffer.fourcc)
            .ok_or_else(|| format!("fourcc {:#010x} isn't imported", buffer.fourcc))?;
        let format = match format {
            TextureFormat::Bgra8 => wgpu::TextureFormat::Bgra8Unorm,
            TextureFormat::Rgba8 => wgpu::TextureFormat::Rgba8Unorm,
        };
        let size = wgpu::Extent3d {
            width: buffer.width,
            height: buffer.height,
            depth_or_array_layers: 1,
        };
        let fd = plane
            .fd
            .try_clone()
            .map_err(|e| format!("the buffer's descriptor: {e}"))?;
        let hal_desc = wgpu::hal::TextureDescriptor {
            label: Some("Imported dmabuf"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUses::RESOURCE,
            memory_flags: wgpu::hal::MemoryFlags::empty(),
            view_formats: Vec::new(),
        };
        let hal_texture = {
            // Safety: read here only, and dropped before wgpu takes the texture.
            let hal = unsafe { device.as_hal::<Vulkan>() }.ok_or("not a Vulkan device")?;
            // Safety: the device has VULKAN_EXTERNAL_MEMORY_DMA_BUF (checked by the callee); the
            // descriptor, modifier, stride and offset are the producer's for this buffer.
            unsafe {
                hal.texture_from_dmabuf_fd(
                    fd,
                    &hal_desc,
                    buffer.modifier,
                    u64::from(plane.stride),
                    u64::from(plane.offset),
                )
            }
            .map_err(|e| format!("import: {e}"))?
        };
        let desc = wgpu::TextureDescriptor {
            label: Some("Imported dmabuf"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        };
        // Safety: made on this device from `desc`'s own size, format and usage. Its contents are
        // the producer's, so it is wrapped as initialized and sampled where it lies: wgpu would
        // clear an uninitialized texture before its first use.
        Ok(unsafe {
            device.create_texture_from_hal::<Vulkan>(
                hal_texture,
                &desc,
                wgpu::TextureUses::RESOURCE,
            )
        })
    }
}

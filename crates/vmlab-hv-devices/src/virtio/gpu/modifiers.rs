//! Vulkan import capabilities for the render node used by virglrenderer.

use ash::vk;
use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

pub(super) fn query(node: &Path) -> BTreeMap<u32, Vec<u64>> {
    match importable(node) {
        Ok(formats) => formats,
        Err(error) => {
            log::warn!("Vulkan modifier query for {}: {error}", node.display());
            BTreeMap::new()
        }
    }
}

fn importable(node: &Path) -> Result<BTreeMap<u32, Vec<u64>>, Box<dyn std::error::Error>> {
    let dev = std::fs::metadata(node)?.rdev();
    // SAFETY: Vulkan loader calls use valid structures and counts supplied by ash.
    // This creates no Vulkan device and performs no rendering or host modesetting.
    unsafe {
        let entry = ash::Entry::load()?;
        let app = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_1);
        let instance = entry.create_instance(
            &vk::InstanceCreateInfo::default().application_info(&app),
            None,
        )?;
        let result = (|| {
            let physical = instance
                .enumerate_physical_devices()?
                .into_iter()
                .find(|physical| {
                    let mut drm = vk::PhysicalDeviceDrmPropertiesEXT::default();
                    let mut props = vk::PhysicalDeviceProperties2::default().push_next(&mut drm);
                    instance.get_physical_device_properties2(*physical, &mut props);
                    drm.has_render != 0
                        && drm.render_major == libc::major(dev) as i64
                        && drm.render_minor == libc::minor(dev) as i64
                })
                .ok_or("no Vulkan physical device matches the render node")?;
            let mut formats = BTreeMap::new();
            for (fourcc, format) in [
                (*b"XR24", vk::Format::B8G8R8A8_UNORM),
                (*b"AR24", vk::Format::B8G8R8A8_UNORM),
                (*b"XB24", vk::Format::R8G8B8A8_UNORM),
                (*b"AB24", vk::Format::R8G8B8A8_UNORM),
            ] {
                let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
                let mut props = vk::FormatProperties2::default().push_next(&mut list);
                instance.get_physical_device_format_properties2(physical, format, &mut props);
                let mut modifiers = vec![
                    vk::DrmFormatModifierPropertiesEXT::default();
                    list.drm_format_modifier_count as usize
                ];
                let mut list = vk::DrmFormatModifierPropertiesListEXT::default()
                    .drm_format_modifier_properties(&mut modifiers);
                let mut props = vk::FormatProperties2::default().push_next(&mut list);
                instance.get_physical_device_format_properties2(physical, format, &mut props);
                let modifiers = modifiers
                    .into_iter()
                    .filter(|modifier| {
                        if modifier.drm_format_modifier_plane_count != 1 {
                            return false;
                        }
                        let mut drm = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
                            .drm_format_modifier(modifier.drm_format_modifier)
                            .sharing_mode(vk::SharingMode::EXCLUSIVE);
                        let mut external = vk::PhysicalDeviceExternalImageFormatInfo::default()
                            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
                        let info = vk::PhysicalDeviceImageFormatInfo2::default()
                            .format(format)
                            .ty(vk::ImageType::TYPE_2D)
                            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                            .usage(
                                vk::ImageUsageFlags::SAMPLED
                                    | vk::ImageUsageFlags::TRANSFER_SRC
                                    | vk::ImageUsageFlags::TRANSFER_DST,
                            )
                            .push_next(&mut drm)
                            .push_next(&mut external);
                        let mut external = vk::ExternalImageFormatProperties::default();
                        let mut props =
                            vk::ImageFormatProperties2::default().push_next(&mut external);
                        instance
                            .get_physical_device_image_format_properties2(
                                physical, &info, &mut props,
                            )
                            .is_ok()
                            && external
                                .external_memory_properties
                                .external_memory_features
                                .contains(vk::ExternalMemoryFeatureFlags::IMPORTABLE)
                    })
                    .map(|modifier| modifier.drm_format_modifier)
                    .collect();
                formats.insert(u32::from_le_bytes(fourcc), modifiers);
            }
            Ok(formats)
        })();
        instance.destroy_instance(None);
        result
    }
}

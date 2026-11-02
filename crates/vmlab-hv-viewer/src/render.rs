//! Vulkan presentation with one submission in flight and cached dma-buf imports.

use crate::{Frame, Surface, input::viewport};
use anyhow::{Context, Result, ensure};
use ash::{Entry, Instance, vk};
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use std::collections::HashMap;
use std::ffi::CStr;
use std::io::Cursor;
use std::os::fd::{AsRawFd, IntoRawFd};
use std::rc::Rc;
use vmlab_hv_display::{Rect, SurfaceKind};
use winit::window::Window;

struct Gpu {
    _entry: Entry,
    instance: Instance,
    physical: vk::PhysicalDevice,
    device: ash::Device,
    queue: vk::Queue,
    family: u32,
    surface_api: ash::khr::surface::Instance,
    surface: vk::SurfaceKHR,
    dmabuf: bool,
}

impl Gpu {
    fn new(window: Option<&Window>, allow_dmabuf: bool) -> Result<Rc<Self>> {
        // SAFETY: the loader is kept alive by Gpu until every Vulkan object is gone.
        unsafe {
            let entry = Entry::load().context("load Vulkan (install a Vulkan ICD)")?;
            let extensions = match window {
                Some(w) => ash_window::enumerate_required_extensions(w.display_handle()?.as_raw())?
                    .to_vec(),
                None => Vec::new(),
            };
            let application = vk::ApplicationInfo::default()
                .application_name(c"vmlab-hv-viewer")
                .api_version(vk::API_VERSION_1_1);
            let instance = entry.create_instance(
                &vk::InstanceCreateInfo::default()
                    .application_info(&application)
                    .enabled_extension_names(&extensions),
                None,
            )?;
            let surface_api = ash::khr::surface::Instance::new(&entry, &instance);
            let surface = if let Some(w) = window {
                match ash_window::create_surface(
                    &entry,
                    &instance,
                    w.display_handle()?.as_raw(),
                    w.window_handle()?.as_raw(),
                    None,
                ) {
                    Ok(s) => s,
                    Err(error) => {
                        instance.destroy_instance(None);
                        return Err(error.into());
                    }
                }
            } else {
                vk::SurfaceKHR::null()
            };
            let result = (|| -> Result<_> {
                let mut candidates = Vec::new();
                for physical in instance.enumerate_physical_devices()? {
                    for (family, properties) in instance
                        .get_physical_device_queue_family_properties(physical)
                        .iter()
                        .enumerate()
                    {
                        if properties.queue_flags.contains(vk::QueueFlags::GRAPHICS)
                            && (surface == vk::SurfaceKHR::null()
                                || surface_api.get_physical_device_surface_support(
                                    physical,
                                    family as u32,
                                    surface,
                                )?)
                        {
                            let props = instance.get_physical_device_properties(physical);
                            let rank = match props.device_type {
                                vk::PhysicalDeviceType::DISCRETE_GPU => 0,
                                vk::PhysicalDeviceType::INTEGRATED_GPU => 1,
                                _ => 2,
                            };
                            candidates.push((rank, physical, family as u32));
                            break;
                        }
                    }
                }
                candidates.sort_by_key(|c| c.0);
                let (_, physical, family) = candidates
                    .first()
                    .copied()
                    .context("no Vulkan graphics/present queue")?;
                let available = instance.enumerate_device_extension_properties(physical)?;
                let has = |name: &CStr| {
                    available
                        .iter()
                        .any(|p| CStr::from_ptr(p.extension_name.as_ptr()) == name)
                };
                let dma_extensions = [
                    ash::khr::external_memory_fd::NAME,
                    ash::ext::external_memory_dma_buf::NAME,
                    ash::ext::image_drm_format_modifier::NAME,
                    ash::ext::queue_family_foreign::NAME,
                ];
                let dmabuf = allow_dmabuf && dma_extensions.iter().all(|name| has(name));
                let mut extensions = Vec::new();
                if surface != vk::SurfaceKHR::null() {
                    extensions.push(ash::khr::swapchain::NAME.as_ptr());
                }
                if dmabuf {
                    extensions.extend(dma_extensions.iter().map(|name| name.as_ptr()));
                }
                let priorities = [1.];
                let queues = [vk::DeviceQueueCreateInfo::default()
                    .queue_family_index(family)
                    .queue_priorities(&priorities)];
                let device = instance.create_device(
                    physical,
                    &vk::DeviceCreateInfo::default()
                        .queue_create_infos(&queues)
                        .enabled_extension_names(&extensions),
                    None,
                )?;
                let queue = device.get_device_queue(family, 0);
                Ok((physical, family, dmabuf, device, queue))
            })();
            let (physical, family, dmabuf, device, queue) = match result {
                Ok(result) => result,
                Err(error) => {
                    if surface != vk::SurfaceKHR::null() {
                        surface_api.destroy_surface(surface, None);
                    }
                    instance.destroy_instance(None);
                    return Err(error);
                }
            };
            Ok(Rc::new(Self {
                _entry: entry,
                instance,
                physical,
                device,
                queue,
                family,
                surface_api,
                surface,
                dmabuf,
            }))
        }
    }

    fn memory_type(&self, bits: u32, flags: vk::MemoryPropertyFlags) -> Result<u32> {
        // SAFETY: physical belongs to the live instance.
        let memory = unsafe {
            self.instance
                .get_physical_device_memory_properties(self.physical)
        };
        (0..memory.memory_type_count)
            .find(|i| {
                bits & (1 << i) != 0
                    && memory.memory_types[*i as usize]
                        .property_flags
                        .contains(flags)
            })
            .context("no compatible Vulkan memory type")
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        // SAFETY: all resource owners retain an Rc<Gpu>; they have been destroyed.
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_device(None);
            if self.surface != vk::SurfaceKHR::null() {
                self.surface_api.destroy_surface(self.surface, None);
            }
            self.instance.destroy_instance(None);
        }
    }
}

struct Image {
    gpu: Rc<Gpu>,
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    width: u32,
    height: u32,
    layout: vk::ImageLayout,
}

impl Image {
    fn new(
        gpu: Rc<Gpu>,
        width: u32,
        height: u32,
        format: vk::Format,
        surface: Option<(&Surface, u32)>,
        opaque: bool,
    ) -> Result<Self> {
        let mut image = Self {
            gpu,
            image: vk::Image::null(),
            memory: vk::DeviceMemory::null(),
            view: vk::ImageView::null(),
            width,
            height,
            layout: vk::ImageLayout::UNDEFINED,
        };
        // SAFETY: create infos borrow live local arrays; image owns each handle immediately.
        unsafe {
            let device = &image.gpu.device;
            let mut info = vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::TYPE_2D)
                .format(format)
                .extent(vk::Extent3D {
                    width,
                    height,
                    depth: 1,
                })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .usage(
                    vk::ImageUsageFlags::SAMPLED
                        | vk::ImageUsageFlags::TRANSFER_DST
                        | vk::ImageUsageFlags::TRANSFER_SRC,
                )
                .tiling(vk::ImageTiling::OPTIMAL);
            let mut external = vk::ExternalMemoryImageCreateInfo::default()
                .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
            let planes;
            let mut modifier;
            if let Some((surface, _)) = surface {
                ensure!(image.gpu.dmabuf, "Vulkan dma-buf import is unavailable");
                // Query the exact modifier, format, usage, and external handle combination.
                let mut drm = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
                    .drm_format_modifier(surface.modifier)
                    .sharing_mode(vk::SharingMode::EXCLUSIVE);
                let mut ext = vk::PhysicalDeviceExternalImageFormatInfo::default()
                    .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
                let query = vk::PhysicalDeviceImageFormatInfo2::default()
                    .format(format)
                    .ty(vk::ImageType::TYPE_2D)
                    .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                    .usage(info.usage)
                    .push_next(&mut drm)
                    .push_next(&mut ext);
                let mut properties = vk::ExternalImageFormatProperties::default();
                let mut output = vk::ImageFormatProperties2::default().push_next(&mut properties);
                image
                    .gpu
                    .instance
                    .get_physical_device_image_format_properties2(
                        image.gpu.physical,
                        &query,
                        &mut output,
                    )?;
                ensure!(
                    properties
                        .external_memory_properties
                        .external_memory_features
                        .contains(vk::ExternalMemoryFeatureFlags::IMPORTABLE),
                    "DRM modifier is not importable"
                );
                let mut modifier_list = vk::DrmFormatModifierPropertiesListEXT::default();
                let mut props = vk::FormatProperties2::default().push_next(&mut modifier_list);
                image.gpu.instance.get_physical_device_format_properties2(
                    image.gpu.physical,
                    format,
                    &mut props,
                );
                let mut modifiers = vec![
                    vk::DrmFormatModifierPropertiesEXT::default();
                    modifier_list.drm_format_modifier_count as usize
                ];
                modifier_list = vk::DrmFormatModifierPropertiesListEXT::default()
                    .drm_format_modifier_properties(&mut modifiers);
                props = vk::FormatProperties2::default().push_next(&mut modifier_list);
                image.gpu.instance.get_physical_device_format_properties2(
                    image.gpu.physical,
                    format,
                    &mut props,
                );
                ensure!(
                    modifiers
                        .iter()
                        .any(|m| m.drm_format_modifier == surface.modifier
                            && m.drm_format_modifier_plane_count == 1),
                    "modifier needs plane layouts the display protocol cannot represent"
                );
                planes = [vk::SubresourceLayout::default()
                    .offset(0)
                    .row_pitch(surface.stride as u64)];
                modifier = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
                    .drm_format_modifier(surface.modifier)
                    .plane_layouts(&planes);
                info = info
                    .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                    .push_next(&mut external)
                    .push_next(&mut modifier);
            }
            image.image = device.create_image(&info, None)?;
            let requirements = device.get_image_memory_requirements(image.image);
            let mut bits = requirements.memory_type_bits;
            let mut import;
            let fd;
            let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image.image);
            let mut allocation = vk::MemoryAllocateInfo::default()
                .allocation_size(requirements.size)
                .push_next(&mut dedicated);
            if let Some((surface, buffer)) = surface {
                fd = surface.buffers[buffer as usize].try_clone()?;
                let api = ash::khr::external_memory_fd::Device::new(&image.gpu.instance, device);
                let mut props = vk::MemoryFdPropertiesKHR::default();
                api.get_memory_fd_properties(
                    vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
                    fd.as_raw_fd(),
                    &mut props,
                )?;
                bits &= props.memory_type_bits;
                import = vk::ImportMemoryFdInfoKHR::default()
                    .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
                    .fd(fd.as_raw_fd());
                allocation = allocation
                    .memory_type_index(
                        image
                            .gpu
                            .memory_type(bits, vk::MemoryPropertyFlags::empty())?,
                    )
                    .push_next(&mut import);
                image.memory = device.allocate_memory(&allocation, None)?;
                // A successful import transfers ownership of the duplicate descriptor to Vulkan.
                let _ = fd.into_raw_fd();
                image.layout = vk::ImageLayout::GENERAL;
            } else {
                allocation = allocation.memory_type_index(
                    image
                        .gpu
                        .memory_type(bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)?,
                );
                image.memory = device.allocate_memory(&allocation, None)?;
            }
            device.bind_image_memory(image.image, image.memory, 0)?;
            let components = vk::ComponentMapping {
                a: if opaque {
                    vk::ComponentSwizzle::ONE
                } else {
                    vk::ComponentSwizzle::IDENTITY
                },
                ..Default::default()
            };
            image.view = device.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image.image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(format)
                    .components(components)
                    .subresource_range(range()),
                None,
            )?;
        }
        Ok(image)
    }

    fn barrier(
        &mut self,
        command: vk::CommandBuffer,
        layout: vk::ImageLayout,
        acquire: bool,
        release: bool,
    ) {
        let foreign = vk::QUEUE_FAMILY_FOREIGN_EXT;
        let barrier = vk::ImageMemoryBarrier::default()
            .image(self.image)
            .subresource_range(range())
            .old_layout(self.layout)
            .new_layout(layout)
            .src_access_mask(if acquire || self.layout == vk::ImageLayout::UNDEFINED {
                vk::AccessFlags::empty()
            } else {
                vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE
            })
            .dst_access_mask(if release {
                vk::AccessFlags::empty()
            } else {
                vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE
            })
            .src_queue_family_index(if acquire {
                foreign
            } else if release {
                self.gpu.family
            } else {
                vk::QUEUE_FAMILY_IGNORED
            })
            .dst_queue_family_index(if acquire {
                self.gpu.family
            } else if release {
                foreign
            } else {
                vk::QUEUE_FAMILY_IGNORED
            });
        // SAFETY: the command is recording; this image is not used by another submission.
        unsafe {
            self.gpu.device.cmd_pipeline_barrier(
                command,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[barrier],
            );
        }
        self.layout = layout;
    }
}

impl Drop for Image {
    fn drop(&mut self) {
        // SAFETY: Renderer waits for each submission before returning or evicting images.
        unsafe {
            self.gpu.device.destroy_image_view(self.view, None);
            self.gpu.device.destroy_image(self.image, None);
            self.gpu.device.free_memory(self.memory, None);
        }
    }
}

fn range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .level_count(1)
        .layer_count(1)
}
fn layers() -> vk::ImageSubresourceLayers {
    vk::ImageSubresourceLayers::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .layer_count(1)
}

struct Buffer {
    gpu: Rc<Gpu>,
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    #[cfg(test)]
    size: usize,
}

impl Buffer {
    fn new(gpu: Rc<Gpu>, bytes: &[u8]) -> Result<Self> {
        let mut result = Self {
            gpu,
            buffer: vk::Buffer::null(),
            memory: vk::DeviceMemory::null(),
            #[cfg(test)]
            size: bytes.len(),
        };
        // SAFETY: handles are owned by result, sizes match the byte slice, memory is coherent.
        unsafe {
            let device = &result.gpu.device;
            result.buffer = device.create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(bytes.len() as u64)
                    .usage(vk::BufferUsageFlags::TRANSFER_SRC | vk::BufferUsageFlags::TRANSFER_DST),
                None,
            )?;
            let requirements = device.get_buffer_memory_requirements(result.buffer);
            let kind = result.gpu.memory_type(
                requirements.memory_type_bits,
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
            )?;
            result.memory = device.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(requirements.size)
                    .memory_type_index(kind),
                None,
            )?;
            device.bind_buffer_memory(result.buffer, result.memory, 0)?;
            let mapped = device.map_memory(
                result.memory,
                0,
                bytes.len() as u64,
                vk::MemoryMapFlags::empty(),
            )?;
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), mapped.cast(), bytes.len());
            device.unmap_memory(result.memory);
        }
        Ok(result)
    }

    #[cfg(test)]
    fn read(&self) -> Result<Vec<u8>> {
        // SAFETY: the test waits for its GPU submission; the memory is host coherent.
        unsafe {
            let mapped = self.gpu.device.map_memory(
                self.memory,
                0,
                self.size as u64,
                vk::MemoryMapFlags::empty(),
            )?;
            let bytes = std::slice::from_raw_parts(mapped.cast::<u8>(), self.size).to_vec();
            self.gpu.device.unmap_memory(self.memory);
            Ok(bytes)
        }
    }
}
impl Drop for Buffer {
    fn drop(&mut self) {
        // SAFETY: staging buffers are retained until the submission's fence signals.
        unsafe {
            self.gpu.device.destroy_buffer(self.buffer, None);
            self.gpu.device.free_memory(self.memory, None);
        }
    }
}

/// Pack only the damaged rows, preserving four-byte alignment for Vulkan copies.
pub fn pack_damage(
    pixels: &[u8],
    stride: u32,
    damage: &[Rect],
) -> (Vec<u8>, Vec<vk::BufferImageCopy>) {
    let mut bytes = Vec::new();
    let mut copies = Vec::new();
    for rect in damage {
        copies.push(
            vk::BufferImageCopy::default()
                .buffer_offset(bytes.len() as u64)
                .image_subresource(layers())
                .image_offset(vk::Offset3D {
                    x: rect.x as i32,
                    y: rect.y as i32,
                    z: 0,
                })
                .image_extent(vk::Extent3D {
                    width: rect.width,
                    height: rect.height,
                    depth: 1,
                }),
        );
        for y in rect.y..rect.y + rect.height {
            let offset = (y * stride + rect.x * 4) as usize;
            bytes.extend_from_slice(&pixels[offset..offset + rect.width as usize * 4]);
        }
    }
    (bytes, copies)
}

struct Swapchain {
    gpu: Rc<Gpu>,
    api: ash::khr::swapchain::Device,
    handle: vk::SwapchainKHR,
    extent: vk::Extent2D,
    views: Vec<vk::ImageView>,
    framebuffers: Vec<vk::Framebuffer>,
    render_pass: vk::RenderPass,
    pipeline: vk::Pipeline,
    finished: Vec<vk::Semaphore>,
}

impl Drop for Swapchain {
    fn drop(&mut self) {
        // SAFETY: recreation and Renderer::drop wait for the presentation queue to idle.
        unsafe {
            let device = &self.gpu.device;
            for semaphore in &self.finished {
                device.destroy_semaphore(*semaphore, None);
            }
            for framebuffer in &self.framebuffers {
                device.destroy_framebuffer(*framebuffer, None);
            }
            device.destroy_pipeline(self.pipeline, None);
            device.destroy_render_pass(self.render_pass, None);
            for view in &self.views {
                device.destroy_image_view(*view, None);
            }
            self.api.destroy_swapchain(self.handle, None);
        }
    }
}

pub struct Renderer {
    gpu: Rc<Gpu>,
    swapchain: Option<Swapchain>,
    pool: vk::CommandPool,
    command: vk::CommandBuffer,
    fence: vk::Fence,
    acquired: vk::Semaphore,
    descriptors: vk::DescriptorPool,
    set_layout: vk::DescriptorSetLayout,
    pipeline_layout: vk::PipelineLayout,
    sampler: vk::Sampler,
    imports: HashMap<(u64, u32), Image>,
    repaint: Option<Image>,
    cursor: Option<Image>,
    cursor_generation: u64,
    surface: Option<u64>,
    dirty: bool,
    occluded: bool,
}

impl Renderer {
    pub fn new(window: &Window, dmabuf: bool) -> Result<Self> {
        let gpu = Gpu::new(Some(window), dmabuf)?;
        Self::with_gpu(gpu)
    }

    fn with_gpu(gpu: Rc<Gpu>) -> Result<Self> {
        let mut renderer = Self {
            gpu,
            swapchain: None,
            pool: vk::CommandPool::null(),
            command: vk::CommandBuffer::null(),
            fence: vk::Fence::null(),
            acquired: vk::Semaphore::null(),
            descriptors: vk::DescriptorPool::null(),
            set_layout: vk::DescriptorSetLayout::null(),
            pipeline_layout: vk::PipelineLayout::null(),
            sampler: vk::Sampler::null(),
            imports: HashMap::new(),
            repaint: None,
            cursor: None,
            cursor_generation: 0,
            surface: None,
            dirty: true,
            occluded: false,
        };
        // SAFETY: created handles are stored before any subsequent fallible operation.
        unsafe {
            let device = &renderer.gpu.device;
            renderer.pool = device.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .queue_family_index(renderer.gpu.family)
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                None,
            )?;
            renderer.command = device.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(renderer.pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            )?[0];
            renderer.fence = device.create_fence(&vk::FenceCreateInfo::default(), None)?;
            renderer.acquired =
                device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None)?;
            let bindings = [vk::DescriptorSetLayoutBinding::default()
                .binding(0)
                .descriptor_count(1)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT)];
            renderer.set_layout = device.create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
                None,
            )?;
            let layouts = [renderer.set_layout];
            renderer.pipeline_layout = device.create_pipeline_layout(
                &vk::PipelineLayoutCreateInfo::default().set_layouts(&layouts),
                None,
            )?;
            let sizes = [vk::DescriptorPoolSize {
                ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                descriptor_count: 4,
            }];
            renderer.descriptors = device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(4)
                    .pool_sizes(&sizes),
                None,
            )?;
            renderer.sampler = device.create_sampler(
                &vk::SamplerCreateInfo::default()
                    .min_filter(vk::Filter::LINEAR)
                    .mag_filter(vk::Filter::LINEAR)
                    .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .max_lod(0.),
                None,
            )?;
        }
        Ok(renderer)
    }

    pub fn dmabuf(&self) -> bool {
        self.gpu.dmabuf
    }
    pub fn resize(&mut self) {
        self.dirty = true;
    }

    pub fn occluded(&mut self, occluded: bool) {
        self.occluded = occluded;
    }

    fn recreate(&mut self, width: u32, height: u32) -> Result<()> {
        // SAFETY: all handles belong to this device; queue idle permits destruction.
        unsafe {
            self.gpu.device.device_wait_idle()?;
            self.swapchain.take();
            let gpu = self.gpu.clone();
            let caps = gpu
                .surface_api
                .get_physical_device_surface_capabilities(gpu.physical, gpu.surface)?;
            ensure!(
                caps.supported_usage_flags
                    .contains(vk::ImageUsageFlags::COLOR_ATTACHMENT),
                "swapchain does not support rendering"
            );
            let formats = gpu
                .surface_api
                .get_physical_device_surface_formats(gpu.physical, gpu.surface)?;
            let format = formats
                .iter()
                .find(|f| f.format == vk::Format::B8G8R8A8_UNORM)
                .or_else(|| formats.first())
                .context("no surface format")?;
            let extent = if caps.current_extent.width != u32::MAX {
                caps.current_extent
            } else {
                vk::Extent2D {
                    width: width.clamp(caps.min_image_extent.width, caps.max_image_extent.width),
                    height: height
                        .clamp(caps.min_image_extent.height, caps.max_image_extent.height),
                }
            };
            let modes = gpu
                .surface_api
                .get_physical_device_surface_present_modes(gpu.physical, gpu.surface)?;
            let mode = if modes.contains(&vk::PresentModeKHR::MAILBOX) {
                vk::PresentModeKHR::MAILBOX
            } else {
                vk::PresentModeKHR::FIFO
            };
            let count = (caps.min_image_count + 1).min(if caps.max_image_count == 0 {
                u32::MAX
            } else {
                caps.max_image_count
            });
            let alpha = [
                vk::CompositeAlphaFlagsKHR::OPAQUE,
                vk::CompositeAlphaFlagsKHR::PRE_MULTIPLIED,
                vk::CompositeAlphaFlagsKHR::POST_MULTIPLIED,
                vk::CompositeAlphaFlagsKHR::INHERIT,
            ]
            .into_iter()
            .find(|a| caps.supported_composite_alpha.contains(*a))
            .context("no composite alpha mode")?;
            let api = ash::khr::swapchain::Device::new(&gpu.instance, &gpu.device);
            let handle = api.create_swapchain(
                &vk::SwapchainCreateInfoKHR::default()
                    .surface(gpu.surface)
                    .min_image_count(count)
                    .image_format(format.format)
                    .image_color_space(format.color_space)
                    .image_extent(extent)
                    .image_array_layers(1)
                    .image_usage(vk::ImageUsageFlags::COLOR_ATTACHMENT)
                    .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
                    .pre_transform(caps.current_transform)
                    .composite_alpha(alpha)
                    .present_mode(mode)
                    .clipped(true),
                None,
            )?;
            let mut chain = Swapchain {
                gpu: gpu.clone(),
                api,
                handle,
                extent,
                views: Vec::new(),
                framebuffers: Vec::new(),
                render_pass: vk::RenderPass::null(),
                pipeline: vk::Pipeline::null(),
                finished: Vec::new(),
            };
            let attachments = [vk::AttachmentDescription::default()
                .format(format.format)
                .samples(vk::SampleCountFlags::TYPE_1)
                .load_op(vk::AttachmentLoadOp::CLEAR)
                .store_op(vk::AttachmentStoreOp::STORE)
                .initial_layout(vk::ImageLayout::UNDEFINED)
                .final_layout(vk::ImageLayout::PRESENT_SRC_KHR)];
            let colors = [vk::AttachmentReference {
                attachment: 0,
                layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            }];
            let subpasses = [vk::SubpassDescription::default()
                .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
                .color_attachments(&colors)];
            let dependencies = [vk::SubpassDependency::default()
                .src_subpass(vk::SUBPASS_EXTERNAL)
                .dst_subpass(0)
                .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
                .dst_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
                .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)];
            chain.render_pass = gpu.device.create_render_pass(
                &vk::RenderPassCreateInfo::default()
                    .attachments(&attachments)
                    .subpasses(&subpasses)
                    .dependencies(&dependencies),
                None,
            )?;
            chain.pipeline = self.pipeline(chain.render_pass)?;
            for image in chain.api.get_swapchain_images(chain.handle)? {
                let view = gpu.device.create_image_view(
                    &vk::ImageViewCreateInfo::default()
                        .image(image)
                        .view_type(vk::ImageViewType::TYPE_2D)
                        .format(format.format)
                        .subresource_range(range()),
                    None,
                )?;
                chain.views.push(view);
                let views = [view];
                chain.framebuffers.push(
                    gpu.device.create_framebuffer(
                        &vk::FramebufferCreateInfo::default()
                            .render_pass(chain.render_pass)
                            .attachments(&views)
                            .width(extent.width)
                            .height(extent.height)
                            .layers(1),
                        None,
                    )?,
                );
                chain.finished.push(
                    gpu.device
                        .create_semaphore(&vk::SemaphoreCreateInfo::default(), None)?,
                );
            }
            self.swapchain = Some(chain);
            self.dirty = false;
        }
        Ok(())
    }

    fn pipeline(&self, pass: vk::RenderPass) -> Result<vk::Pipeline> {
        // SAFETY: SPIR-V is compiled from the adjacent shaders, both modules outlive creation.
        unsafe {
            let device = &self.gpu.device;
            let vert = ash::util::read_spv(&mut Cursor::new(include_bytes!("quad.vert.spv")))?;
            let frag = ash::util::read_spv(&mut Cursor::new(include_bytes!("quad.frag.spv")))?;
            let vertex = device
                .create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&vert), None)?;
            let fragment = match device
                .create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&frag), None)
            {
                Ok(module) => module,
                Err(error) => {
                    device.destroy_shader_module(vertex, None);
                    return Err(error.into());
                }
            };
            let stages = [
                vk::PipelineShaderStageCreateInfo::default()
                    .stage(vk::ShaderStageFlags::VERTEX)
                    .module(vertex)
                    .name(c"main"),
                vk::PipelineShaderStageCreateInfo::default()
                    .stage(vk::ShaderStageFlags::FRAGMENT)
                    .module(fragment)
                    .name(c"main"),
            ];
            let vertex_input = vk::PipelineVertexInputStateCreateInfo::default();
            let assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
                .topology(vk::PrimitiveTopology::TRIANGLE_LIST);
            let viewport = vk::PipelineViewportStateCreateInfo::default()
                .viewport_count(1)
                .scissor_count(1);
            let raster = vk::PipelineRasterizationStateCreateInfo::default()
                .polygon_mode(vk::PolygonMode::FILL)
                .line_width(1.)
                .cull_mode(vk::CullModeFlags::NONE);
            let multisample = vk::PipelineMultisampleStateCreateInfo::default()
                .rasterization_samples(vk::SampleCountFlags::TYPE_1);
            let attachments = [vk::PipelineColorBlendAttachmentState::default()
                .blend_enable(true)
                .src_color_blend_factor(vk::BlendFactor::ONE)
                .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
                .color_blend_op(vk::BlendOp::ADD)
                .src_alpha_blend_factor(vk::BlendFactor::ONE)
                .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
                .alpha_blend_op(vk::BlendOp::ADD)
                .color_write_mask(vk::ColorComponentFlags::RGBA)];
            let blend = vk::PipelineColorBlendStateCreateInfo::default().attachments(&attachments);
            let states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
            let dynamic = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&states);
            let info = vk::GraphicsPipelineCreateInfo::default()
                .stages(&stages)
                .vertex_input_state(&vertex_input)
                .input_assembly_state(&assembly)
                .viewport_state(&viewport)
                .rasterization_state(&raster)
                .multisample_state(&multisample)
                .color_blend_state(&blend)
                .dynamic_state(&dynamic)
                .layout(self.pipeline_layout)
                .render_pass(pass);
            let result = device.create_graphics_pipelines(vk::PipelineCache::null(), &[info], None);
            device.destroy_shader_module(vertex, None);
            device.destroy_shader_module(fragment, None);
            match result {
                Ok(pipelines) => Ok(pipelines[0]),
                Err((pipelines, error)) => {
                    for pipeline in pipelines {
                        device.destroy_pipeline(pipeline, None);
                    }
                    Err(error.into())
                }
            }
        }
    }

    fn begin(&self) -> Result<()> {
        // SAFETY: the previous submit was waited before its caller returned.
        unsafe {
            self.gpu
                .device
                .reset_command_pool(self.pool, vk::CommandPoolResetFlags::empty())?;
            self.gpu
                .device
                .reset_descriptor_pool(self.descriptors, vk::DescriptorPoolResetFlags::empty())?;
            self.gpu.device.begin_command_buffer(
                self.command,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;
        }
        Ok(())
    }

    fn upload(
        &self,
        image: &mut Image,
        pixels: &[u8],
        stride: u32,
        damage: &[Rect],
    ) -> Result<Option<Buffer>> {
        let (bytes, copies) = pack_damage(pixels, stride, damage);
        if bytes.is_empty() {
            return Ok(None);
        }
        let staging = Buffer::new(self.gpu.clone(), &bytes)?;
        image.barrier(
            self.command,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            false,
            false,
        );
        // SAFETY: packed regions are within the image and staging allocation.
        unsafe {
            self.gpu.device.cmd_copy_buffer_to_image(
                self.command,
                staging.buffer,
                image.image,
                image.layout,
                &copies,
            );
        }
        image.barrier(
            self.command,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            false,
            false,
        );
        Ok(Some(staging))
    }

    fn draw(&self, image: &Image, region: (f64, f64, f64, f64)) -> Result<()> {
        let (x, y, width, height) = region;
        if width <= 0. || height <= 0. {
            return Ok(());
        }
        // SAFETY: descriptor set lives until the submit finishes; image and sampler outlive it.
        unsafe {
            let layouts = [self.set_layout];
            let set = self.gpu.device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(self.descriptors)
                    .set_layouts(&layouts),
            )?[0];
            let images = [vk::DescriptorImageInfo::default()
                .image_view(image.view)
                .image_layout(image.layout)
                .sampler(self.sampler)];
            self.gpu.device.update_descriptor_sets(
                &[vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(0)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .image_info(&images)],
                &[],
            );
            self.gpu.device.cmd_bind_descriptor_sets(
                self.command,
                vk::PipelineBindPoint::GRAPHICS,
                self.pipeline_layout,
                0,
                &[set],
                &[],
            );
            self.gpu.device.cmd_set_viewport(
                self.command,
                0,
                &[vk::Viewport {
                    x: x as f32,
                    y: y as f32,
                    width: width as f32,
                    height: height as f32,
                    min_depth: 0.,
                    max_depth: 1.,
                }],
            );
            self.gpu.device.cmd_draw(self.command, 3, 1, 0, 0);
        }
        Ok(())
    }

    fn submit(&self, acquired: bool) -> Result<()> {
        // SAFETY: the command is complete, and fence reuse follows a successful wait.
        unsafe {
            self.gpu.device.end_command_buffer(self.command)?;
            self.gpu.device.reset_fences(&[self.fence])?;
            let commands = [self.command];
            let waits = [self.acquired];
            let stages = [vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT];
            let mut info = vk::SubmitInfo::default().command_buffers(&commands);
            if acquired {
                info = info.wait_semaphores(&waits).wait_dst_stage_mask(&stages);
            }
            self.gpu
                .device
                .queue_submit(self.gpu.queue, &[info], self.fence)?;
            self.gpu
                .device
                .wait_for_fences(&[self.fence], true, u64::MAX)?;
        }
        Ok(())
    }

    /// A frame may be released immediately on return, including after import failure.
    /// Import failures are returned to the shell, which requests the CPU path.
    pub fn present(
        &mut self,
        window: &Window,
        surface: Option<&Surface>,
        frame: Option<&Frame>,
        cursor: &crate::Cursor,
    ) -> Result<()> {
        let size = window.inner_size();
        let drawable = size.width != 0 && size.height != 0 && !self.occluded;
        if drawable && (self.dirty || self.swapchain.is_none()) {
            self.recreate(size.width, size.height)?;
        }
        if let (Some(surface), Some(_)) = (surface, frame)
            && self.surface != Some(surface.id)
        {
            self.imports.clear();
            self.repaint = None;
            self.surface = Some(surface.id);
        }
        let mut imported = None;
        let mut repaint = self.repaint.take();
        let mut staging = Vec::new();
        self.begin()?;
        if let (Some(surface), Some(frame)) = (surface, frame) {
            if repaint
                .as_ref()
                .is_none_or(|i| i.width != surface.width || i.height != surface.height)
            {
                repaint = Some(Image::new(
                    self.gpu.clone(),
                    surface.width,
                    surface.height,
                    format(surface.fourcc)?,
                    None,
                    true,
                )?);
            }
            if surface.kind == SurfaceKind::Dmabuf {
                let key = (surface.id, frame.buffer);
                let mut image = match self.imports.remove(&key) {
                    Some(image) => image,
                    None => Image::new(
                        self.gpu.clone(),
                        surface.width,
                        surface.height,
                        format(surface.fourcc)?,
                        Some((surface, frame.buffer)),
                        true,
                    )?,
                };
                image.barrier(
                    self.command,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    true,
                    false,
                );
                let cache = repaint.as_mut().unwrap();
                cache.barrier(
                    self.command,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    false,
                    false,
                );
                // Keep an owned repaint image. No released guest allocation is sampled on expose.
                // SAFETY: matching formats/extents and transfer layouts, ownership acquired above.
                unsafe {
                    self.gpu.device.cmd_copy_image(
                        self.command,
                        image.image,
                        image.layout,
                        cache.image,
                        cache.layout,
                        &[vk::ImageCopy::default()
                            .src_subresource(layers())
                            .dst_subresource(layers())
                            .extent(vk::Extent3D {
                                width: surface.width,
                                height: surface.height,
                                depth: 1,
                            })],
                    );
                }
                cache.barrier(
                    self.command,
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    false,
                    false,
                );
                image.barrier(
                    self.command,
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    false,
                    false,
                );
                imported = Some((key, image));
            } else if let Some(buffer) = self.upload(
                repaint.as_mut().unwrap(),
                &surface.pixels,
                surface.stride,
                &frame.damage,
            )? {
                staging.push(buffer);
            }
        }
        let mut cursor_image = self.cursor.take();
        if self.cursor_generation != cursor.generation {
            cursor_image = None;
            if cursor.width > 0 && cursor.height > 0 {
                let mut image = Image::new(
                    self.gpu.clone(),
                    cursor.width,
                    cursor.height,
                    vk::Format::B8G8R8A8_UNORM,
                    None,
                    false,
                )?;
                if let Some(buffer) = self.upload(
                    &mut image,
                    &cursor.argb,
                    cursor.width * 4,
                    &[Rect {
                        x: 0,
                        y: 0,
                        width: cursor.width,
                        height: cursor.height,
                    }],
                )? {
                    staging.push(buffer);
                }
                cursor_image = Some(image);
            }
            self.cursor_generation = cursor.generation;
        }
        if !drawable {
            // Keep the repaint cache current while hidden without blocking on WSI.
            if let Some((_, image)) = &mut imported {
                image.barrier(self.command, vk::ImageLayout::GENERAL, false, true);
            }
            self.submit(false)?;
            if let Some((key, image)) = imported {
                self.imports.insert(key, image);
            }
            self.repaint = repaint;
            self.cursor = cursor_image;
            return Ok(());
        }
        let chain = self.swapchain.as_ref().unwrap();
        // SAFETY: acquire semaphore is consumed by the following submit before reuse.
        let acquired = unsafe {
            chain
                .api
                .acquire_next_image(chain.handle, 50_000_000, self.acquired, vk::Fence::null())
        };
        let (index, suboptimal) = match acquired {
            Ok(value) => value,
            Err(
                error @ (vk::Result::ERROR_OUT_OF_DATE_KHR
                | vk::Result::TIMEOUT
                | vk::Result::NOT_READY),
            ) => {
                // Still complete uploads and foreign ownership transfers before releasing a frame.
                if let Some((_, image)) = &mut imported {
                    image.barrier(self.command, vk::ImageLayout::GENERAL, false, true);
                }
                self.submit(false)?;
                if let Some((key, image)) = imported {
                    self.imports.insert(key, image);
                }
                self.repaint = repaint;
                self.cursor = cursor_image;
                self.dirty |= error == vk::Result::ERROR_OUT_OF_DATE_KHR;
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        };
        let clear = [vk::ClearValue {
            color: vk::ClearColorValue {
                float32: [0., 0., 0., 1.],
            },
        }];
        // SAFETY: framebuffer matches render pass, acquired index is in range.
        unsafe {
            self.gpu.device.cmd_begin_render_pass(
                self.command,
                &vk::RenderPassBeginInfo::default()
                    .render_pass(chain.render_pass)
                    .framebuffer(chain.framebuffers[index as usize])
                    .render_area(vk::Rect2D {
                        offset: vk::Offset2D::default(),
                        extent: chain.extent,
                    })
                    .clear_values(&clear),
                vk::SubpassContents::INLINE,
            );
            self.gpu.device.cmd_bind_pipeline(
                self.command,
                vk::PipelineBindPoint::GRAPHICS,
                chain.pipeline,
            );
            self.gpu.device.cmd_set_scissor(
                self.command,
                0,
                &[vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: chain.extent,
                }],
            );
        }
        if let Some(image) = imported
            .as_ref()
            .map(|(_, image)| image)
            .or(repaint.as_ref())
        {
            let region = viewport(
                (chain.extent.width, chain.extent.height),
                (image.width, image.height),
            );
            self.draw(image, region)?;
            if cursor.visible
                && let Some(cursor_image) = &cursor_image
            {
                let scale = region.2 / image.width as f64;
                self.draw(
                    cursor_image,
                    (
                        region.0 + (cursor.x as f64 - cursor.hot_x as f64) * scale,
                        region.1 + (cursor.y as f64 - cursor.hot_y as f64) * scale,
                        cursor.width as f64 * scale,
                        cursor.height as f64 * scale,
                    ),
                )?;
            }
        }
        // SAFETY: an active render pass exists on this recording command.
        unsafe {
            self.gpu.device.cmd_end_render_pass(self.command);
        }
        if let Some((_, image)) = &mut imported {
            image.barrier(self.command, vk::ImageLayout::GENERAL, false, true);
        }
        // Per-swapchain-image signal semaphore is reused only after reacquiring that image.
        // SAFETY: command dependencies are complete, and all referenced resources outlive the wait.
        unsafe {
            let device = &self.gpu.device;
            device.end_command_buffer(self.command)?;
            device.reset_fences(&[self.fence])?;
            let waits = [self.acquired];
            let stages = [vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT];
            let commands = [self.command];
            let signals = [chain.finished[index as usize]];
            device.queue_submit(
                self.gpu.queue,
                &[vk::SubmitInfo::default()
                    .wait_semaphores(&waits)
                    .wait_dst_stage_mask(&stages)
                    .command_buffers(&commands)
                    .signal_semaphores(&signals)],
                self.fence,
            )?;
            let chains = [chain.handle];
            let indices = [index];
            let result = chain.api.queue_present(
                self.gpu.queue,
                &vk::PresentInfoKHR::default()
                    .wait_semaphores(&signals)
                    .swapchains(&chains)
                    .image_indices(&indices),
            );
            // Release tokens are sent only after this wait, never merely after queue_submit.
            device.wait_for_fences(&[self.fence], true, u64::MAX)?;
            match result {
                Ok(changed) => self.dirty |= changed || suboptimal,
                Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => self.dirty = true,
                Err(error) => return Err(error.into()),
            }
        }
        if let Some((key, image)) = imported {
            self.imports.insert(key, image);
        }
        self.repaint = repaint;
        self.cursor = cursor_image;
        Ok(())
    }
}

fn format(fourcc: u32) -> Result<vk::Format> {
    match fourcc {
        x if x == u32::from_le_bytes(*b"XR24") || x == u32::from_le_bytes(*b"AR24") => {
            Ok(vk::Format::B8G8R8A8_UNORM)
        }
        x if x == u32::from_le_bytes(*b"XB24") || x == u32::from_le_bytes(*b"AB24") => {
            Ok(vk::Format::R8G8B8A8_UNORM)
        }
        _ => anyhow::bail!("unsupported DRM format {fourcc:#x}"),
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        // SAFETY: wait includes presentation, then destroy children before their layouts/pools.
        unsafe {
            let _ = self.gpu.device.device_wait_idle();
            self.swapchain.take();
            self.imports.clear();
            self.repaint.take();
            self.cursor.take();
            let device = &self.gpu.device;
            device.destroy_sampler(self.sampler, None);
            device.destroy_pipeline_layout(self.pipeline_layout, None);
            device.destroy_descriptor_pool(self.descriptors, None);
            device.destroy_descriptor_set_layout(self.set_layout, None);
            device.destroy_semaphore(self.acquired, None);
            device.destroy_fence(self.fence, None);
            device.destroy_command_pool(self.pool, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::FromRawFd;

    #[link(name = "gbm")]
    unsafe extern "C" {
        fn gbm_create_device(fd: i32) -> *mut std::ffi::c_void;
        fn gbm_device_destroy(device: *mut std::ffi::c_void);
        fn gbm_bo_create_with_modifiers2(
            device: *mut std::ffi::c_void,
            width: u32,
            height: u32,
            format: u32,
            modifiers: *const u64,
            count: u32,
            flags: u32,
        ) -> *mut std::ffi::c_void;
        fn gbm_bo_destroy(bo: *mut std::ffi::c_void);
        fn gbm_bo_get_fd(bo: *mut std::ffi::c_void) -> i32;
        fn gbm_bo_get_stride(bo: *mut std::ffi::c_void) -> u32;
        fn gbm_bo_get_modifier(bo: *mut std::ffi::c_void) -> u64;
    }

    struct Gbm {
        device: *mut std::ffi::c_void,
        bo: *mut std::ffi::c_void,
    }
    impl Drop for Gbm {
        fn drop(&mut self) {
            // SAFETY: the BO is owned here and destroyed before its GBM device.
            unsafe {
                if !self.bo.is_null() {
                    gbm_bo_destroy(self.bo);
                }
                if !self.device.is_null() {
                    gbm_device_destroy(self.device);
                }
            }
        }
    }

    #[test]
    fn gpu_dmabuf_modifier_import_is_cached_and_reacquired() {
        let Some(node) = std::env::var_os("VMLAB_VIEWER_DRM_NODE") else {
            eprintln!(
                "SKIP dma-buf hardware test: set VMLAB_VIEWER_DRM_NODE to a render node (select its Vulkan ICD with VK_DRIVER_FILES)"
            );
            return;
        };
        let node = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(node)
            .unwrap();
        let gpu = Gpu::new(None, true).unwrap();
        assert!(gpu.dmabuf, "selected ICD lacks dma-buf extensions");
        let renderer = Renderer::with_gpu(gpu.clone()).unwrap();
        // SAFETY: only render-node buffer allocation is used. No DRM master,
        // modesetting, driver binding, or host display operation is performed.
        let gbm = unsafe {
            let mut gbm = Gbm {
                device: gbm_create_device(node.as_raw_fd()),
                bo: std::ptr::null_mut(),
            };
            assert!(!gbm.device.is_null());
            let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
            let mut properties = vk::FormatProperties2::default().push_next(&mut list);
            gpu.instance.get_physical_device_format_properties2(
                gpu.physical,
                vk::Format::B8G8R8A8_UNORM,
                &mut properties,
            );
            let mut modifiers = vec![
                vk::DrmFormatModifierPropertiesEXT::default();
                list.drm_format_modifier_count as usize
            ];
            list = vk::DrmFormatModifierPropertiesListEXT::default()
                .drm_format_modifier_properties(&mut modifiers);
            properties = vk::FormatProperties2::default().push_next(&mut list);
            gpu.instance.get_physical_device_format_properties2(
                gpu.physical,
                vk::Format::B8G8R8A8_UNORM,
                &mut properties,
            );
            for modifier in modifiers
                .iter()
                .filter(|m| m.drm_format_modifier_plane_count == 1)
            {
                gbm.bo = gbm_bo_create_with_modifiers2(
                    gbm.device,
                    64,
                    32,
                    u32::from_le_bytes(*b"XR24"),
                    &modifier.drm_format_modifier,
                    1,
                    4,
                );
                if !gbm.bo.is_null() {
                    break;
                }
            }
            assert!(
                !gbm.bo.is_null(),
                "GBM and Vulkan have no common one-plane modifier"
            );
            gbm
        };
        // SAFETY: these queries borrow the live BO. get_fd returns a new owned fd.
        let (fd, stride, modifier) = unsafe {
            (
                gbm_bo_get_fd(gbm.bo),
                gbm_bo_get_stride(gbm.bo),
                gbm_bo_get_modifier(gbm.bo),
            )
        };
        assert!(fd >= 0);
        // SAFETY: gbm_bo_get_fd returned this owned descriptor exactly once.
        let file = unsafe { std::fs::File::from_raw_fd(fd) };
        let surface = Surface {
            id: 1,
            width: 64,
            height: 32,
            fourcc: u32::from_le_bytes(*b"XR24"),
            stride,
            modifier,
            kind: SurfaceKind::Dmabuf,
            buffers: vec![file],
            pixels: Vec::new(),
            initialized: false,
        };
        eprintln!("GBM test allocation: modifier={modifier:#x}, stride={stride}");
        let mut image = Image::new(
            gpu.clone(),
            64,
            32,
            vk::Format::B8G8R8A8_UNORM,
            Some((&surface, 0)),
            true,
        )
        .unwrap();
        let imported = image.image;
        for value in [23, 97, 111] {
            renderer.begin().unwrap();
            image.barrier(renderer.command, vk::ImageLayout::GENERAL, true, false);
            let bytes = vec![value; 64 * 32 * 4];
            let staging = renderer
                .upload(
                    &mut image,
                    &bytes,
                    256,
                    &[Rect {
                        x: 0,
                        y: 0,
                        width: 64,
                        height: 32,
                    }],
                )
                .unwrap();
            image.barrier(
                renderer.command,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                false,
                false,
            );
            let readback = Buffer::new(gpu.clone(), &vec![0; 64 * 32 * 4]).unwrap();
            // SAFETY: the command is recording and the whole image fits the readback buffer.
            unsafe {
                gpu.device.cmd_copy_image_to_buffer(
                    renderer.command,
                    image.image,
                    image.layout,
                    readback.buffer,
                    &[vk::BufferImageCopy::default()
                        .image_subresource(layers())
                        .image_extent(vk::Extent3D {
                            width: 64,
                            height: 32,
                            depth: 1,
                        })],
                );
            }
            image.barrier(renderer.command, vk::ImageLayout::GENERAL, false, true);
            renderer.submit(false).unwrap();
            drop(staging);
            assert_eq!(readback.read().unwrap(), bytes);
            assert_eq!(image.image, imported);
        }
        eprintln!(
            "dma-buf import: modifier={modifier:#x}, stride={stride}; same VkImage reused across 3 release/reacquire cycles"
        );
    }

    #[test]
    fn gpu_damage_upload_preserves_undamaged_pixels() {
        let gpu = match Gpu::new(None, false) {
            Ok(gpu) => gpu,
            Err(error) => {
                eprintln!("SKIP Vulkan upload: {error:#}");
                return;
            }
        };
        let renderer = Renderer::with_gpu(gpu.clone()).unwrap();
        let mut image =
            Image::new(gpu.clone(), 4, 4, vk::Format::B8G8R8A8_UNORM, None, true).unwrap();
        renderer.begin().unwrap();
        let staging = renderer
            .upload(
                &mut image,
                &[7; 64],
                16,
                &[Rect {
                    x: 0,
                    y: 0,
                    width: 4,
                    height: 4,
                }],
            )
            .unwrap();
        renderer.submit(false).unwrap();
        drop(staging);
        renderer.begin().unwrap();
        let staging = renderer
            .upload(
                &mut image,
                &[99; 64],
                16,
                &[Rect {
                    x: 1,
                    y: 2,
                    width: 2,
                    height: 1,
                }],
            )
            .unwrap();
        image.barrier(
            renderer.command,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            false,
            false,
        );
        let readback = Buffer::new(gpu.clone(), &[0; 64]).unwrap();
        // SAFETY: all regions are in range, command is recording, wait precedes CPU read.
        unsafe {
            gpu.device.cmd_copy_image_to_buffer(
                renderer.command,
                image.image,
                image.layout,
                readback.buffer,
                &[vk::BufferImageCopy::default()
                    .image_subresource(layers())
                    .image_extent(vk::Extent3D {
                        width: 4,
                        height: 4,
                        depth: 1,
                    })],
            );
        }
        renderer.submit(false).unwrap();
        drop(staging);
        let mut expected = vec![7; 64];
        expected[36..44].fill(99);
        assert_eq!(readback.read().unwrap(), expected);
    }
}

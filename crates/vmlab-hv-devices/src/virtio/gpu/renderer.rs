// Copyright 2018 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE.crosvm file.
//
// Adapted from crosvm devices/src/virtio/gpu/{mod.rs,virtio_gpu.rs},
// revision ad804a6ecf1d3c4337f18b56092fc54d8ca574fa.

use super::*;
use rutabaga_gfx::*;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

pub const APERTURE_SIZE: u64 = 1 << 32;

/// The VMM keeps each host mapping registered until `unmap` returns.
/// Addresses and sizes are page aligned and relative to the shared-memory BAR.
pub trait BlobMapper: Send + Sync {
    fn set_base(&self, base: Option<u64>) -> Result<(), DeviceError>;
    fn map(&self, id: u32, offset: u64, host: u64, size: u64) -> Result<(), DeviceError>;
    fn unmap(&self, id: u32) -> Result<(), DeviceError>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GpuMode {
    Virgl,
    Venus,
}

type Task = Box<dyn FnOnce(&mut Backend) + Send>;
type Ring = (u32, u8);

#[derive(Clone)]
pub(super) struct Renderer {
    worker: Arc<Worker>,
    pub capsets: u32,
    completed: Arc<Mutex<BTreeMap<Ring, u64>>>,
    mapper: Arc<dyn BlobMapper>,
}

struct Worker {
    sender: Option<mpsc::Sender<Task>>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

struct HostResource {
    width: u32,
    height: u32,
    format: u32,
    size: u64,
    mapped: bool,
    // Renderer iovecs borrow this allocation until detach or unref.
    memory: Option<GuestMemory>,
    scanout_info: Option<Resource3DInfo>,
}

struct Backend {
    gpu: Rutabaga,
    resources: BTreeMap<u32, HostResource>,
    contexts: BTreeSet<u32>,
    mapper: Arc<dyn BlobMapper>,
    scanout: Option<(u32, Rect)>,
    mode: GpuMode,
    scanout_copy: Option<super::scanout::Scanout>,
    node: PathBuf,
}

impl Drop for Backend {
    fn drop(&mut self) {
        for (&id, resource) in &self.resources {
            if resource.mapped {
                // A failed unregister must not leave KVM referencing freed host pages.
                if let Err(error) = self.mapper.unmap(id) {
                    eprintln!("GPU mapping teardown: {error}");
                    std::process::abort();
                }
                let _ = self.gpu.unmap(id);
            }
            let _ = self.gpu.unref_resource(id);
        }
        self.scanout_copy.take();
        self.gpu.force_ctx_0();
    }
}

pub(super) fn ring(flags: u32, ctx: u32, index: u8) -> Ring {
    if flags & 2 != 0 { (ctx, index) } else { (0, 0) }
}

impl Renderer {
    pub fn new(
        mode: GpuMode,
        node: PathBuf,
        mapper: Arc<dyn BlobMapper>,
    ) -> Result<Self, DeviceError> {
        // Validate explicitly: virglrenderer otherwise falls back to another GPU.
        let node = std::fs::canonicalize(node)?;
        if !node.starts_with("/dev/dri")
            || !node
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("renderD"))
        {
            return Err(DeviceError::Other(
                "render_node must name a DRM render node".into(),
            ));
        }
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&node)?;
        let (sender, receiver) = mpsc::channel::<Task>();
        let (ready, initialized) = mpsc::sync_channel(1);
        let completed = Arc::new(Mutex::new(BTreeMap::<Ring, u64>::new()));
        let done = completed.clone();
        let worker_mapper = mapper.clone();
        let join = std::thread::Builder::new()
            .name("virtio-gpu-renderer".into())
            .spawn(move || {
                let mask = match mode {
                    GpuMode::Virgl => (1 << RUTABAGA_CAPSET_VIRGL) | (1 << RUTABAGA_CAPSET_VIRGL2),
                    GpuMode::Venus => {
                        (1 << RUTABAGA_CAPSET_VIRGL)
                            | (1 << RUTABAGA_CAPSET_VIRGL2)
                            | (1 << RUTABAGA_CAPSET_VENUS)
                    }
                };
                let handler = RutabagaFenceHandler::new(move |f: RutabagaFence| {
                    let mut completed = done.lock();
                    let value = completed
                        .entry(ring(f.flags, f.ctx_id, f.ring_idx))
                        .or_default();
                    *value = (*value).max(f.fence_id);
                });
                let gpu = RutabagaBuilder::new(mask, handler)
                    .set_default_component(RutabagaComponentType::VirglRenderer)
                    .set_use_egl(true)
                    .set_use_surfaceless(false)
                    .set_use_vulkan(mode == GpuMode::Venus)
                    .set_rutabaga_paths(Some(vec![RutabagaPath {
                        path: node.clone(),
                        path_type: RUTABAGA_PATH_TYPE_GPU,
                    }]))
                    .build();
                let gpu = match gpu {
                    Ok(gpu) => gpu,
                    Err(error) => {
                        let _ = ready.send(Err(error.to_string()));
                        return;
                    }
                };
                if gpu.get_num_capsets() == 0 {
                    let _ = ready.send(Err(
                        "virglrenderer failed initialization; refusing rutabaga's 2D fallback"
                            .into(),
                    ));
                    return;
                }
                let _ = ready.send(Ok(gpu.get_num_capsets()));
                let mut backend = Backend {
                    gpu,
                    resources: BTreeMap::new(),
                    contexts: BTreeSet::new(),
                    mapper: worker_mapper,
                    scanout: None,
                    mode,
                    scanout_copy: None,
                    node,
                };
                loop {
                    match receiver.recv_timeout(Duration::from_millis(2)) {
                        Ok(task) => {
                            backend.gpu.force_ctx_0();
                            task(&mut backend);
                            backend.gpu.force_ctx_0();
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                    backend.gpu.event_poll();
                }
            })?;
        let capsets = initialized
            .recv()
            .map_err(|e| DeviceError::Other(e.to_string()))?
            .map_err(|e| DeviceError::Other(format!("initializing virtio-gpu renderer: {e}")))?;
        Ok(Self {
            worker: Arc::new(Worker {
                sender: Some(sender),
                join: Some(join),
            }),
            capsets,
            completed,
            mapper,
        })
    }

    pub fn set_base(&self, base: Option<u64>) -> Result<(), DeviceError> {
        self.mapper.set_base(base)
    }

    fn call<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Backend) -> Result<T, u32> + Send + 'static,
    ) -> Result<T, u32> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.worker
            .sender
            .as_ref()
            .unwrap()
            .send(Box::new(move |backend| {
                let _ = tx.send(f(backend));
            }))
            .map_err(|_| 0x1200u32)?;
        rx.recv().map_err(|_| 0x1200u32)?
    }

    pub fn command(&self, mem: &GuestMemory, request: &[u8]) -> Result<(u32, Vec<u8>), u32> {
        let mem = mem.clone();
        let request = request.to_vec();
        self.call(move |b| b.command(&mem, &request))
    }

    pub fn fence(&self, flags: u32, id: u64, ctx: u32, index: u8) -> Result<(), u32> {
        self.call(move |b| {
            host(b.gpu.create_fence(RutabagaFence {
                flags,
                fence_id: id,
                ctx_id: ctx,
                ring_idx: index,
            }))
        })
    }

    pub fn complete(&self, flags: u32, id: u64, ctx: u32, index: u8) -> bool {
        self.completed
            .lock()
            .get(&ring(flags, ctx, index))
            .is_some_and(|done| *done >= id)
    }

    pub fn has_contexts(&self) -> bool {
        self.call(|b| Ok(!b.contexts.is_empty())).unwrap_or(true)
    }

    pub fn frame(&self) -> Result<Option<Frame>, u32> {
        self.dma_frame().map(|frame| {
            frame.map(|frame| Frame {
                mode: Mode {
                    width: frame.width,
                    height: frame.height,
                    stride: frame.stride,
                    format: PixelFormat::Xrgb8888,
                },
                pixels: frame.pixels.as_ref().clone(),
                dirty: true,
            })
        })
    }

    pub fn mode(&self) -> Option<Mode> {
        self.call(|b| {
            Ok(b.scanout.map(|(_, rect)| Mode {
                width: rect.w,
                height: rect.h,
                stride: rect.w * 4,
                format: PixelFormat::Xrgb8888,
            }))
        })
        .ok()
        .flatten()
    }

    pub fn cursor_resource(&self, id: u32) -> Result<Resource, u32> {
        self.call(move |b| {
            let resource = b.resources.get(&id).ok_or(RESOURCE)?;
            if resource.width != 64 || resource.height != 64 || !valid_format(resource.format) {
                return Err(INVALID);
            }
            let format = resource.format;
            let mut pixels = vec![0; 64 * 64 * 4];
            host(b.gpu.transfer_read(
                0,
                id,
                Transfer3D::new_2d(0, 0, 64, 64, 0),
                Some(std::io::IoSliceMut::new(&mut pixels)),
            ))?;
            Ok(Resource {
                width: 64,
                height: 64,
                format,
                pixels,
                backing: vec![],
            })
        })
    }

    pub fn dma_frame(&self) -> Result<Option<super::DmaFrame>, u32> {
        self.call(|b| {
            let Some((id, rect)) = b.scanout else {
                return Ok(None);
            };
            let resource = b.resources.get(&id).ok_or(RESOURCE)?;
            let mut info = host(b.gpu.resource3d_info(id))?;
            let (width, height) = if let Some(scanout) = resource.scanout_info {
                info.drm_fourcc = scanout.drm_fourcc;
                info.strides[0] = scanout.strides[0];
                info.offsets[0] = scanout.offsets[0];
                (scanout.width, scanout.height)
            } else {
                (resource.width, resource.height)
            };
            let handle = host(b.gpu.export_blob(id))?;
            let handle = handle.as_mesa_handle().ok_or(INVALID)?;
            if handle.handle_type != RUTABAGA_HANDLE_TYPE_MEM_DMABUF {
                return Err(INVALID);
            }
            use magma_gpu::util::AsRawDescriptor;
            if b.scanout_copy.is_none() {
                b.scanout_copy = Some(super::scanout::Scanout::new(&b.node)?);
            }
            b.scanout_copy
                .as_ref()
                .unwrap()
                .capture(
                    handle.os_handle.as_raw_descriptor(),
                    info,
                    width,
                    height,
                    rect,
                )
                .map(Some)
        })
    }

    pub fn reset(&self) -> Result<(), u32> {
        self.call(|b| {
            let ids: Vec<_> = b.resources.keys().copied().collect();
            for id in ids {
                b.unref(id)?;
            }
            for id in std::mem::take(&mut b.contexts) {
                host(b.gpu.destroy_context(id))?;
            }
            b.scanout = None;
            Ok(())
        })?;
        self.completed.lock().clear();
        Ok(())
    }
}

fn host<T>(result: RutabagaResult<T>) -> Result<T, u32> {
    result.map_err(|error| {
        eprintln!("virtio-gpu: {error}");
        0x1200
    })
}

fn iovecs(mem: &GuestMemory, d: &mut Decoder<'_>, count: usize) -> Result<Vec<RutabagaIovec>, u32> {
    if count > MAX_ENTRIES {
        return Err(INVALID);
    }
    let mut result = Vec::with_capacity(count);
    for _ in 0..count {
        let address = d.u64()?;
        let length = d.u32()? as usize;
        d.u32()?;
        if length == 0 {
            return Err(INVALID);
        }
        // Split at guest-memory region boundaries rather than assuming contiguous host RAM.
        vm_memory::GuestMemory::get_slices(
            mem,
            GuestAddress(address),
            length,
            Permissions::ReadWrite,
        )
        .map_err(|_| INVALID)?
        .try_for_each(|slice| {
            let slice = slice.map_err(|_| INVALID)?;
            result.push(RutabagaIovec {
                base: slice.ptr_guard_mut().as_ptr().cast(),
                len: slice.len(),
            });
            Ok::<_, u32>(())
        })?;
    }
    Ok(result)
}

impl Backend {
    fn unref(&mut self, id: u32) -> Result<(), u32> {
        let r = self.resources.get(&id).ok_or(RESOURCE)?;
        if r.mapped {
            self.mapper.unmap(id).map_err(|_| 0x1200u32)?;
            host(self.gpu.unmap(id))?;
        }
        host(self.gpu.unref_resource(id))?;
        self.resources.remove(&id);
        if self.scanout.is_some_and(|(scanout, _)| scanout == id) {
            self.scanout = None;
        }
        Ok(())
    }

    fn command(&mut self, mem: &GuestMemory, request: &[u8]) -> Result<(u32, Vec<u8>), u32> {
        let mut d = Decoder::new(request);
        let kind = d.u32()?;
        d.u32()?;
        d.u64()?;
        let ctx = d.u32()?;
        d.u32()?;
        let mut out = Vec::new();
        let response = match kind {
            0x108 => {
                let (id, version, size) = host(self.gpu.get_capset_info(d.u32()?))?;
                d.u32()?;
                for value in [id, version, size, 0] {
                    out.extend_from_slice(&value.to_le_bytes());
                }
                0x1102
            }
            0x109 => {
                let id = d.u32()?;
                let version = d.u32()?;
                out = host(self.gpu.get_capset(id, version))?;
                0x1103
            }
            0x200 => {
                let len = d.u32()? as usize;
                let init = d.u32()?;
                let name = d.take(64)?;
                if ctx == 0 || len > 64 || self.contexts.len() >= MAX_RESOURCES {
                    return Err(INVALID);
                }
                let capset = init & 0xff;
                if !(matches!(
                    (self.mode, capset),
                    (GpuMode::Virgl, 0..=2) | (GpuMode::Venus, 0..=2 | 4)
                )) || init & !0xff != 0
                {
                    return Err(INVALID);
                }
                let name = std::str::from_utf8(&name[..len]).map_err(|_| INVALID)?;
                host(self.gpu.create_context(ctx, init, Some(name)))?;
                self.contexts.insert(ctx);
                NODATA
            }
            0x201 => {
                host(self.gpu.destroy_context(ctx))?;
                self.contexts.remove(&ctx);
                NODATA
            }
            0x202 | 0x203 => {
                let id = d.u32()?;
                d.u32()?;
                if kind == 0x202 {
                    host(self.gpu.context_attach_resource(ctx, id))?;
                } else {
                    host(self.gpu.context_detach_resource(ctx, id))?;
                }
                NODATA
            }
            0x101 | 0x204 => {
                let id = d.u32()?;
                let create = if kind == 0x101 {
                    ResourceCreate3D {
                        target: 2,
                        format: d.u32()?,
                        bind: 2,
                        width: d.u32()?,
                        height: d.u32()?,
                        depth: 1,
                        array_size: 1,
                        last_level: 0,
                        nr_samples: 0,
                        flags: 0,
                    }
                } else {
                    ResourceCreate3D {
                        target: d.u32()?,
                        format: d.u32()?,
                        bind: d.u32()?,
                        width: d.u32()?,
                        height: d.u32()?,
                        depth: d.u32()?,
                        array_size: d.u32()?,
                        last_level: d.u32()?,
                        nr_samples: d.u32()?,
                        flags: d.u32()?,
                    }
                };
                if id == 0 || self.resources.len() >= MAX_RESOURCES {
                    return Err(INVALID);
                }
                host(self.gpu.resource_create_3d(id, create))?;
                self.resources.insert(
                    id,
                    HostResource {
                        width: create.width,
                        height: create.height,
                        format: create.format,
                        size: 0,
                        mapped: false,
                        memory: None,
                        scanout_info: None,
                    },
                );
                NODATA
            }
            0x102 => {
                let id = d.u32()?;
                d.u32()?;
                self.unref(id)?;
                NODATA
            }
            0x106 => {
                let id = d.u32()?;
                let count = d.u32()? as usize;
                let resource = self.resources.get_mut(&id).ok_or(RESOURCE)?;
                if resource.memory.is_some() {
                    return Err(INVALID);
                }
                let vecs = iovecs(mem, &mut d, count)?;
                host(self.gpu.attach_backing(id, vecs))?;
                resource.memory = Some(mem.clone());
                NODATA
            }
            0x107 => {
                let id = d.u32()?;
                d.u32()?;
                host(self.gpu.detach_backing(id))?;
                self.resources.get_mut(&id).ok_or(RESOURCE)?.memory = None;
                NODATA
            }
            0x105 | 0x205 | 0x206 => {
                let x = d.u32()?;
                let y = d.u32()?;
                let (z, w, h, depth) = if kind == 0x105 {
                    (0, d.u32()?, d.u32()?, 1)
                } else {
                    (d.u32()?, d.u32()?, d.u32()?, d.u32()?)
                };
                let offset = d.u64()?;
                let id = d.u32()?;
                let level = d.u32()?;
                let (stride, layer_stride) = if kind == 0x105 {
                    (0, 0)
                } else {
                    (d.u32()?, d.u32()?)
                };
                let transfer = Transfer3D {
                    x,
                    y,
                    z,
                    w,
                    h,
                    d: depth,
                    level,
                    stride,
                    layer_stride,
                    offset,
                };
                if kind == 0x206 {
                    host(self.gpu.transfer_read(ctx, id, transfer, None))?;
                } else {
                    host(self.gpu.transfer_write(ctx, id, transfer, None))?;
                }
                NODATA
            }
            0x207 => {
                let size = d.u32()? as usize;
                d.u32()?;
                if size > 16 * 1024 * 1024 || !size.is_multiple_of(4) {
                    return Err(INVALID);
                }
                let mut command = d.take(size)?.to_vec();
                host(self.gpu.submit_command(ctx, &mut command, &[]))?;
                NODATA
            }
            0x103 => {
                let rect = d.rect()?;
                if d.u32()? != 0 {
                    return Err(SCANOUT);
                }
                let id = d.u32()?;
                if id == 0 {
                    self.scanout = None;
                } else {
                    let r = self.resources.get(&id).ok_or(RESOURCE)?;
                    if !valid_size(rect.w, rect.h)
                        || !rect.fits(r.width, r.height)
                        || !valid_format(r.format)
                    {
                        return Err(INVALID);
                    }
                    self.scanout = Some((id, rect));
                }
                NODATA
            }
            0x104 => {
                d.rect()?;
                let id = d.u32()?;
                d.u32()?;
                self.resources.get(&id).ok_or(RESOURCE)?;
                // Virgl flush is ordered by the following fence. Venus blob flush
                // does not use the optional rutabaga resource_flush operation.
                NODATA
            }
            0x10c => {
                let id = d.u32()?;
                let blob_mem = d.u32()?;
                let blob_flags = d.u32()?;
                let count = d.u32()? as usize;
                let blob_id = d.u64()?;
                let size = d.u64()?;
                if id == 0
                    || size == 0
                    || size > APERTURE_SIZE
                    || self.resources.len() >= MAX_RESOURCES
                {
                    return Err(INVALID);
                }
                let vecs = iovecs(mem, &mut d, count)?;
                host(self.gpu.resource_create_blob(
                    ctx,
                    id,
                    ResourceCreateBlob {
                        blob_mem,
                        blob_flags,
                        blob_id,
                        size,
                    },
                    Some(vecs),
                    None,
                ))?;
                self.resources.insert(
                    id,
                    HostResource {
                        width: 0,
                        height: 0,
                        format: 0,
                        size,
                        mapped: false,
                        memory: Some(mem.clone()),
                        scanout_info: None,
                    },
                );
                NODATA
            }
            0x10d => {
                let rect = d.rect()?;
                if d.u32()? != 0 {
                    return Err(SCANOUT);
                }
                let id = d.u32()?;
                let width = d.u32()?;
                let height = d.u32()?;
                let format = d.u32()?;
                d.u32()?;
                let mut strides = [0; 4];
                let mut offsets = [0; 4];
                for stride in &mut strides {
                    *stride = d.u32()?;
                }
                for offset in &mut offsets {
                    *offset = d.u32()?;
                }
                if id == 0 {
                    self.scanout = None;
                } else {
                    if !valid_size(width, height)
                        || !rect.fits(width, height)
                        || !valid_format(format)
                        || strides[0] < width * 4
                        || strides[1..] != [0; 3]
                        || offsets[1..] != [0; 3]
                    {
                        return Err(INVALID);
                    }
                    let resource = self.resources.get_mut(&id).ok_or(RESOURCE)?;
                    if (offsets[0] as u64)
                        .checked_add(strides[0] as u64 * height as u64)
                        .is_none_or(|end| end > resource.size)
                    {
                        return Err(INVALID);
                    }
                    let drm_fourcc = match format {
                        1 => u32::from_le_bytes(*b"AR24"),
                        2 => u32::from_le_bytes(*b"XR24"),
                        67 => u32::from_le_bytes(*b"AB24"),
                        134 => u32::from_le_bytes(*b"XB24"),
                        _ => return Err(INVALID),
                    };
                    resource.scanout_info = Some(Resource3DInfo {
                        width,
                        height,
                        drm_fourcc,
                        strides,
                        offsets,
                        modifier: 0,
                    });
                    self.scanout = Some((id, rect));
                }
                NODATA
            }
            0x208 => {
                let id = d.u32()?;
                d.u32()?;
                let offset = d.u64()?;
                let resource = self.resources.get_mut(&id).ok_or(RESOURCE)?;
                let size = resource.size;
                if resource.mapped
                    || offset & 4095 != 0
                    || size & 4095 != 0
                    || offset
                        .checked_add(size)
                        .is_none_or(|end| end > APERTURE_SIZE)
                {
                    return Err(INVALID);
                }
                let info = host(self.gpu.map_info(id))?;
                let mapping = host(self.gpu.map(id))?;
                if mapping.size < size || mapping.ptr & 4095 != 0 {
                    let _ = self.gpu.unmap(id);
                    return Err(INVALID);
                }
                if let Err(error) = self.mapper.map(id, offset, mapping.ptr, size) {
                    let _ = self.gpu.unmap(id);
                    eprintln!("GPU blob mapping: {error}");
                    return Err(0x1200);
                }
                resource.mapped = true;
                out.extend_from_slice(&(info & RUTABAGA_MAP_CACHE_MASK).to_le_bytes());
                out.extend_from_slice(&0u32.to_le_bytes());
                0x1106
            }
            0x209 => {
                let id = d.u32()?;
                d.u32()?;
                let resource = self.resources.get_mut(&id).ok_or(RESOURCE)?;
                if !resource.mapped {
                    return Err(INVALID);
                }
                self.mapper.unmap(id).map_err(|_| 0x1200u32)?;
                host(self.gpu.unmap(id))?;
                resource.mapped = false;
                NODATA
            }
            _ => return Err(INVALID),
        };
        Ok((response, out))
    }
}

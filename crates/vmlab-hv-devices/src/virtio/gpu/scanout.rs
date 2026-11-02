//! Direct scanout export and pooled EGL external-texture fallback.
//! Khronos EGL_EXT_image_dma_buf_import[_modifiers], GL_OES_EGL_image_external.

use glow::HasContext;
use rutabaga_gfx::{Resource3DInfo, ResourceCreate3D};
use std::ffi::{CStr, c_char, c_void};
use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;
use std::ptr::{null, null_mut};
use std::sync::Arc;

type Handle = *mut c_void;
type CreateImage = unsafe extern "C" fn(Handle, Handle, u32, Handle, *const i32) -> Handle;
type DestroyImage = unsafe extern "C" fn(Handle, Handle) -> u32;
type ImageTarget = unsafe extern "C" fn(u32, Handle);
type CreateSync = unsafe extern "C" fn(Handle, u32, *const i32) -> Handle;
type DestroySync = unsafe extern "C" fn(Handle, Handle) -> u32;
type DupFence = unsafe extern "C" fn(Handle, Handle) -> i32;
type QueryModifiers = unsafe extern "C" fn(Handle, i32, i32, *mut u64, *mut u32, *mut i32) -> u32;
type QueryImage = unsafe extern "C" fn(Handle, Handle, *mut i32, *mut i32, *mut u64) -> u32;

const INVALID_MODIFIER: u64 = (1 << 56) - 1;
type ReadPixels = Arc<dyn Fn() -> Result<Vec<u8>, u32> + Send + Sync>;

#[link(name = "EGL")]
unsafe extern "C" {
    fn eglGetPlatformDisplay(platform: u32, native: Handle, attrs: *const isize) -> Handle;
    fn eglInitialize(display: Handle, major: *mut i32, minor: *mut i32) -> u32;
    fn eglBindAPI(api: u32) -> u32;
    fn eglChooseConfig(
        display: Handle,
        attrs: *const i32,
        configs: *mut Handle,
        count: i32,
        found: *mut i32,
    ) -> u32;
    fn eglCreateContext(
        display: Handle,
        config: Handle,
        share: Handle,
        attrs: *const i32,
    ) -> Handle;
    fn eglMakeCurrent(display: Handle, draw: Handle, read: Handle, context: Handle) -> u32;
    fn eglDestroyContext(display: Handle, context: Handle) -> u32;
    fn eglGetProcAddress(name: *const c_char) -> *const c_void;
    fn eglGetError() -> u32;
    fn eglQueryAPI() -> u32;
    fn eglGetCurrentDisplay() -> Handle;
    fn eglGetCurrentContext() -> Handle;
    fn eglGetCurrentSurface(which: i32) -> Handle;
    fn virgl_renderer_resource_unref(id: u32);
    fn virgl_renderer_resource_import_eglimage(args: *const ResourceArgs, image: Handle) -> i32;
}

#[repr(C)]
struct ResourceArgs {
    id: u32,
    target: u32,
    format: u32,
    bind: u32,
    width: u32,
    height: u32,
    depth: u32,
    array_size: u32,
    last_level: u32,
    samples: u32,
    flags: u32,
}

#[link(name = "gbm")]
unsafe extern "C" {
    fn gbm_create_device(fd: i32) -> Handle;
    fn gbm_device_destroy(device: Handle);
    fn gbm_bo_create(device: Handle, width: u32, height: u32, format: u32, flags: u32) -> Handle;
    fn gbm_bo_create_with_modifiers2(
        device: Handle,
        width: u32,
        height: u32,
        format: u32,
        modifiers: *const u64,
        count: u32,
        flags: u32,
    ) -> Handle;
    fn gbm_bo_destroy(bo: Handle);
    fn gbm_bo_get_fd(bo: Handle) -> i32;
    fn gbm_bo_get_stride(bo: Handle) -> u32;
    fn gbm_bo_get_modifier(bo: Handle) -> u64;
    fn gbm_bo_import(device: Handle, kind: u32, data: *mut c_void, flags: u32) -> Handle;
}

#[repr(C)]
struct ImportFd {
    fd: i32,
    width: u32,
    height: u32,
    stride: u32,
    format: u32,
}

fn scanout_format(create: ResourceCreate3D) -> Option<u32> {
    // Preserve the renderer's allocation rules for linear, mapped, mipmapped
    // and multisampled resources. Only ordinary scanout render targets change.
    if create.bind & (1 << 18) == 0
        || create.bind & (1 << 22) != 0
        || create.flags & !1 != 0
        || create.target != 2
        || create.depth != 1
        || create.array_size != 1
        || create.last_level != 0
        || create.nr_samples > 1
    {
        return None;
    }
    match create.format {
        1 => Some(u32::from_le_bytes(*b"AR24")),
        2 => Some(u32::from_le_bytes(*b"XR24")),
        67 => Some(u32::from_le_bytes(*b"AB24")),
        134 => Some(u32::from_le_bytes(*b"XB24")),
        _ => None,
    }
}

/// A scanout publication. Acquire a lease before sampling; release after completion.
#[derive(Clone)]
pub struct DmaFrame {
    pub fd: Arc<OwnedFd>,
    pub acquire: Option<Arc<OwnedFd>>,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub fourcc: u32,
    pub modifier: u64,
    pub pixels: Option<Arc<Vec<u8>>>,
    pub buffers: Arc<Vec<Arc<OwnedFd>>>,
    pub buffer: u32,
    pub lease: Arc<vmlab_hv_display::BufferLease>,
    pub direct: bool,
    pub(super) reader: Option<ReadPixels>,
    pub(super) snapshot: bool,
}

impl DmaFrame {
    pub fn readback(&self) -> std::io::Result<Vec<u8>> {
        if let Some(pixels) = &self.pixels {
            return Ok(pixels.as_ref().clone());
        }
        let _guard = if self.snapshot {
            None
        } else {
            Some(self.lease.acquire().ok_or(std::io::ErrorKind::WouldBlock)?)
        };
        self.reader
            .as_ref()
            .ok_or_else(|| std::io::Error::other("GPU frame has no readback provider"))?()
        .map_err(|error| std::io::Error::other(format!("GPU readback failed: {error:#x}")))
    }
}

struct PoolBuffer {
    fd: Arc<OwnedFd>,
    stride: u32,
    modifier: u64,
    lease: Arc<vmlab_hv_display::BufferLease>,
}

pub(super) struct Scanout {
    platform: Platform,
    gl: glow::Context,
    program: glow::Program,
    image: CreateImage,
    destroy_image: DestroyImage,
    target: ImageTarget,
    pool: Vec<PoolBuffer>,
    size: (u32, u32),
    modifiers: std::collections::BTreeMap<u32, Vec<u64>>,
}

pub(super) struct Platform {
    display: Handle,
    context: Handle,
    gbm: Handle,
    _node: File,
}

struct Current {
    api: u32,
    display: Handle,
    context: Handle,
    draw: Handle,
    read: Handle,
}

impl Current {
    fn save() -> Self {
        // SAFETY: EGL current-state queries read this thread's bindings.
        unsafe {
            Self {
                api: eglQueryAPI(),
                display: eglGetCurrentDisplay(),
                context: eglGetCurrentContext(),
                draw: eglGetCurrentSurface(0x3059),
                read: eglGetCurrentSurface(0x305a),
            }
        }
    }
}

impl Drop for Current {
    fn drop(&mut self) {
        // SAFETY: capture borrows, and never destroys, the previous context and
        // surfaces. Restore the API as well as the context for libepoxy dispatch.
        unsafe {
            eglBindAPI(self.api);
            if !self.display.is_null() {
                eglMakeCurrent(self.display, self.draw, self.read, self.context);
            }
        }
    }
}

impl std::ops::Deref for Scanout {
    type Target = Platform;
    fn deref(&self) -> &Platform {
        &self.platform
    }
}

fn proc(name: &CStr) -> Result<*const c_void, u32> {
    // SAFETY: the name is terminated and EGL owns the returned code pointer.
    let ptr = unsafe { eglGetProcAddress(name.as_ptr()) };
    if ptr.is_null() { Err(0x1200) } else { Ok(ptr) }
}

impl Scanout {
    /// Give simple scanout-bound resources an explicit layout before guest use.
    /// GL texture exports otherwise report an implicit modifier on some drivers.
    pub fn resource(
        &self,
        id: u32,
        create: ResourceCreate3D,
    ) -> Result<Option<(Arc<OwnedFd>, Resource3DInfo)>, u32> {
        let Some(format) = scanout_format(create) else {
            return Ok(None);
        };
        // SAFETY: GBM borrows the device; the EGL image is imported into virgl's
        // current display and destroyed only after virgl has bound its storage.
        unsafe {
            let bo = self.allocate(create.width, create.height, format);
            if bo.is_null() {
                return Ok(None);
            }
            let raw = gbm_bo_get_fd(bo);
            let info = Resource3DInfo {
                width: create.width,
                height: create.height,
                drm_fourcc: format,
                strides: [gbm_bo_get_stride(bo), 0, 0, 0],
                offsets: [0; 4],
                modifier: gbm_bo_get_modifier(bo),
            };
            gbm_bo_destroy(bo);
            if raw < 0 {
                return Ok(None);
            }
            let fd = Arc::new(OwnedFd::from_raw_fd(raw));
            if !self
                .modifiers
                .get(&format)
                .is_some_and(|modifiers| modifiers.contains(&info.modifier))
            {
                return Ok(None);
            }
            let display = eglGetCurrentDisplay();
            let image = match self.import_on(display, raw, info, create.width, create.height) {
                Ok(image) => image,
                Err(_) => return Ok(None),
            };
            let args = ResourceArgs {
                id,
                target: create.target,
                format: create.format,
                bind: create.bind,
                width: create.width,
                height: create.height,
                depth: create.depth,
                array_size: create.array_size,
                last_level: create.last_level,
                samples: create.nr_samples,
                flags: create.flags,
            };
            virgl_renderer_resource_unref(id);
            let result = virgl_renderer_resource_import_eglimage(&args, image);
            (self.destroy_image)(display, image);
            if result != 0 {
                return Err(0x1200);
            }
            log::debug!(
                "explicit guest scanout allocation id={id} modifier={:#x}",
                info.modifier
            );
            Ok(Some((fd, info)))
        }
    }

    unsafe fn allocate(&self, width: u32, height: u32, format: u32) -> Handle {
        // SAFETY: callers keep the GBM device live and validate resource dimensions.
        unsafe {
            if let Some(modifiers) = self.modifiers.get(&format) {
                for modifier in modifiers {
                    let bo = gbm_bo_create_with_modifiers2(
                        self.gbm, width, height, format, modifier, 1, 4,
                    );
                    if !bo.is_null() {
                        return bo;
                    }
                }
            }
            gbm_bo_create(self.gbm, width, height, format, 4)
        }
    }
    pub fn new(node: &Path) -> Result<Self, u32> {
        let modifiers = super::modifiers::query(node);
        let node = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(node)
            .map_err(|_| 0x1200u32)?;
        let _current = Current::save();
        // SAFETY: all EGL/GBM objects are created, used and destroyed on the
        // renderer thread. Attribute lists terminate with EGL_NONE.
        unsafe {
            let gbm = gbm_create_device(node.as_raw_fd());
            if gbm.is_null() {
                return Err(0x1200);
            }
            let display = eglGetPlatformDisplay(0x31d7, gbm, null());
            let mut config = null_mut();
            let mut count = 0;
            if display.is_null()
                || eglInitialize(display, null_mut(), null_mut()) == 0
                || eglBindAPI(0x30a0) == 0
                || eglChooseConfig(
                    display,
                    [
                        0x3040, 4, 0x3033, 0, 0x3024, 8, 0x3023, 8, 0x3022, 8, 0x3038,
                    ]
                    .as_ptr(),
                    &mut config,
                    1,
                    &mut count,
                ) == 0
                || count == 0
            {
                log::warn!(
                    "GPU capture EGL display/config failed: {:#x}, configs={count}",
                    eglGetError()
                );
                gbm_device_destroy(gbm);
                return Err(0x1200);
            }
            let context =
                eglCreateContext(display, config, null_mut(), [0x3098, 2, 0x3038].as_ptr());
            if context.is_null() || eglMakeCurrent(display, null_mut(), null_mut(), context) == 0 {
                if !context.is_null() {
                    eglDestroyContext(display, context);
                }
                gbm_device_destroy(gbm);
                return Err(0x1200);
            }
            let platform = Platform {
                display,
                context,
                gbm,
                _node: node,
            };
            let gl = glow::Context::from_loader_function(|name| {
                let name = std::ffi::CString::new(name).unwrap();
                eglGetProcAddress(name.as_ptr())
            });
            log::info!(
                "GPU readback renderer: {}",
                gl.get_parameter_string(glow::RENDERER)
            );
            let program = gl.create_program().map_err(|_| 0x1200u32)?;
            for (kind, source) in [
                (
                    glow::VERTEX_SHADER,
                    "attribute vec2 p; varying vec2 uv; uniform vec4 crop; void main(){gl_Position=vec4(p,0.,1.);uv=crop.xy+(p*.5+.5)*crop.zw;}",
                ),
                (
                    glow::FRAGMENT_SHADER,
                    "#extension GL_OES_EGL_image_external : require\nprecision mediump float; varying vec2 uv; uniform samplerExternalOES image; void main(){gl_FragColor=texture2D(image,uv);}",
                ),
            ] {
                let shader = gl.create_shader(kind).map_err(|_| 0x1200u32)?;
                gl.shader_source(shader, source);
                gl.compile_shader(shader);
                if !gl.get_shader_compile_status(shader) {
                    log::warn!("GPU scanout shader: {}", gl.get_shader_info_log(shader));
                    gl.delete_shader(shader);
                    return Err(0x1200);
                }
                gl.attach_shader(program, shader);
                gl.delete_shader(shader);
            }
            gl.bind_attrib_location(program, 0, "p");
            gl.link_program(program);
            if !gl.get_program_link_status(program) {
                return Err(0x1200);
            }
            let mut scanout = Self {
                platform,
                gl,
                program,
                image: std::mem::transmute::<*const c_void, CreateImage>(proc(
                    c"eglCreateImageKHR",
                )?),
                destroy_image: std::mem::transmute::<*const c_void, DestroyImage>(proc(
                    c"eglDestroyImageKHR",
                )?),
                target: std::mem::transmute::<*const c_void, ImageTarget>(proc(
                    c"glEGLImageTargetTexture2DOES",
                )?),
                pool: Vec::new(),
                size: (0, 0),
                modifiers,
            };
            // Match both the consumer's Vulkan import and the producer's EGL
            // rendering support before choosing a GBM allocation modifier.
            let formats = scanout
                .modifiers
                .iter()
                .map(|(&fourcc, modifiers)| {
                    let modifiers = modifiers
                        .iter()
                        .copied()
                        .filter(|&modifier| {
                            scanout.modifier_external(Resource3DInfo {
                                drm_fourcc: fourcc,
                                modifier,
                                ..Default::default()
                            }) == Some(false)
                        })
                        .collect();
                    (fourcc, modifiers)
                })
                .collect();
            scanout.modifiers = formats;
            Ok(scanout)
        }
    }

    pub fn direct(&self, info: Resource3DInfo, width: u32, height: u32, rect: super::Rect) -> bool {
        log::debug!(
            "scanout candidate {info:?}, resource={width}x{height}, crop={},{},{}x{}",
            rect.x,
            rect.y,
            rect.w,
            rect.h
        );
        if rect.x != 0
            || rect.y != 0
            || rect.w != width
            || rect.h != height
            || info.offsets != [0; 4]
            || info.strides[1..] != [0; 3]
            || !self
                .modifiers
                .get(&info.drm_fourcc)
                .is_some_and(|modifiers| modifiers.contains(&info.modifier))
            || ![
                u32::from_le_bytes(*b"XR24"),
                u32::from_le_bytes(*b"AR24"),
                u32::from_le_bytes(*b"XB24"),
                u32::from_le_bytes(*b"AB24"),
            ]
            .contains(&info.drm_fourcc)
        {
            return false;
        }
        true
    }

    pub fn external_only(&self, info: Resource3DInfo) -> bool {
        self.modifier_external(info) == Some(true)
    }

    fn modifier_external(&self, info: Resource3DInfo) -> Option<bool> {
        // SAFETY: the extension validates the format; both arrays have count entries.
        unsafe {
            let Ok(query) = proc(c"eglQueryDmaBufModifiersEXT") else {
                return None;
            };
            let query = std::mem::transmute::<*const c_void, QueryModifiers>(query);
            let mut count = 0;
            if query(
                self.display,
                info.drm_fourcc as i32,
                0,
                null_mut(),
                null_mut(),
                &mut count,
            ) == 0
                || !(1..=4096).contains(&count)
            {
                return None;
            }
            let mut modifiers = vec![0; count as usize];
            let mut external = vec![0; count as usize];
            if query(
                self.display,
                info.drm_fourcc as i32,
                count,
                modifiers.as_mut_ptr(),
                external.as_mut_ptr(),
                &mut count,
            ) == 0
            {
                return None;
            }
            log::debug!("scanout modifiers {modifiers:?}, external-only={external:?}");
            modifiers
                .iter()
                .zip(&external)
                .find(|(modifier, _)| **modifier == info.modifier)
                .map(|(_, external)| *external != 0)
        }
    }

    pub fn metadata(
        &self,
        fd: i32,
        mut info: Resource3DInfo,
        width: u32,
        height: u32,
    ) -> Resource3DInfo {
        if ![u64::MAX, INVALID_MODIFIER].contains(&info.modifier) {
            return info;
        }
        let mut data = ImportFd {
            fd,
            width,
            height,
            stride: info.strides[0],
            format: info.drm_fourcc,
        };
        // SAFETY: GBM_BO_IMPORT_FD reads this public gbm.h structure and borrows fd.
        unsafe {
            let bo = gbm_bo_import(self.gbm, 0x5503, (&mut data as *mut ImportFd).cast(), 4);
            if !bo.is_null() {
                let modifier = gbm_bo_get_modifier(bo);
                log::debug!("GBM imported scanout modifier={modifier:#x}");
                if ![u64::MAX, INVALID_MODIFIER].contains(&modifier) {
                    info.modifier = modifier;
                }
                gbm_bo_destroy(bo);
                if ![u64::MAX, INVALID_MODIFIER].contains(&info.modifier) {
                    return info;
                }
            }
        }
        let Ok(image) = self.import(fd, info, width, height) else {
            return info;
        };
        // SAFETY: the image is live and the extension writes only the supplied scalars.
        unsafe {
            if let Ok(query) = proc(c"eglExportDMABUFImageQueryMESA") {
                let query = std::mem::transmute::<*const c_void, QueryImage>(query);
                let mut format = 0;
                let mut planes = 0;
                let mut modifier = INVALID_MODIFIER;
                let result = query(self.display, image, &mut format, &mut planes, &mut modifier);
                log::debug!(
                    "EGL image metadata result={result} planes={planes} modifier={modifier:#x}"
                );
                if result != 0 && planes == 1 {
                    info.drm_fourcc = format as u32;
                    info.modifier = modifier;
                }
            }
            (self.destroy_image)(self.display, image);
        }
        info
    }

    fn import(
        &self,
        fd: i32,
        info: Resource3DInfo,
        width: u32,
        height: u32,
    ) -> Result<Handle, u32> {
        self.import_on(self.display, fd, info, width, height)
    }

    fn import_on(
        &self,
        display: Handle,
        fd: i32,
        info: Resource3DInfo,
        width: u32,
        height: u32,
    ) -> Result<Handle, u32> {
        let mut attrs = vec![
            0x3057,
            width as i32,
            0x3056,
            height as i32,
            0x3271,
            info.drm_fourcc as i32,
            0x3272,
            fd,
            0x3273,
            info.offsets[0] as i32,
            0x3274,
            info.strides[0] as i32,
        ];
        if ![u64::MAX, INVALID_MODIFIER].contains(&info.modifier) {
            attrs.extend_from_slice(&[
                0x3443,
                info.modifier as i32,
                0x3444,
                (info.modifier >> 32) as i32,
            ]);
        }
        for plane in 1..4 {
            if info.strides[plane] == 0 {
                continue;
            }
            let (fd_key, offset_key, pitch_key, lo_key, hi_key) = match plane {
                1 => (0x3275, 0x3276, 0x3277, 0x3445, 0x3446),
                2 => (0x3278, 0x3279, 0x327a, 0x3447, 0x3448),
                _ => (0x3440, 0x3441, 0x3442, 0x3449, 0x344a),
            };
            attrs.extend_from_slice(&[
                fd_key,
                fd,
                offset_key,
                info.offsets[plane] as i32,
                pitch_key,
                info.strides[plane] as i32,
            ]);
            if ![u64::MAX, INVALID_MODIFIER].contains(&info.modifier) {
                attrs.extend_from_slice(&[
                    lo_key,
                    info.modifier as i32,
                    hi_key,
                    (info.modifier >> 32) as i32,
                ]);
            }
        }
        attrs.push(0x3038);
        // SAFETY: fd stays open through import; dimensions and metadata were
        // checked by the GPU command parser. EGL validates modifiers and pitch.
        let image =
            unsafe { (self.image)(display, null_mut(), 0x3270, null_mut(), attrs.as_ptr()) };
        if image.is_null() {
            // SAFETY: eglGetError reads thread-local error state.
            log::warn!("GPU dma-buf import failed: {:#x}, info={info:?}", unsafe {
                eglGetError()
            });
            Err(0x1200)
        } else {
            Ok(image)
        }
    }

    pub fn capture(
        &mut self,
        fd: i32,
        info: Resource3DInfo,
        width: u32,
        height: u32,
        rect: super::Rect,
        cpu: bool,
    ) -> Result<DmaFrame, u32> {
        let _current = Current::save();
        // SAFETY: this context is private to the renderer thread. Local GL
        // objects and EGL images remain live until rendering and readback finish.
        unsafe {
            if eglBindAPI(0x30a0) == 0
                || eglMakeCurrent(self.display, null_mut(), null_mut(), self.context) == 0
            {
                return Err(0x1200);
            }
            let source = self.import(fd, info, width, height)?;
            if self.size != (rect.w, rect.h) {
                self.pool.clear();
                self.size = (rect.w, rect.h);
            }
            let result = (|| {
                if self.pool.is_empty() {
                    let mut pool = Vec::new();
                    for _ in 0..3 {
                        let format = u32::from_le_bytes(*b"XR24");
                        let mut bo = null_mut();
                        if let Some(modifiers) = self.modifiers.get(&format) {
                            for modifier in modifiers {
                                bo = gbm_bo_create_with_modifiers2(
                                    self.gbm, rect.w, rect.h, format, modifier, 1, 4,
                                );
                                if !bo.is_null() {
                                    break;
                                }
                            }
                        }
                        if bo.is_null() {
                            bo = gbm_bo_create(self.gbm, rect.w, rect.h, format, 4);
                        }
                        if bo.is_null() {
                            return Err(0x1200);
                        }
                        let raw = gbm_bo_get_fd(bo);
                        let stride = gbm_bo_get_stride(bo);
                        let modifier = gbm_bo_get_modifier(bo);
                        log::debug!("scanout pool buffer modifier={modifier:#x}, stride={stride}");
                        gbm_bo_destroy(bo);
                        if raw < 0 {
                            return Err(0x1200);
                        }
                        pool.push(PoolBuffer {
                            fd: Arc::new(OwnedFd::from_raw_fd(raw)),
                            stride,
                            modifier,
                            lease: vmlab_hv_display::BufferLease::new(),
                        });
                    }
                    self.pool = pool;
                }
                let index = self
                    .pool
                    .iter()
                    .position(|buffer| buffer.lease.try_retire())
                    .ok_or(super::EXPORT_BUSY)?;
                self.pool[index].lease = vmlab_hv_display::BufferLease::new();
                let buffer = &self.pool[index];
                let raw = buffer.fd.as_raw_fd();
                let stride = buffer.stride;
                let modifier = buffer.modifier;
                let destination = self.import(
                    raw,
                    Resource3DInfo {
                        drm_fourcc: u32::from_le_bytes(*b"XR24"),
                        strides: [stride, 0, 0, 0],
                        modifier,
                        ..Default::default()
                    },
                    rect.w,
                    rect.h,
                )?;
                let result = self
                    .draw(source, destination, (width, height), rect, stride, cpu)
                    .map(|pixels| DmaFrame {
                        fd: buffer.fd.clone(),
                        acquire: self.fence(),
                        width: rect.w,
                        height: rect.h,
                        stride,
                        fourcc: u32::from_le_bytes(*b"XR24"),
                        modifier,
                        pixels: pixels.map(Arc::new),
                        buffers: Arc::new(
                            self.pool.iter().map(|buffer| buffer.fd.clone()).collect(),
                        ),
                        buffer: index as u32,
                        lease: buffer.lease.clone(),
                        direct: false,
                        reader: None,
                        snapshot: false,
                    });
                (self.destroy_image)(self.display, destination);
                result
            })();
            (self.destroy_image)(self.display, source);
            result
        }
    }

    fn draw(
        &self,
        source: Handle,
        destination: Handle,
        dimensions: (u32, u32),
        rect: super::Rect,
        stride: u32,
        cpu: bool,
    ) -> Result<Option<Vec<u8>>, u32> {
        let (width, height) = dimensions;
        let gl = &self.gl;
        // SAFETY: capture made this private context current and keeps both images
        // alive. CPU pixel storage has exactly the dimensions passed to GL.
        unsafe {
            let input = gl.create_texture().map_err(|_| 0x1200u32)?;
            let output = gl.create_texture().map_err(|_| 0x1200u32)?;
            let fbo = gl.create_framebuffer().map_err(|_| 0x1200u32)?;
            let vertices = gl.create_buffer().map_err(|_| 0x1200u32)?;
            let result =
                (|| {
                    gl.active_texture(glow::TEXTURE0);
                    gl.bind_texture(0x8d65, Some(input));
                    (self.target)(0x8d65, source);
                    gl.tex_parameter_i32(0x8d65, glow::TEXTURE_MIN_FILTER, glow::NEAREST as i32);
                    gl.tex_parameter_i32(0x8d65, glow::TEXTURE_MAG_FILTER, glow::NEAREST as i32);
                    gl.tex_parameter_i32(0x8d65, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
                    gl.tex_parameter_i32(0x8d65, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
                    gl.bind_texture(glow::TEXTURE_2D, Some(output));
                    (self.target)(glow::TEXTURE_2D, destination);
                    gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
                    gl.framebuffer_texture_2d(
                        glow::FRAMEBUFFER,
                        glow::COLOR_ATTACHMENT0,
                        glow::TEXTURE_2D,
                        Some(output),
                        0,
                    );
                    if gl.check_framebuffer_status(glow::FRAMEBUFFER) != glow::FRAMEBUFFER_COMPLETE
                    {
                        return Err(0x1200);
                    }
                    gl.use_program(Some(self.program));
                    gl.uniform_1_i32(gl.get_uniform_location(self.program, "image").as_ref(), 0);
                    gl.uniform_4_f32(
                        gl.get_uniform_location(self.program, "crop").as_ref(),
                        rect.x as f32 / width as f32,
                        rect.y as f32 / height as f32,
                        rect.w as f32 / width as f32,
                        rect.h as f32 / height as f32,
                    );
                    gl.bind_buffer(glow::ARRAY_BUFFER, Some(vertices));
                    let points = [-1f32, -1., 1., -1., -1., 1., 1., 1.];
                    let bytes = std::slice::from_raw_parts(
                        points.as_ptr().cast(),
                        std::mem::size_of_val(&points),
                    );
                    gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytes, glow::STATIC_DRAW);
                    gl.enable_vertex_attrib_array(0);
                    gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, 0, 0);
                    gl.viewport(0, 0, rect.w as i32, rect.h as i32);
                    gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                    if !cpu {
                        gl.finish();
                        return if gl.get_error() == glow::NO_ERROR {
                            Ok(None)
                        } else {
                            Err(0x1200)
                        };
                    }
                    let mut rgba = vec![0; rect.w as usize * rect.h as usize * 4];
                    gl.read_pixels(
                        0,
                        0,
                        rect.w as i32,
                        rect.h as i32,
                        glow::RGBA,
                        glow::UNSIGNED_BYTE,
                        glow::PixelPackData::Slice(Some(&mut rgba)),
                    );
                    gl.finish();
                    if gl.get_error() != glow::NO_ERROR {
                        return Err(0x1200);
                    }
                    let mut pixels = vec![0; stride as usize * rect.h as usize];
                    for (src, dst) in rgba.as_chunks::<4>().0.iter().zip(
                        pixels.chunks_mut(stride as usize).flat_map(|row| {
                            row[..rect.w as usize * 4].as_chunks_mut::<4>().0.iter_mut()
                        }),
                    ) {
                        dst.copy_from_slice(&[src[2], src[1], src[0], src[3]]);
                    }
                    Ok(Some(pixels))
                })();
            gl.delete_buffer(vertices);
            gl.delete_framebuffer(fbo);
            gl.delete_texture(input);
            gl.delete_texture(output);
            result
        }
    }

    pub fn readback(
        &mut self,
        fd: i32,
        info: Resource3DInfo,
        width: u32,
        height: u32,
    ) -> Result<Vec<u8>, u32> {
        let _current = Current::save();
        // SAFETY: published buffers are renderable single-plane images. The
        // caller holds a lease until readback completes. No pool slot is reused.
        unsafe {
            if eglBindAPI(0x30a0) == 0
                || eglMakeCurrent(self.display, null_mut(), null_mut(), self.context) == 0
            {
                return Err(0x1200);
            }
            let image = self.import(fd, info, width, height)?;
            let gl = &self.gl;
            let texture = gl.create_texture().map_err(|_| 0x1200u32)?;
            let fbo = gl.create_framebuffer().map_err(|_| 0x1200u32)?;
            let result = (|| {
                gl.bind_texture(glow::TEXTURE_2D, Some(texture));
                (self.target)(glow::TEXTURE_2D, image);
                gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
                gl.framebuffer_texture_2d(
                    glow::FRAMEBUFFER,
                    glow::COLOR_ATTACHMENT0,
                    glow::TEXTURE_2D,
                    Some(texture),
                    0,
                );
                if gl.check_framebuffer_status(glow::FRAMEBUFFER) != glow::FRAMEBUFFER_COMPLETE {
                    return Err(0x1200);
                }
                let mut rgba = vec![0; width as usize * height as usize * 4];
                gl.read_pixels(
                    0,
                    0,
                    width as i32,
                    height as i32,
                    glow::RGBA,
                    glow::UNSIGNED_BYTE,
                    glow::PixelPackData::Slice(Some(&mut rgba)),
                );
                gl.finish();
                if gl.get_error() != glow::NO_ERROR {
                    return Err(0x1200);
                }
                for pixel in rgba.as_chunks_mut::<4>().0 {
                    pixel.swap(0, 2);
                }
                Ok(rgba)
            })();
            gl.delete_framebuffer(fbo);
            gl.delete_texture(texture);
            (self.destroy_image)(self.display, image);
            result
        }
    }

    fn fence(&self) -> Option<Arc<OwnedFd>> {
        // SAFETY: extension pointers have the signatures defined by
        // EGL_ANDROID_native_fence_sync. glFinish has already completed writes,
        // so implementations without this extension need no acquire fence.
        unsafe {
            let create =
                std::mem::transmute::<*const c_void, CreateSync>(proc(c"eglCreateSyncKHR").ok()?);
            let destroy =
                std::mem::transmute::<*const c_void, DestroySync>(proc(c"eglDestroySyncKHR").ok()?);
            let dup = std::mem::transmute::<*const c_void, DupFence>(
                proc(c"eglDupNativeFenceFDANDROID").ok()?,
            );
            let sync = create(self.display, 0x3144, [0x3038].as_ptr());
            if sync.is_null() {
                return None;
            }
            self.gl.flush();
            let fd = dup(self.display, sync);
            destroy(self.display, sync);
            (fd >= 0).then(|| Arc::new(OwnedFd::from_raw_fd(fd)))
        }
    }
}

impl Drop for Scanout {
    fn drop(&mut self) {
        let _current = Current::save();
        // SAFETY: worker shutdown happens after all tasks. The node fd remains
        // live; the display is shared with virglrenderer and is not terminated.
        unsafe {
            eglBindAPI(0x30a0);
            eglMakeCurrent(self.display, null_mut(), null_mut(), self.context);
            self.gl.delete_program(self.program);
        }
    }
}

impl Drop for Platform {
    fn drop(&mut self) {
        let mut current = Current::save();
        if current.context == self.context {
            current.display = null_mut();
            current.context = null_mut();
        }
        let _current = current;
        // SAFETY: this object owns the context and GBM device on the renderer
        // thread. It is dropped after the GL program and all capture objects.
        unsafe {
            eglMakeCurrent(self.display, null_mut(), null_mut(), null_mut());
            eglDestroyContext(self.display, self.context);
            gbm_device_destroy(self.gbm);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scanout_allocation_preserves_special_resource_constraints() {
        let create = ResourceCreate3D {
            target: 2,
            format: 2,
            bind: 2 | (1 << 18),
            width: 64,
            height: 64,
            depth: 1,
            array_size: 1,
            last_level: 0,
            nr_samples: 0,
            flags: 1,
        };
        assert_eq!(scanout_format(create), Some(u32::from_le_bytes(*b"XR24")));
        assert_eq!(
            scanout_format(ResourceCreate3D {
                bind: create.bind | (1 << 22),
                ..create
            }),
            None
        );
        assert_eq!(
            scanout_format(ResourceCreate3D { flags: 3, ..create }),
            None
        );
        assert_eq!(
            scanout_format(ResourceCreate3D {
                last_level: 1,
                ..create
            }),
            None
        );
        assert_eq!(
            scanout_format(ResourceCreate3D {
                nr_samples: 4,
                ..create
            }),
            None
        );
    }

    #[test]
    fn external_only_scanout_reuses_pool_and_keeps_held_gpu_pixels() {
        let Some(node) = std::env::var_os("VMLAB_HV_TEST_RENDER_NODE") else {
            eprintln!("SKIP external-only scanout: set VMLAB_HV_TEST_RENDER_NODE");
            return;
        };
        let mut scanout = Scanout::new(Path::new(&node)).unwrap();
        struct Bo(Handle);
        impl Drop for Bo {
            fn drop(&mut self) {
                // SAFETY: this test owns the BO and drops it before its GBM device.
                unsafe {
                    gbm_bo_destroy(self.0);
                }
            }
        }
        unsafe extern "C" {
            fn gbm_bo_map(
                bo: Handle,
                x: u32,
                y: u32,
                w: u32,
                h: u32,
                flags: u32,
                stride: *mut u32,
                data: *mut Handle,
            ) -> Handle;
            fn gbm_bo_unmap(bo: Handle, data: Handle);
        }
        let format = u32::from_le_bytes(*b"XR24");
        // SAFETY: allocate only on the selected render node, with GBM linear/write flags.
        let bo = Bo(unsafe { gbm_bo_create(scanout.gbm, 64, 32, format, 8 | 16) });
        if bo.0.is_null() {
            eprintln!(
                "SKIP external-only scanout: selected GBM driver cannot allocate a linear writable BO"
            );
            std::mem::forget(bo);
            return;
        }
        // SAFETY: the BO is live; gbm_bo_get_fd returns an independently owned fd.
        let (raw, info) = unsafe {
            (
                gbm_bo_get_fd(bo.0),
                Resource3DInfo {
                    width: 64,
                    height: 32,
                    drm_fourcc: format,
                    strides: [gbm_bo_get_stride(bo.0), 0, 0, 0],
                    offsets: [0; 4],
                    modifier: gbm_bo_get_modifier(bo.0),
                },
            )
        };
        assert!(raw >= 0);
        // SAFETY: GBM returned a new descriptor.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        if !scanout.external_only(info) {
            eprintln!(
                "SKIP external-only scanout: selected driver does not mark linear XR24 external-only"
            );
            return;
        }
        let paint = |colour: [u8; 4]| {
            let mut stride = 0;
            let mut data = null_mut();
            // SAFETY: map the owned linear BO for CPU writes, then unmap before GPU sampling.
            unsafe {
                let memory = gbm_bo_map(bo.0, 0, 0, 64, 32, 2, &mut stride, &mut data);
                assert!(!memory.is_null());
                for y in 0..32 {
                    let row = std::slice::from_raw_parts_mut(
                        memory.cast::<u8>().add(y * stride as usize),
                        64 * 4,
                    );
                    for pixel in row.as_chunks_mut::<4>().0 {
                        pixel.copy_from_slice(&colour);
                    }
                }
                gbm_bo_unmap(bo.0, data);
            }
        };
        let rect = super::super::Rect {
            x: 0,
            y: 0,
            w: 64,
            h: 32,
        };
        paint([23, 41, 193, 0]);
        let first = scanout
            .capture(fd.as_raw_fd(), info, 64, 32, rect, false)
            .unwrap();
        assert!(first.pixels.is_none());
        let held = first.lease.acquire().unwrap();
        paint([199, 31, 17, 0]);
        let second = scanout
            .capture(fd.as_raw_fd(), info, 64, 32, rect, false)
            .unwrap();
        assert_ne!(first.buffer, second.buffer);
        let output_info = Resource3DInfo {
            drm_fourcc: first.fourcc,
            strides: [first.stride, 0, 0, 0],
            modifier: first.modifier,
            ..Default::default()
        };
        let old = scanout
            .readback(first.fd.as_raw_fd(), output_info, 64, 32)
            .unwrap();
        assert_eq!(&old[..3], &[23, 41, 193]);
        drop(held);
        let third = scanout
            .capture(fd.as_raw_fd(), info, 64, 32, rect, false)
            .unwrap();
        assert_eq!(third.buffer, first.buffer);
        assert!(Arc::ptr_eq(&third.fd, &first.fd));
        eprintln!(
            "external-only source modifier={:#x}, pooled modifier={:#x}; held GPU pixels preserved",
            info.modifier, first.modifier
        );
    }
}

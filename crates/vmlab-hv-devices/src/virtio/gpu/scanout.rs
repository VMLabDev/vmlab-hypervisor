//! Immutable scanout copies using EGL dma-buf import and external-texture sampling.
//! Khronos EGL_EXT_image_dma_buf_import[_modifiers], GL_OES_EGL_image_external.

use glow::HasContext;
use rutabaga_gfx::Resource3DInfo;
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
}

#[link(name = "gbm")]
unsafe extern "C" {
    fn gbm_create_device(fd: i32) -> Handle;
    fn gbm_device_destroy(device: Handle);
    fn gbm_bo_create(device: Handle, width: u32, height: u32, format: u32, flags: u32) -> Handle;
    fn gbm_bo_destroy(bo: Handle);
    fn gbm_bo_get_fd(bo: Handle) -> i32;
    fn gbm_bo_get_stride(bo: Handle) -> u32;
    fn gbm_bo_get_modifier(bo: Handle) -> u64;
}

/// Every fd owns a fresh allocation, independent of subsequent guest writes.
#[derive(Clone)]
pub struct DmaFrame {
    pub fd: Arc<OwnedFd>,
    pub acquire: Option<Arc<OwnedFd>>,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub fourcc: u32,
    pub modifier: u64,
    pub pixels: Arc<Vec<u8>>,
}

pub(super) struct Scanout {
    platform: Platform,
    gl: glow::Context,
    program: glow::Program,
    image: CreateImage,
    destroy_image: DestroyImage,
    target: ImageTarget,
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
    pub fn new(node: &Path) -> Result<Self, u32> {
        let node = File::open(node).map_err(|_| 0x1200u32)?;
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
                eprintln!(
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
            eprintln!(
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
                    eprintln!("GPU scanout shader: {}", gl.get_shader_info_log(shader));
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
            Ok(Self {
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
            })
        }
    }

    fn import(
        &self,
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
        if info.modifier != u64::MAX {
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
            if info.modifier != u64::MAX {
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
            unsafe { (self.image)(self.display, null_mut(), 0x3270, null_mut(), attrs.as_ptr()) };
        if image.is_null() {
            // SAFETY: eglGetError reads thread-local error state.
            eprintln!("GPU dma-buf import failed: {:#x}, info={info:?}", unsafe {
                eglGetError()
            });
            Err(0x1200)
        } else {
            Ok(image)
        }
    }

    pub fn capture(
        &self,
        fd: i32,
        info: Resource3DInfo,
        width: u32,
        height: u32,
        rect: super::Rect,
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
            let bo = gbm_bo_create(self.gbm, rect.w, rect.h, u32::from_le_bytes(*b"XR24"), 4);
            if bo.is_null() {
                (self.destroy_image)(self.display, source);
                return Err(0x1200);
            }
            let result = (|| {
                let raw = gbm_bo_get_fd(bo);
                if raw < 0 {
                    return Err(0x1200);
                }
                let exported = OwnedFd::from_raw_fd(raw);
                let stride = gbm_bo_get_stride(bo);
                let modifier = gbm_bo_get_modifier(bo);
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
                    .draw(source, destination, width, height, rect, stride)
                    .map(|pixels| DmaFrame {
                        fd: Arc::new(exported),
                        acquire: self.fence(),
                        width: rect.w,
                        height: rect.h,
                        stride,
                        fourcc: u32::from_le_bytes(*b"XR24"),
                        modifier,
                        pixels: Arc::new(pixels),
                    });
                (self.destroy_image)(self.display, destination);
                result
            })();
            gbm_bo_destroy(bo);
            (self.destroy_image)(self.display, source);
            result
        }
    }

    fn draw(
        &self,
        source: Handle,
        destination: Handle,
        width: u32,
        height: u32,
        rect: super::Rect,
        stride: u32,
    ) -> Result<Vec<u8>, u32> {
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
                        dst.copy_from_slice(&[src[2], src[1], src[0], 0]);
                    }
                    Ok(pixels)
                })();
            gl.delete_buffer(vertices);
            gl.delete_framebuffer(fbo);
            gl.delete_texture(input);
            gl.delete_texture(output);
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
        // SAFETY: this object owns the context and GBM device on the renderer
        // thread. It is dropped after the GL program and all capture objects.
        unsafe {
            eglMakeCurrent(self.display, null_mut(), null_mut(), null_mut());
            eglDestroyContext(self.display, self.context);
            gbm_device_destroy(self.gbm);
        }
    }
}

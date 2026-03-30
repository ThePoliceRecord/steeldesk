//! Minimal VAAPI bindings for DMA-BUF import and encoding.
//!
//! Uses `dlopen` to load `libva.so.2` at runtime so there is no link-time
//! dependency on the VAAPI SDK.  If `libva` is not installed on the target
//! system, [`VaapiLib::load`] returns `None` and the rest of the module is
//! inert.
//!
//! # Scope
//!
//! Only the subset of the VAAPI surface required for DMA-BUF import and
//! H.264/H.265 encode is declared here.  The full VAAPI API has hundreds of
//! entry points; we intentionally keep this minimal.
//!
//! # Safety
//!
//! All VAAPI calls go through raw FFI function pointers obtained via `dlsym`.
//! Callers must ensure that:
//!
//! 1. The `VADisplay` was obtained from a valid DRM fd (via `vaGetDisplayDRM`).
//! 2. DMA-BUF file descriptors passed to [`import_dmabuf_to_surface`] are
//!    valid for the lifetime of the returned `VASurfaceID`.
//! 3. Surfaces are destroyed before the display is terminated.

use std::os::raw::{c_char, c_int, c_uint, c_void};
use std::sync::Once;

use super::DmaBufFrame;

// ---------------------------------------------------------------------------
// Type aliases matching <va/va.h>
// ---------------------------------------------------------------------------

/// Opaque VAAPI display handle (`VADisplay` in C).
pub type VADisplay = *mut c_void;

/// Surface identifier (`VASurfaceID` in C, typedef unsigned int).
pub type VASurfaceID = c_uint;

/// Configuration identifier.
pub type VAConfigID = c_uint;

/// Context identifier.
pub type VAContextID = c_uint;

/// Buffer identifier.
pub type VABufferID = c_uint;

/// VAAPI status code (`VAStatus` in C, typedef int).
pub type VAStatus = c_int;

// ---------------------------------------------------------------------------
// Constants from <va/va.h>
// ---------------------------------------------------------------------------

/// Operation completed successfully.
pub const VA_STATUS_SUCCESS: VAStatus = 0;
/// Generic operation failure.
pub const VA_STATUS_ERROR_OPERATION_FAILED: VAStatus = 1;
/// Allocation failure.
pub const VA_STATUS_ERROR_ALLOCATION_FAILED: VAStatus = 2;
/// Invalid display.
pub const VA_STATUS_ERROR_INVALID_DISPLAY: VAStatus = 3;
/// Invalid config.
pub const VA_STATUS_ERROR_INVALID_CONFIG: VAStatus = 4;
/// Invalid context.
pub const VA_STATUS_ERROR_INVALID_CONTEXT: VAStatus = 5;
/// Invalid surface.
pub const VA_STATUS_ERROR_INVALID_SURFACE: VAStatus = 6;
/// Invalid buffer.
pub const VA_STATUS_ERROR_INVALID_BUFFER: VAStatus = 7;
/// Invalid parameter.
pub const VA_STATUS_ERROR_INVALID_PARAMETER: VAStatus = 8;

/// Surface format: YUV 4:2:0 8-bit (NV12, I420, etc.).
pub const VA_RT_FORMAT_YUV420: c_uint = 0x0000_0001;
/// Surface format: YUV 4:2:0 10-bit (P010).
pub const VA_RT_FORMAT_YUV420_10: c_uint = 0x0000_0100;
/// Surface format: YUV 4:2:2 8-bit.
pub const VA_RT_FORMAT_YUV422: c_uint = 0x0000_0002;
/// Surface format: RGB 32-bit.
pub const VA_RT_FORMAT_RGB32: c_uint = 0x0000_0011;

/// Profile: H.264 Main.
pub const VA_PROFILE_H264_MAIN: c_uint = 6;
/// Profile: H.264 High.
pub const VA_PROFILE_H264_HIGH: c_uint = 7;
/// Profile: H.265 (HEVC) Main.
pub const VA_PROFILE_HEVC_MAIN: c_uint = 12;
/// Profile: H.265 (HEVC) Main 10.
pub const VA_PROFILE_HEVC_MAIN10: c_uint = 13;

/// Entrypoint: video encode (low-power variant preferred by modern drivers).
pub const VA_ENTRYPOINT_ENCSLICE: c_uint = 6;
/// Entrypoint: video encode (low-power).
pub const VA_ENTRYPOINT_ENCSLICE_LP: c_uint = 8;

/// Surface attribute type: pixel format.
pub const VA_SURFACE_ATTRIB_PIXEL_FORMAT: c_uint = 1;
/// Surface attribute type: memory type.
pub const VA_SURFACE_ATTRIB_MEM_TYPE: c_uint = 4;
/// Surface attribute type: external buffer descriptor.
pub const VA_SURFACE_ATTRIB_EXTERNAL_BUFFER_DESCRIPTOR: c_uint = 5;

/// Memory type flag: DRM PRIME (DMA-BUF fd).
pub const VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME: c_uint = 0x2000_0000;

/// Surface attribute flag: the attribute value is being *set* by the caller.
pub const VA_SURFACE_ATTRIB_FLAG_SET: c_uint = 2;

/// VA fourcc for NV12.
pub const VA_FOURCC_NV12: c_uint = 0x3231_564E;
/// VA fourcc for P010 (10-bit 4:2:0).
pub const VA_FOURCC_P010: c_uint = 0x3031_3050;

// ---------------------------------------------------------------------------
// Struct layouts matching <va/va.h> / <va/va_drmcommon.h>
// ---------------------------------------------------------------------------

/// `VASurfaceAttrib` — key/value pair used when creating surfaces.
///
/// Layout matches the C struct exactly (verified via static assertions below).
#[repr(C)]
#[derive(Clone)]
pub struct VASurfaceAttrib {
    /// Attribute type (e.g. `VA_SURFACE_ATTRIB_PIXEL_FORMAT`).
    pub type_: c_uint,
    /// Flags (e.g. `VA_SURFACE_ATTRIB_FLAG_SET`).
    pub flags: c_uint,
    /// Value — interpreted according to `type_`.
    pub value: VAGenericValue,
}

/// `VAGenericValue` — tagged union for attribute values.
///
/// The real C type is a `{ int type; union { int i; float f; void *p; ... } }`.
/// We store the largest variant (`*mut c_void`) and reinterpret as needed.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct VAGenericValue {
    /// Value type tag: 0 = none, 1 = int, 2 = float, 3 = pointer, 4 = func.
    pub type_: c_uint,
    /// Padding (on 64-bit, aligns the union to 8 bytes).
    pub _pad: c_uint,
    /// The value, stored as a pointer-sized integer.  For integer values,
    /// only the low 32 bits are significant.
    pub value: usize,
}

impl VAGenericValue {
    /// Create an integer-typed value.
    pub fn int(v: c_int) -> Self {
        Self {
            type_: 1, // VAGenericValueTypeInteger
            _pad: 0,
            value: v as usize,
        }
    }

    /// Create a pointer-typed value.
    pub fn pointer(p: *mut c_void) -> Self {
        Self {
            type_: 3, // VAGenericValueTypePointer
            _pad: 0,
            value: p as usize,
        }
    }
}

/// `VASurfaceAttribExternalBuffers` — describes an externally-allocated
/// buffer (e.g. a DMA-BUF) to be imported as a VA surface.
///
/// Used with `VA_SURFACE_ATTRIB_EXTERNAL_BUFFER_DESCRIPTOR` when calling
/// `vaCreateSurfaces`.
#[repr(C)]
pub struct VASurfaceAttribExternalBuffers {
    /// VA fourcc pixel format (e.g. `VA_FOURCC_NV12`).
    pub pixel_format: c_uint,
    /// Width in pixels.
    pub width: c_uint,
    /// Height in pixels.
    pub height: c_uint,
    /// Total buffer size in bytes.
    pub data_size: c_uint,
    /// Number of planes (1 for NV12 single-buffer, 2 for split-plane).
    pub num_planes: c_uint,
    /// Per-plane row stride in bytes.
    pub pitches: [c_uint; 4],
    /// Per-plane byte offset from the start of the buffer.
    pub offsets: [c_uint; 4],
    /// Pointer to an array of buffer handles (DMA-BUF fds cast to `usize`).
    pub buffers: *mut usize,
    /// Number of entries in `buffers`.
    pub num_buffers: c_uint,
    /// Flags (reserved, set to 0).
    pub flags: c_uint,
    /// Driver-private data pointer (set to null).
    pub private_data: *mut c_void,
}

// ---------------------------------------------------------------------------
// Function-pointer table loaded via dlopen
// ---------------------------------------------------------------------------

/// Runtime-loaded VAAPI function pointers.
///
/// Loaded once via [`VaapiLib::load`].  All function pointers are non-null
/// after a successful load.
pub struct VaapiLib {
    _handle: *mut c_void,

    // -- display lifecycle --
    pub va_initialize: unsafe extern "C" fn(
        dpy: VADisplay,
        major_version: *mut c_int,
        minor_version: *mut c_int,
    ) -> VAStatus,
    pub va_terminate: unsafe extern "C" fn(dpy: VADisplay) -> VAStatus,

    // -- surface management --
    pub va_create_surfaces: unsafe extern "C" fn(
        dpy: VADisplay,
        format: c_uint,
        width: c_uint,
        height: c_uint,
        surfaces: *mut VASurfaceID,
        num_surfaces: c_uint,
        attrib_list: *mut VASurfaceAttrib,
        num_attribs: c_uint,
    ) -> VAStatus,
    pub va_destroy_surfaces: unsafe extern "C" fn(
        dpy: VADisplay,
        surfaces: *mut VASurfaceID,
        num_surfaces: c_int,
    ) -> VAStatus,

    // -- config --
    pub va_create_config: unsafe extern "C" fn(
        dpy: VADisplay,
        profile: c_uint,
        entrypoint: c_uint,
        attrib_list: *mut c_void,
        num_attribs: c_int,
        config_id: *mut VAConfigID,
    ) -> VAStatus,
    pub va_destroy_config:
        unsafe extern "C" fn(dpy: VADisplay, config_id: VAConfigID) -> VAStatus,

    // -- context --
    pub va_create_context: unsafe extern "C" fn(
        dpy: VADisplay,
        config_id: VAConfigID,
        picture_width: c_int,
        picture_height: c_int,
        flag: c_int,
        render_targets: *mut VASurfaceID,
        num_render_targets: c_int,
        context: *mut VAContextID,
    ) -> VAStatus,
    pub va_destroy_context:
        unsafe extern "C" fn(dpy: VADisplay, context: VAContextID) -> VAStatus,

    // -- encode buffer management --
    pub va_create_buffer: unsafe extern "C" fn(
        dpy: VADisplay,
        context: VAContextID,
        type_: c_uint,
        size: c_uint,
        num_elements: c_uint,
        data: *mut c_void,
        buf_id: *mut VABufferID,
    ) -> VAStatus,
    pub va_destroy_buffer:
        unsafe extern "C" fn(dpy: VADisplay, buffer_id: VABufferID) -> VAStatus,
    pub va_map_buffer: unsafe extern "C" fn(
        dpy: VADisplay,
        buf_id: VABufferID,
        pbuf: *mut *mut c_void,
    ) -> VAStatus,
    pub va_unmap_buffer:
        unsafe extern "C" fn(dpy: VADisplay, buf_id: VABufferID) -> VAStatus,

    // -- encode pipeline --
    pub va_begin_picture: unsafe extern "C" fn(
        dpy: VADisplay,
        context: VAContextID,
        render_target: VASurfaceID,
    ) -> VAStatus,
    pub va_render_picture: unsafe extern "C" fn(
        dpy: VADisplay,
        context: VAContextID,
        buffers: *mut VABufferID,
        num_buffers: c_int,
    ) -> VAStatus,
    pub va_end_picture:
        unsafe extern "C" fn(dpy: VADisplay, context: VAContextID) -> VAStatus,
    pub va_sync_surface:
        unsafe extern "C" fn(dpy: VADisplay, render_target: VASurfaceID) -> VAStatus,

    // -- error reporting --
    pub va_error_str: unsafe extern "C" fn(status: VAStatus) -> *const c_char,
}

// dlsym helper — returns None on failure.
unsafe fn load_sym(handle: *mut c_void, name: &[u8]) -> Option<*mut c_void> {
    let sym = libc::dlsym(handle, name.as_ptr() as *const c_char);
    if sym.is_null() {
        None
    } else {
        Some(sym)
    }
}

macro_rules! load_fn {
    ($handle:expr, $name:literal) => {
        match load_sym($handle, concat!($name, "\0").as_bytes()) {
            Some(p) => std::mem::transmute(p),
            None => return None,
        }
    };
}

impl VaapiLib {
    /// Attempt to load `libva.so.2` (or `libva.so`) via `dlopen`.
    ///
    /// Returns `None` if the library is not installed or any required symbol
    /// is missing.  This function is safe to call on any platform — on
    /// non-Linux it always returns `None`.
    pub fn load() -> Option<Self> {
        #[cfg(not(target_os = "linux"))]
        {
            return None;
        }

        #[cfg(target_os = "linux")]
        {
            use crate::libc;

            // Try versioned soname first, then unversioned.
            let handle = unsafe {
                let h = libc::dlopen(
                    b"libva.so.2\0".as_ptr() as *const c_char,
                    libc::RTLD_LAZY | libc::RTLD_LOCAL,
                );
                if h.is_null() {
                    let h = libc::dlopen(
                        b"libva.so\0".as_ptr() as *const c_char,
                        libc::RTLD_LAZY | libc::RTLD_LOCAL,
                    );
                    if h.is_null() {
                        return None;
                    }
                    h
                } else {
                    h
                }
            };

            unsafe {
                Some(VaapiLib {
                    _handle: handle,
                    va_initialize: load_fn!(handle, "vaInitialize"),
                    va_terminate: load_fn!(handle, "vaTerminate"),
                    va_create_surfaces: load_fn!(handle, "vaCreateSurfaces"),
                    va_destroy_surfaces: load_fn!(handle, "vaDestroySurfaces"),
                    va_create_config: load_fn!(handle, "vaCreateConfig"),
                    va_destroy_config: load_fn!(handle, "vaDestroyConfig"),
                    va_create_context: load_fn!(handle, "vaCreateContext"),
                    va_destroy_context: load_fn!(handle, "vaDestroyContext"),
                    va_create_buffer: load_fn!(handle, "vaCreateBuffer"),
                    va_destroy_buffer: load_fn!(handle, "vaDestroyBuffer"),
                    va_map_buffer: load_fn!(handle, "vaMapBuffer"),
                    va_unmap_buffer: load_fn!(handle, "vaUnmapBuffer"),
                    va_begin_picture: load_fn!(handle, "vaBeginPicture"),
                    va_render_picture: load_fn!(handle, "vaRenderPicture"),
                    va_end_picture: load_fn!(handle, "vaEndPicture"),
                    va_sync_surface: load_fn!(handle, "vaSyncSurface"),
                    va_error_str: load_fn!(handle, "vaErrorStr"),
                })
            }
        }
    }

    /// Return a human-readable description of a `VAStatus` code.
    pub fn error_string(&self, status: VAStatus) -> String {
        if status == VA_STATUS_SUCCESS {
            return "success".to_string();
        }
        let ptr = unsafe { (self.va_error_str)(status) };
        if ptr.is_null() {
            return format!("unknown VA error {}", status);
        }
        let cstr = unsafe { std::ffi::CStr::from_ptr(ptr) };
        cstr.to_string_lossy().into_owned()
    }
}

// We never call dlclose — the library stays loaded for the process lifetime.
// The handle is Send because VAAPI is thread-safe at the display level.
unsafe impl Send for VaapiLib {}
unsafe impl Sync for VaapiLib {}

// ---------------------------------------------------------------------------
// Singleton accessor
// ---------------------------------------------------------------------------

static VAAPI_INIT: Once = Once::new();
static mut VAAPI_LIB: Option<VaapiLib> = None;

/// Return a reference to the process-wide `VaapiLib`, loading it on first
/// call.  Returns `None` if `libva` is not available.
pub fn vaapi_lib() -> Option<&'static VaapiLib> {
    VAAPI_INIT.call_once(|| {
        // SAFETY: guarded by Once — only one thread executes this.
        unsafe {
            VAAPI_LIB = VaapiLib::load();
        }
    });
    // SAFETY: after call_once the value is immutable.
    unsafe { VAAPI_LIB.as_ref() }
}

// ---------------------------------------------------------------------------
// DMA-BUF import
// ---------------------------------------------------------------------------

/// Import a DMA-BUF frame into a VAAPI surface (zero-copy).
///
/// This creates a VA surface backed by the DMA-BUF fd from `dmabuf`.
/// The GPU can then read this surface directly for encoding without any
/// CPU-side copies.
///
/// # Arguments
///
/// * `lib`     - Loaded VAAPI function pointers.
/// * `display` - An initialized `VADisplay` (from `vaGetDisplayDRM`).
/// * `dmabuf`  - The DMA-BUF frame to import.
///
/// # Errors
///
/// Returns an error string if `vaCreateSurfaces` fails.
///
/// # Safety
///
/// The caller must ensure:
/// - `display` was obtained from a valid, initialized VAAPI display.
/// - `dmabuf.fd` is a valid DMA-BUF file descriptor.
/// - The returned `VASurfaceID` is destroyed (via `vaDestroySurfaces`)
///   before the DMA-BUF fd is closed.
pub unsafe fn import_dmabuf_to_surface(
    lib: &VaapiLib,
    display: VADisplay,
    dmabuf: &DmaBufFrame,
) -> Result<VASurfaceID, String> {
    if display.is_null() {
        return Err("VADisplay is null".to_string());
    }
    if dmabuf.fd < 0 {
        return Err(format!("invalid DMA-BUF fd: {}", dmabuf.fd));
    }

    // Choose VA RT format based on the DRM fourcc.
    let rt_format = drm_format_to_va_rt(dmabuf.format)
        .ok_or_else(|| format!("unsupported DRM format: 0x{:08x}", dmabuf.format))?;
    let va_fourcc = drm_format_to_va_fourcc(dmabuf.format)
        .ok_or_else(|| format!("no VA fourcc for DRM format: 0x{:08x}", dmabuf.format))?;

    // For NV12 single-fd: plane 0 = Y, plane 1 = UV interleaved.
    // UV plane offset = stride * height (immediately after Y plane).
    let uv_offset = dmabuf.stride * dmabuf.height;
    let uv_stride = dmabuf.stride; // same stride for NV12

    let mut fd_as_usize = dmabuf.fd as usize;

    let mut ext_buf = VASurfaceAttribExternalBuffers {
        pixel_format: va_fourcc,
        width: dmabuf.width,
        height: dmabuf.height,
        data_size: dmabuf.stride * dmabuf.height * 3 / 2, // NV12: W*H*1.5
        num_planes: 2,
        pitches: [dmabuf.stride, uv_stride, 0, 0],
        offsets: [dmabuf.offset, dmabuf.offset + uv_offset, 0, 0],
        buffers: &mut fd_as_usize as *mut usize,
        num_buffers: 1,
        flags: 0,
        private_data: std::ptr::null_mut(),
    };

    // Build the three surface attributes:
    // 1. Pixel format
    // 2. Memory type = DRM_PRIME
    // 3. External buffer descriptor
    let mut attribs = [
        VASurfaceAttrib {
            type_: VA_SURFACE_ATTRIB_PIXEL_FORMAT,
            flags: VA_SURFACE_ATTRIB_FLAG_SET,
            value: VAGenericValue::int(va_fourcc as c_int),
        },
        VASurfaceAttrib {
            type_: VA_SURFACE_ATTRIB_MEM_TYPE,
            flags: VA_SURFACE_ATTRIB_FLAG_SET,
            value: VAGenericValue::int(VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME as c_int),
        },
        VASurfaceAttrib {
            type_: VA_SURFACE_ATTRIB_EXTERNAL_BUFFER_DESCRIPTOR,
            flags: VA_SURFACE_ATTRIB_FLAG_SET,
            value: VAGenericValue::pointer(
                &mut ext_buf as *mut VASurfaceAttribExternalBuffers as *mut c_void,
            ),
        },
    ];

    let mut surface_id: VASurfaceID = 0;
    let status = (lib.va_create_surfaces)(
        display,
        rt_format,
        dmabuf.width,
        dmabuf.height,
        &mut surface_id,
        1,
        attribs.as_mut_ptr(),
        attribs.len() as c_uint,
    );

    if status != VA_STATUS_SUCCESS {
        return Err(format!(
            "vaCreateSurfaces failed: {} (status={})",
            lib.error_string(status),
            status,
        ));
    }

    Ok(surface_id)
}

// ---------------------------------------------------------------------------
// DRM format helpers
// ---------------------------------------------------------------------------

/// DRM fourcc for ARGB8888 / XRGB8888.
/// Note: in DRM, ARGB8888 and XRGB8888 have distinct fourccs (AR24 vs XR24),
/// but VAAPI treats both as VA_RT_FORMAT_RGB32.  We use the ARGB variant here.
pub const DRM_FORMAT_ARGB8888: u32 = 0x3441_5258; // 'XR24' little-endian
/// DRM fourcc for P010 (10-bit 4:2:0).
pub const DRM_FORMAT_P010: u32 = 0x3031_3050;

/// Map a DRM fourcc to the corresponding `VA_RT_FORMAT_*` constant.
pub fn drm_format_to_va_rt(drm_fourcc: u32) -> Option<c_uint> {
    match drm_fourcc {
        DmaBufFrame::DRM_FORMAT_NV12 => Some(VA_RT_FORMAT_YUV420),
        DRM_FORMAT_P010 => Some(VA_RT_FORMAT_YUV420_10),
        DRM_FORMAT_ARGB8888 => Some(VA_RT_FORMAT_RGB32),
        _ => None,
    }
}

/// Map a DRM fourcc to the corresponding `VA_FOURCC_*` constant.
pub fn drm_format_to_va_fourcc(drm_fourcc: u32) -> Option<c_uint> {
    match drm_fourcc {
        DmaBufFrame::DRM_FORMAT_NV12 => Some(VA_FOURCC_NV12),
        DRM_FORMAT_P010 => Some(VA_FOURCC_P010),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem;

    // -- constant value tests -----------------------------------------------

    #[test]
    fn va_status_success_is_zero() {
        assert_eq!(VA_STATUS_SUCCESS, 0);
    }

    #[test]
    fn va_rt_format_values() {
        assert_eq!(VA_RT_FORMAT_YUV420, 0x0000_0001);
        assert_eq!(VA_RT_FORMAT_YUV420_10, 0x0000_0100);
        assert_eq!(VA_RT_FORMAT_YUV422, 0x0000_0002);
        assert_eq!(VA_RT_FORMAT_RGB32, 0x0000_0011);
    }

    #[test]
    fn va_fourcc_values() {
        assert_eq!(VA_FOURCC_NV12, 0x3231_564E);
        assert_eq!(VA_FOURCC_P010, 0x3031_3050);
    }

    #[test]
    fn va_profile_values() {
        assert_eq!(VA_PROFILE_H264_MAIN, 6);
        assert_eq!(VA_PROFILE_H264_HIGH, 7);
        assert_eq!(VA_PROFILE_HEVC_MAIN, 12);
        assert_eq!(VA_PROFILE_HEVC_MAIN10, 13);
    }

    #[test]
    fn va_entrypoint_values() {
        assert_eq!(VA_ENTRYPOINT_ENCSLICE, 6);
        assert_eq!(VA_ENTRYPOINT_ENCSLICE_LP, 8);
    }

    #[test]
    fn va_surface_attrib_mem_type_drm_prime() {
        assert_eq!(VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME, 0x2000_0000);
    }

    #[test]
    fn drm_format_nv12_matches_dmabuf_frame() {
        assert_eq!(DmaBufFrame::DRM_FORMAT_NV12, VA_FOURCC_NV12);
    }

    // -- struct layout tests ------------------------------------------------

    #[test]
    fn va_generic_value_size() {
        // Must be 16 bytes on 64-bit (4 + 4 + 8) to match the C union layout.
        #[cfg(target_pointer_width = "64")]
        assert_eq!(mem::size_of::<VAGenericValue>(), 16);
        #[cfg(target_pointer_width = "32")]
        assert_eq!(mem::size_of::<VAGenericValue>(), 12);
    }

    #[test]
    fn va_surface_attrib_size() {
        // type_(4) + flags(4) + VAGenericValue(16) = 24 on 64-bit
        #[cfg(target_pointer_width = "64")]
        assert_eq!(mem::size_of::<VASurfaceAttrib>(), 24);
    }

    #[test]
    fn va_generic_value_int_round_trip() {
        let v = VAGenericValue::int(42);
        assert_eq!(v.type_, 1);
        assert_eq!(v.value as c_int, 42);
    }

    #[test]
    fn va_generic_value_pointer_round_trip() {
        let mut data: u32 = 0xDEAD_BEEF;
        let ptr = &mut data as *mut u32 as *mut c_void;
        let v = VAGenericValue::pointer(ptr);
        assert_eq!(v.type_, 3);
        assert_eq!(v.value, ptr as usize);
    }

    #[test]
    fn external_buffers_has_expected_fields() {
        // Verify the struct can be constructed and fields are accessible.
        let mut fd: usize = 42;
        let ext = VASurfaceAttribExternalBuffers {
            pixel_format: VA_FOURCC_NV12,
            width: 1920,
            height: 1080,
            data_size: 1920 * 1080 * 3 / 2,
            num_planes: 2,
            pitches: [1920, 1920, 0, 0],
            offsets: [0, 1920 * 1080, 0, 0],
            buffers: &mut fd,
            num_buffers: 1,
            flags: 0,
            private_data: std::ptr::null_mut(),
        };
        assert_eq!(ext.pixel_format, VA_FOURCC_NV12);
        assert_eq!(ext.width, 1920);
        assert_eq!(ext.height, 1080);
        assert_eq!(ext.num_planes, 2);
        assert_eq!(ext.pitches[0], 1920);
        assert_eq!(ext.offsets[1], 1920 * 1080);
        assert_eq!(unsafe { *ext.buffers }, 42);
    }

    // -- DRM format mapping tests -------------------------------------------

    #[test]
    fn drm_format_to_va_rt_nv12() {
        assert_eq!(
            drm_format_to_va_rt(DmaBufFrame::DRM_FORMAT_NV12),
            Some(VA_RT_FORMAT_YUV420)
        );
    }

    #[test]
    fn drm_format_to_va_rt_p010() {
        assert_eq!(
            drm_format_to_va_rt(DRM_FORMAT_P010),
            Some(VA_RT_FORMAT_YUV420_10)
        );
    }

    #[test]
    fn drm_format_to_va_rt_unknown() {
        assert_eq!(drm_format_to_va_rt(0xFFFF_FFFF), None);
    }

    #[test]
    fn drm_format_to_va_fourcc_nv12() {
        assert_eq!(
            drm_format_to_va_fourcc(DmaBufFrame::DRM_FORMAT_NV12),
            Some(VA_FOURCC_NV12)
        );
    }

    #[test]
    fn drm_format_to_va_fourcc_p010() {
        assert_eq!(
            drm_format_to_va_fourcc(DRM_FORMAT_P010),
            Some(VA_FOURCC_P010)
        );
    }

    #[test]
    fn drm_format_to_va_fourcc_unknown() {
        assert_eq!(drm_format_to_va_fourcc(0xFFFF_FFFF), None);
    }

    // -- dlopen detection test ----------------------------------------------

    #[test]
    fn vaapi_lib_load_does_not_panic() {
        // On systems without libva this returns None; on systems with it,
        // returns Some.  Either way it must not panic or crash.
        let result = VaapiLib::load();
        if result.is_some() {
            // Verify we can call error_string without crashing.
            let lib = result.as_ref().unwrap();
            let msg = lib.error_string(VA_STATUS_SUCCESS);
            assert!(!msg.is_empty());
        }
    }

    #[test]
    fn vaapi_lib_singleton_consistent() {
        let a = vaapi_lib();
        let b = vaapi_lib();
        // Both must be Some or both None — same pointer.
        assert_eq!(a.is_some(), b.is_some());
    }

    // -- import_dmabuf_to_surface validation tests --------------------------

    #[test]
    fn import_rejects_null_display() {
        if let Some(lib) = vaapi_lib() {
            let dmabuf = DmaBufFrame {
                fd: 0,
                width: 1920,
                height: 1080,
                stride: 1920,
                offset: 0,
                format: DmaBufFrame::DRM_FORMAT_NV12,
                modifier: DmaBufFrame::DRM_FORMAT_MOD_LINEAR,
            };
            let result = unsafe {
                import_dmabuf_to_surface(lib, std::ptr::null_mut(), &dmabuf)
            };
            assert!(result.is_err());
            assert!(result.unwrap_err().contains("null"));
        }
    }

    #[test]
    fn import_rejects_negative_fd() {
        if let Some(lib) = vaapi_lib() {
            let dmabuf = DmaBufFrame {
                fd: -1,
                width: 1920,
                height: 1080,
                stride: 1920,
                offset: 0,
                format: DmaBufFrame::DRM_FORMAT_NV12,
                modifier: DmaBufFrame::DRM_FORMAT_MOD_LINEAR,
            };
            // Use a non-null but invalid display — the function should reject
            // the fd before calling into VAAPI.
            let fake_display = 0x1234_usize as *mut c_void;
            let result = unsafe {
                import_dmabuf_to_surface(lib, fake_display, &dmabuf)
            };
            assert!(result.is_err());
            assert!(result.unwrap_err().contains("invalid DMA-BUF fd"));
        }
    }

    #[test]
    fn import_rejects_unsupported_format() {
        if let Some(lib) = vaapi_lib() {
            let dmabuf = DmaBufFrame {
                fd: 10,
                width: 1920,
                height: 1080,
                stride: 1920,
                offset: 0,
                format: 0xDEAD_BEEF, // unsupported format
                modifier: DmaBufFrame::DRM_FORMAT_MOD_LINEAR,
            };
            let fake_display = 0x1234_usize as *mut c_void;
            let result = unsafe {
                import_dmabuf_to_surface(lib, fake_display, &dmabuf)
            };
            assert!(result.is_err());
            assert!(result.unwrap_err().contains("unsupported DRM format"));
        }
    }
}

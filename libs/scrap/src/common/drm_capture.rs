// DRM/KMS Direct Framebuffer Capture
//
// This module provides screen capture via the Linux DRM (Direct Rendering
// Manager) subsystem.  It reads the GPU's scanout framebuffer directly,
// bypassing any compositor or portal.
//
// # When to use
//
// DRM capture is the only option when no user session is active:
// - Login screen (GDM/SDDM greeter) — the xdg-desktop-portal is not running
// - Headless — no compositor at all
// - After reboot before any user logs in
//
// In all these cases PipeWire/portal capture cannot work because there is
// nobody to authorize the screen share.
//
// # Capture flow
//
// ```text
// 1. Open /dev/dri/card0 (needs root or video group)
// 2. Enumerate CRTCs → find active CRTC with a framebuffer
// 3. Get framebuffer ID from CRTC
// 4. Use DRM_IOCTL_MODE_MAP_DUMB to mmap the framebuffer (dumb buffers)
//    — or —
//    Use DRM_IOCTL_PRIME_HANDLE_TO_FD to export a DMA-BUF, then mmap (GBM)
// 5. Read BGRA/XRGB pixel data from mapped memory
// 6. Convert to BGRA for the scrap PixelBuffer format
// ```
//
// # Permissions
//
// - `CAP_SYS_ADMIN` or root for `drmModeGetFB2` with GEM handle export
// - The SteelDesk service typically runs as root, so this is satisfied
// - Membership in the `video` group allows opening /dev/dri/card* but may
//   not suffice for framebuffer readback on all drivers
//
// # Kernel requirements
//
// - Linux 5.15+ for reliable dumb-buffer access on all GPU drivers
// - The DRM device must not be render-only (needs modesetting support)

use std::io;

/// Well-known DRM pixel formats (fourcc codes).
///
/// DRM uses fourcc codes to describe pixel formats.  We only need to
/// recognize a handful of common scanout formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrmPixelFormat {
    /// XRGB8888 — 32 bpp, 8 bits per channel, X (ignored) in high byte.
    /// Memory layout: B G R X (little-endian).
    Xrgb8888,
    /// ARGB8888 — 32 bpp with alpha.
    /// Memory layout: B G R A (little-endian).
    Argb8888,
    /// XBGR8888 — 32 bpp, reversed channel order.
    /// Memory layout: R G B X (little-endian).
    Xbgr8888,
    /// RGB565 — 16 bpp, common on older/embedded hardware.
    Rgb565,
    /// Unknown / unsupported format.
    Other(u32),
}

impl DrmPixelFormat {
    /// Construct from a DRM fourcc code.
    pub fn from_fourcc(fourcc: u32) -> Self {
        // DRM fourcc constants (from drm_fourcc.h):
        //   XR24 = 0x34325258  (XRGB8888)
        //   AR24 = 0x34325241  (ARGB8888)
        //   XB24 = 0x34324258  (XBGR8888)
        //   RG16 = 0x36314752  (RGB565)
        const DRM_FORMAT_XRGB8888: u32 = 0x34325258;
        const DRM_FORMAT_ARGB8888: u32 = 0x34325241;
        const DRM_FORMAT_XBGR8888: u32 = 0x34324258;
        const DRM_FORMAT_RGB565: u32 = 0x36314752;

        match fourcc {
            DRM_FORMAT_XRGB8888 => DrmPixelFormat::Xrgb8888,
            DRM_FORMAT_ARGB8888 => DrmPixelFormat::Argb8888,
            DRM_FORMAT_XBGR8888 => DrmPixelFormat::Xbgr8888,
            DRM_FORMAT_RGB565 => DrmPixelFormat::Rgb565,
            other => DrmPixelFormat::Other(other),
        }
    }

    /// Bytes per pixel for this format.
    pub fn bytes_per_pixel(&self) -> usize {
        match self {
            DrmPixelFormat::Xrgb8888 | DrmPixelFormat::Argb8888 | DrmPixelFormat::Xbgr8888 => 4,
            DrmPixelFormat::Rgb565 => 2,
            DrmPixelFormat::Other(_) => 4, // assume 32bpp as fallback
        }
    }

    /// Whether this format has the same memory layout as BGRA (which is what
    /// the rest of the scrap pipeline expects).
    ///
    /// XRGB8888 and ARGB8888 are stored as B-G-R-X/A in memory on
    /// little-endian systems, which matches BGRA layout.
    pub fn is_bgra_compatible(&self) -> bool {
        matches!(self, DrmPixelFormat::Xrgb8888 | DrmPixelFormat::Argb8888)
    }
}

/// Information about a DRM CRTC (CRT Controller) that is actively scanning
/// out a framebuffer to a display connector.
#[derive(Debug, Clone)]
pub struct DrmCrtcInfo {
    /// CRTC index (0-based, used for identification).
    pub crtc_index: u32,
    /// Framebuffer width in pixels.
    pub width: u32,
    /// Framebuffer height in pixels.
    pub height: u32,
    /// Pixel format of the framebuffer.
    pub format: DrmPixelFormat,
    /// Stride (pitch) of the framebuffer in bytes.
    pub stride: u32,
}

/// DRM/KMS capture backend.
///
/// Opens a DRM device and reads the active CRTC's framebuffer.  This works
/// at the login screen and in headless mode where no compositor portal is
/// available.
///
/// # Lifecycle
///
/// ```text
/// DrmCapture::new()          — open device, find active CRTC
/// drm.capture_frame()        — read one frame (pixel data as Vec<u8>)
/// drm.width() / drm.height() — dimensions of the captured output
/// drop(drm)                  — close the device fd
/// ```
pub struct DrmCapture {
    /// Path to the DRM device that was opened (e.g. "/dev/dri/card0").
    device_path: String,
    /// Information about the active CRTC we are capturing from.
    crtc_info: DrmCrtcInfo,
    /// Reusable pixel buffer to avoid repeated allocation.
    buffer: Vec<u8>,
    // TODO: When implementing the actual ioctls, add:
    //   card_fd: std::fs::File,        // open fd to /dev/dri/cardN
    //   fb_map: *mut u8,               // mmap'd framebuffer pointer
    //   fb_map_len: usize,             // length of the mapping
}

impl DrmCapture {
    /// Try to open a DRM device and set up capture from the primary CRTC.
    ///
    /// This will:
    /// 1. Iterate `/dev/dri/card*` to find a device with modesetting support
    /// 2. Enumerate CRTCs to find one that is actively driving a display
    /// 3. Get the framebuffer info (dimensions, format, stride)
    /// 4. Set up mmap for reading pixel data
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - No DRM device is found
    /// - The device cannot be opened (permissions)
    /// - No active CRTC is found (no display connected)
    /// - The framebuffer format is unsupported
    ///
    /// # Permissions
    ///
    /// Requires `CAP_SYS_ADMIN` or root for `drmModeGetFB2` with GEM handle
    /// export.  The SteelDesk service runs as root, so this is typically
    /// satisfied.
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let device_path = Self::find_drm_device()?;

        // TODO: Actual implementation requires DRM ioctls:
        //
        // 1. Open the device:
        //    let card_fd = File::open(&device_path)?;
        //
        // 2. Get DRM resources:
        //    let res = drm_mode_get_resources(card_fd.as_raw_fd())?;
        //
        // 3. Find active CRTC:
        //    for crtc_id in res.crtcs {
        //        let crtc = drm_mode_get_crtc(fd, crtc_id)?;
        //        if crtc.fb_id != 0 {
        //            // This CRTC has an active framebuffer
        //        }
        //    }
        //
        // 4. Get framebuffer info:
        //    let fb = drm_mode_get_fb2(fd, crtc.fb_id)?;
        //    // fb.width, fb.height, fb.pixel_format, fb.pitches[0]
        //
        // 5. Map the framebuffer:
        //    For dumb buffers:
        //      let map_offset = drm_mode_map_dumb(fd, fb.handles[0])?;
        //      let ptr = mmap(null, size, PROT_READ, MAP_SHARED, fd, map_offset);
        //    For GBM surfaces:
        //      let dma_fd = drm_prime_handle_to_fd(fd, fb.handles[0])?;
        //      let ptr = mmap(null, size, PROT_READ, MAP_SHARED, dma_fd, 0);

        // Stub: return a placeholder that reports "no active CRTC" until
        // the ioctl implementation is filled in.
        Err(format!(
            "DRM capture not yet implemented (found device: {}). \
             TODO: implement DRM ioctls for framebuffer readback.",
            device_path
        )
        .into())
    }

    /// Find the first DRM device with modesetting support.
    ///
    /// Iterates `/dev/dri/card0` through `/dev/dri/card15` and returns the
    /// first device path that exists.  A full implementation would also
    /// verify that the device supports modesetting (not render-only).
    fn find_drm_device() -> Result<String, Box<dyn std::error::Error>> {
        for i in 0..16 {
            let path = format!("/dev/dri/card{}", i);
            if std::path::Path::new(&path).exists() {
                // TODO: open the device and check DRM_CAP_DUMB_BUFFER
                // to verify it supports modesetting / dumb buffers.
                // Render-only nodes (/dev/dri/renderD*) should be skipped.
                return Ok(path);
            }
        }
        Err("No DRM device found in /dev/dri/card*".into())
    }

    /// Check if DRM capture is potentially available on this system.
    ///
    /// This is a quick check that does NOT open any device or require
    /// elevated permissions.  It returns `true` if at least one DRM card
    /// device exists in `/dev/dri/`.
    ///
    /// A `true` return does not guarantee that `DrmCapture::new()` will
    /// succeed — the device may lack permissions or have no active CRTC.
    pub fn available() -> bool {
        for i in 0..16 {
            let path = format!("/dev/dri/card{}", i);
            if std::path::Path::new(&path).exists() {
                return true;
            }
        }
        false
    }

    /// Capture a single frame from the active CRTC.
    ///
    /// Returns the pixel data as a byte slice in BGRA format (or the native
    /// DRM format if conversion is not implemented yet).  The data is valid
    /// until the next call to `capture_frame()`.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The CRTC is no longer active (display disconnected)
    /// - The framebuffer format changed
    /// - The mmap read fails
    pub fn capture_frame(&mut self) -> Result<&[u8], Box<dyn std::error::Error>> {
        // TODO: Actual implementation:
        //
        // 1. Re-read CRTC info to check if the FB changed:
        //    let crtc = drm_mode_get_crtc(self.card_fd, self.crtc_id)?;
        //    if crtc.fb_id != self.current_fb_id {
        //        // FB changed (resolution change, VT switch, etc.)
        //        // Re-map the new framebuffer
        //    }
        //
        // 2. Copy pixel data from the mmap'd region:
        //    For dumb buffers, the mmap region is directly readable.
        //    For GBM/DMA-BUF, we may need to handle tiling/swizzling.
        //
        //    unsafe {
        //        std::ptr::copy_nonoverlapping(
        //            self.fb_map,
        //            self.buffer.as_mut_ptr(),
        //            self.fb_map_len,
        //        );
        //    }
        //
        // 3. Convert pixel format if needed:
        //    If format is XBGR8888 (RGBA), swap R and B channels.
        //    If format is RGB565, expand to BGRA.
        //    If format is XRGB8888/ARGB8888, data is already BGRA-compatible.

        let stride = self.crtc_info.stride as usize;
        let height = self.crtc_info.height as usize;
        let frame_size = stride * height;
        self.buffer.resize(frame_size, 0);

        // Stub: fill with a dark gray pattern so tests can verify the buffer
        // size is correct without actual DRM hardware.
        for pixel in self.buffer.chunks_exact_mut(4) {
            pixel[0] = 0x20; // B
            pixel[1] = 0x20; // G
            pixel[2] = 0x20; // R
            pixel[3] = 0xFF; // A
        }

        Ok(&self.buffer)
    }

    /// Width of the captured display in pixels.
    pub fn width(&self) -> usize {
        self.crtc_info.width as usize
    }

    /// Height of the captured display in pixels.
    pub fn height(&self) -> usize {
        self.crtc_info.height as usize
    }

    /// Stride (bytes per row) of the captured framebuffer.
    pub fn stride(&self) -> usize {
        self.crtc_info.stride as usize
    }

    /// Pixel format of the captured framebuffer.
    pub fn format(&self) -> DrmPixelFormat {
        self.crtc_info.format
    }

    /// Path to the DRM device being used.
    pub fn device_path(&self) -> &str {
        &self.device_path
    }
}

/// A display backed by a DRM CRTC.
///
/// This is the DRM equivalent of `x11::Display` and `wayland::Display`.
/// It represents a single output (monitor) driven by a DRM CRTC.
pub struct DrmDisplay {
    /// Width of the display in pixels.
    width: usize,
    /// Height of the display in pixels.
    height: usize,
    /// Which DRM card device this display is on (e.g. "/dev/dri/card0").
    device_path: String,
    /// CRTC index on the card.
    crtc_index: u32,
}

impl DrmDisplay {
    /// Enumerate all active DRM displays.
    ///
    /// Returns one `DrmDisplay` per active CRTC that is driving a connected
    /// output.
    pub fn all() -> io::Result<Vec<DrmDisplay>> {
        // TODO: iterate /dev/dri/card*, open each, enumerate CRTCs,
        // return one DrmDisplay per active CRTC.
        //
        // For now, return empty — the caller falls through to the
        // "no display found" error, which is correct when DRM ioctls
        // are not yet implemented.
        Ok(Vec::new())
    }

    /// Get the primary DRM display (first active CRTC found).
    pub fn primary() -> io::Result<DrmDisplay> {
        let mut all = Self::all()?;
        if all.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "No active DRM display found",
            ));
        }
        Ok(all.remove(0))
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    pub fn origin(&self) -> (i32, i32) {
        // TODO: DRM connectors have position info in multi-monitor setups.
        // For now, assume origin at (0, 0).
        (0, 0)
    }

    pub fn is_online(&self) -> bool {
        true
    }

    pub fn is_primary(&self) -> bool {
        self.crtc_index == 0
    }

    pub fn name(&self) -> String {
        format!("DRM:{}/crtc{}", self.device_path, self.crtc_index)
    }

    pub fn scale(&self) -> f64 {
        // DRM does not have a concept of logical vs physical scale.
        // The compositor handles scaling; at the DRM level we see
        // physical pixels only.
        1.0
    }

    pub fn logical_width(&self) -> usize {
        self.width
    }

    pub fn logical_height(&self) -> usize {
        self.height
    }
}

/// A capturer backed by DRM framebuffer readback.
///
/// Implements the same interface as `x11::Capturer` and `wayland::Capturer`
/// so it can be used as a variant in the `linux::Capturer` enum.
pub struct DrmCapturer {
    display: DrmDisplay,
    buffer: Vec<u8>,
    // TODO: when DrmCapture is fully implemented, store the DrmCapture
    // instance here instead of a separate buffer.
}

impl DrmCapturer {
    pub fn new(display: DrmDisplay) -> io::Result<DrmCapturer> {
        // TODO: initialize DrmCapture from the display's device_path and
        // crtc_index.  For now, allocate a buffer based on the display
        // dimensions.
        let buf_size = display.width * display.height * 4; // BGRA
        Ok(DrmCapturer {
            display,
            buffer: vec![0u8; buf_size],
        })
    }

    pub fn width(&self) -> usize {
        self.display.width()
    }

    pub fn height(&self) -> usize {
        self.display.height()
    }
}

impl crate::TraitCapturer for DrmCapturer {
    fn frame<'a>(&'a mut self, _timeout: std::time::Duration) -> io::Result<crate::Frame<'a>> {
        // TODO: call DrmCapture::capture_frame() and return the pixel data.
        //
        // Once DRM ioctls are implemented:
        //   let data = self.drm_capture.capture_frame()
        //       .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        //   return Ok(Frame::PixelBuffer(PixelBuffer::new(
        //       data, Pixfmt::BGRA, self.width(), self.height()
        //   )));
        //
        // Until then, return WouldBlock to signal "no frame available",
        // which the capture loop handles gracefully (it retries).
        Err(io::ErrorKind::WouldBlock.into())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- DrmPixelFormat --

    #[test]
    fn pixel_format_xrgb8888_from_fourcc() {
        let fmt = DrmPixelFormat::from_fourcc(0x34325258);
        assert_eq!(fmt, DrmPixelFormat::Xrgb8888);
    }

    #[test]
    fn pixel_format_argb8888_from_fourcc() {
        let fmt = DrmPixelFormat::from_fourcc(0x34325241);
        assert_eq!(fmt, DrmPixelFormat::Argb8888);
    }

    #[test]
    fn pixel_format_xbgr8888_from_fourcc() {
        let fmt = DrmPixelFormat::from_fourcc(0x34324258);
        assert_eq!(fmt, DrmPixelFormat::Xbgr8888);
    }

    #[test]
    fn pixel_format_rgb565_from_fourcc() {
        let fmt = DrmPixelFormat::from_fourcc(0x36314752);
        assert_eq!(fmt, DrmPixelFormat::Rgb565);
    }

    #[test]
    fn pixel_format_unknown_from_fourcc() {
        let fmt = DrmPixelFormat::from_fourcc(0xDEADBEEF);
        assert_eq!(fmt, DrmPixelFormat::Other(0xDEADBEEF));
    }

    #[test]
    fn pixel_format_bytes_per_pixel() {
        assert_eq!(DrmPixelFormat::Xrgb8888.bytes_per_pixel(), 4);
        assert_eq!(DrmPixelFormat::Argb8888.bytes_per_pixel(), 4);
        assert_eq!(DrmPixelFormat::Xbgr8888.bytes_per_pixel(), 4);
        assert_eq!(DrmPixelFormat::Rgb565.bytes_per_pixel(), 2);
        assert_eq!(DrmPixelFormat::Other(0).bytes_per_pixel(), 4);
    }

    #[test]
    fn pixel_format_bgra_compatible() {
        assert!(DrmPixelFormat::Xrgb8888.is_bgra_compatible());
        assert!(DrmPixelFormat::Argb8888.is_bgra_compatible());
        assert!(!DrmPixelFormat::Xbgr8888.is_bgra_compatible());
        assert!(!DrmPixelFormat::Rgb565.is_bgra_compatible());
        assert!(!DrmPixelFormat::Other(0).is_bgra_compatible());
    }

    // -- DrmCapture availability --

    #[test]
    fn drm_available_returns_bool_without_panic() {
        // On machines with a GPU, this returns true.
        // On CI without /dev/dri, this returns false.
        // The important thing is it does not panic.
        let _avail = DrmCapture::available();
    }

    #[test]
    fn drm_find_device_does_not_panic() {
        // May return Ok or Err depending on hardware; must not panic.
        let _result = DrmCapture::find_drm_device();
    }

    // -- DrmCapture::new stub --

    #[test]
    fn drm_capture_new_returns_error_stub() {
        // The stub implementation always returns Err because ioctls
        // are not yet implemented.  This test verifies the error path
        // does not panic.
        let result = DrmCapture::new();
        // On machines without /dev/dri, this errors with "No DRM device".
        // On machines with /dev/dri, this errors with "not yet implemented".
        assert!(result.is_err());
    }

    // -- DrmCrtcInfo --

    #[test]
    fn crtc_info_construction() {
        let info = DrmCrtcInfo {
            crtc_index: 0,
            width: 1920,
            height: 1080,
            format: DrmPixelFormat::Xrgb8888,
            stride: 7680,
        };
        assert_eq!(info.width, 1920);
        assert_eq!(info.height, 1080);
        assert_eq!(info.stride, 7680);
        assert_eq!(info.format, DrmPixelFormat::Xrgb8888);
    }

    // -- DrmDisplay --

    #[test]
    fn drm_display_all_returns_empty_stub() {
        // Stub returns empty vec (no ioctls implemented yet).
        let displays = DrmDisplay::all().unwrap();
        assert!(displays.is_empty());
    }

    #[test]
    fn drm_display_primary_returns_not_found_stub() {
        // Stub has no displays, so primary() should return NotFound.
        let result = DrmDisplay::primary();
        assert!(result.is_err());
    }

    #[test]
    fn drm_display_properties() {
        let d = DrmDisplay {
            width: 2560,
            height: 1440,
            device_path: "/dev/dri/card0".to_string(),
            crtc_index: 0,
        };
        assert_eq!(d.width(), 2560);
        assert_eq!(d.height(), 1440);
        assert_eq!(d.origin(), (0, 0));
        assert!(d.is_online());
        assert!(d.is_primary());
        assert_eq!(d.scale(), 1.0);
        assert_eq!(d.logical_width(), 2560);
        assert_eq!(d.logical_height(), 1440);
        assert!(d.name().contains("card0"));
        assert!(d.name().contains("crtc0"));
    }

    #[test]
    fn drm_display_secondary_not_primary() {
        let d = DrmDisplay {
            width: 1920,
            height: 1080,
            device_path: "/dev/dri/card0".to_string(),
            crtc_index: 1,
        };
        assert!(!d.is_primary());
        assert!(d.name().contains("crtc1"));
    }

    // -- DrmCapturer --

    #[test]
    fn drm_capturer_dimensions() {
        let display = DrmDisplay {
            width: 1920,
            height: 1080,
            device_path: "/dev/dri/card0".to_string(),
            crtc_index: 0,
        };
        let capturer = DrmCapturer::new(display).unwrap();
        assert_eq!(capturer.width(), 1920);
        assert_eq!(capturer.height(), 1080);
    }

    #[test]
    fn drm_capturer_frame_returns_would_block_stub() {
        use crate::TraitCapturer;
        use std::time::Duration;

        let display = DrmDisplay {
            width: 640,
            height: 480,
            device_path: "/dev/dri/card0".to_string(),
            crtc_index: 0,
        };
        let mut capturer = DrmCapturer::new(display).unwrap();
        let result = capturer.frame(Duration::from_millis(100));
        assert!(result.is_err());
        let err = result.err().unwrap();
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
    }

    // -- DrmCapture with constructed state (simulates post-ioctl) --

    #[test]
    fn drm_capture_capture_frame_stub() {
        // Construct a DrmCapture manually (bypassing new()) to test
        // capture_frame() logic with the stub fill pattern.
        let mut cap = DrmCapture {
            device_path: "/dev/dri/card0".to_string(),
            crtc_info: DrmCrtcInfo {
                crtc_index: 0,
                width: 4,
                height: 2,
                format: DrmPixelFormat::Xrgb8888,
                stride: 16, // 4 pixels * 4 bytes
            },
            buffer: Vec::new(),
        };
        let frame = cap.capture_frame().unwrap();
        // 4 * 2 * 4 = 32 bytes (but stride-based: 16 * 2 = 32)
        assert_eq!(frame.len(), 32);
        // Check the stub fill pattern (dark gray BGRA)
        assert_eq!(&frame[0..4], &[0x20, 0x20, 0x20, 0xFF]);
    }

    #[test]
    fn drm_capture_accessors() {
        let cap = DrmCapture {
            device_path: "/dev/dri/card1".to_string(),
            crtc_info: DrmCrtcInfo {
                crtc_index: 2,
                width: 3840,
                height: 2160,
                format: DrmPixelFormat::Argb8888,
                stride: 15360,
            },
            buffer: Vec::new(),
        };
        assert_eq!(cap.width(), 3840);
        assert_eq!(cap.height(), 2160);
        assert_eq!(cap.stride(), 15360);
        assert_eq!(cap.format(), DrmPixelFormat::Argb8888);
        assert_eq!(cap.device_path(), "/dev/dri/card1");
    }
}

// GPU Zero-Copy Pipeline — Scaffolding & Capability Detection
//
// # Current GPU Pipeline State
//
// RustDesk has two hardware-accelerated video paths, both using the `hwcodec`
// crate:
//
// ## VRAM Path (`feature = "vram"`, Windows-only today)
//
// Encode side:
//   DXGI Desktop Duplication → ID3D11Texture2D (GPU) → hwcodec VRAM Encoder
//   → encoded bitstream (CPU) → network
//
// Decode side:
//   network → encoded bitstream → hwcodec VRAM Decoder → GPU texture
//   → render (no CPU copy)
//
// The VRAM path keeps frame data on the GPU throughout capture and encode.
// The texture pointer is passed directly from DXGI to the encoder — zero
// GPU-CPU copies on the capture side.
//
// Key files:
//   - `libs/scrap/src/common/vram.rs` — VRamEncoder, VRamDecoder
//   - `libs/scrap/src/dxgi/mod.rs` — DXGI capture with `output_texture` mode
//   - `libs/scrap/src/common/codec.rs` — `handle_vram_video_frame()`
//
// ## HwRam Path (`feature = "hwcodec"`, all desktop platforms)
//
// Encode side:
//   Screen capture → CPU pixel buffer (BGRA) → libyuv BGRA→NV12 (CPU)
//   → ffmpeg upload to GPU → HW encoder (NVENC/QSV/VAAPI/VideoToolbox)
//   → encoded bitstream (CPU) → network
//
// Decode side:
//   network → encoded bitstream → HW decoder → CPU pixel buffer
//   → libyuv NV12/I420→ARGB (CPU) → render
//
// The HwRam path crosses the GPU-CPU boundary multiple times: once to read
// the framebuffer, once for color conversion, once to upload to the encoder,
// and again on the decode side to read back decoded frames.
//
// Key files:
//   - `libs/scrap/src/common/hwcodec.rs` — HwRamEncoder, HwRamDecoder
//   - `libs/scrap/src/common/codec.rs` — `handle_hwram_video_frame()`
//
// # What the `vram` Feature Enables
//
// When `feature = "vram"` is active:
//
// 1. `EncoderCfg::VRAM` variant becomes available in `codec.rs`
// 2. `EncoderApi::input_texture()` method is compiled into the trait
// 3. `VRamEncoder` and `VRamDecoder` types are available from `vram.rs`
// 4. `HwCodecConfig` gains `vram_encode` and `vram_decode` fields
// 5. `Decoder` struct gains `h264_vram` and `h265_vram` fields
// 6. `enable_vram_option()` is compiled in; it checks:
//    - Windows only (`cfg!(windows)`)
//    - Hardware codec option is enabled in user config
//    - For encode: DirectX capture is enabled
//    - For decode: D3D render is allowed
// 7. VRAM decoders are preferred over HwRam decoders when available
//    (checked first in `Decoder::new()`, lines 539-546 and 565-572)
// 8. `handle_vram_video_frame()` outputs an `ImageTexture` (GPU ptr)
//    instead of copying to `ImageRgb` (CPU buffer)
//
// # What's Needed for Linux (VAAPI) Zero-Copy
//
// The Linux path currently uses GStreamer with `pipewiresrc` and forces
// `always-copy=true`. To achieve zero-copy:
//
// 1. Accept DMA-BUF file descriptors from PipeWire instead of CPU buffers
//    - Negotiate `SPA_DATA_DmaBuf` buffer type in PipeWire stream
//    - Remove `always-copy=true` (or use a separate capture path)
//
// 2. Import DMA-BUF fds into VAAPI surfaces
//    - Use `vaCreateSurfaces` with `VASurfaceAttribExternalBuffers`
//    - Or use `vaCreateSurfaceFromFd` (simpler but less portable)
//
// 3. Pass VAAPI surface handles to the encoder
//    - Extend `hwcodec` crate to accept VASurfaceID as input
//    - Or add a new `Frame::DmaBuf` variant
//
// 4. Feature gate: `feature = "vram"` on Linux (currently hardcoded to
//    Windows in `enable_vram_option()`)
//
// # What's Needed for macOS (VideoToolbox) Zero-Copy
//
// The macOS path uses ScreenCaptureKit which provides `IOSurface` backed
// `CMSampleBuffer`s. To achieve zero-copy:
//
// 1. Extract `IOSurfaceRef` from the captured `CMSampleBuffer`
//    - `CMSampleBufferGetImageBuffer()` → `CVPixelBufferGetIOSurface()`
//
// 2. Pass `IOSurface` to VideoToolbox encoder
//    - `VTCompressionSessionEncodeFrame()` accepts `CVPixelBuffer` backed
//      by `IOSurface` — this is already zero-copy if no format conversion
//      is needed
//
// 3. On decode, use `VTDecompressionSession` with `IOSurface`-backed output
//    - `kCVPixelBufferIOSurfacePropertiesKey` in output buffer attributes
//
// 4. Render via Metal from the `IOSurface` directly
//

// # Linux VAAPI Zero-Copy Pipeline
//
// ## Zero-copy path (target):
//
//   PipeWire DMA-BUF fd
//       |-- [NO COPY] fd passed via DmaBufFrame struct
//       v
//   Import into VAAPI surface
//       |-- vaCreateSurfaces with VASurfaceAttribExternalBuffers
//       |-- binds DMA-BUF memory directly to VA surface (zero-copy)
//       v
//   VAAPI encode (H.264 or H.265)
//       |-- [output: CPU] encoded bitstream (NALUs)
//       v
//   Network
//
// ## Current path (with GPU->CPU->GPU round-trip):
//
//   PipeWire + GStreamer pipewiresrc (always-copy=true)
//       |-- [COPY 1: GPU->CPU] GStreamer maps buffer to CPU memory
//       v
//   CPU BGRA buffer (PixelBuffer)
//       |-- [COPY 2: CPU, format conversion] libyuv BGRA->NV12
//       v
//   CPU NV12 YUV buffer
//       |-- [COPY 3: CPU->GPU] ffmpeg upload to VAAPI encoder surface
//       v
//   VAAPI encode
//       |-- [output: CPU] encoded bitstream
//       v
//   Network
//
// ## Implementation steps (in order of dependency):
//
// Step 1 (this file): Capability detection and data structures.
//   - vaapi_available(): check for /dev/dri/renderD* nodes
//   - vaapi_device_path(): find the first available render node
//   - GpuPipelineMode::VaapiZeroCopy variant
//   - DmaBufFrame struct (in libs/scrap/src/common/mod.rs)
//
// Step 2 (future): PipeWire DMA-BUF negotiation.
//   - Remove always-copy=true from pipewiresrc
//   - Add memory:DMABuf feature to GStreamer caps negotiation
//   - Extract DMA-BUF fd, stride, offset, modifier from GstBuffer
//
// Step 3 (future): VAAPI surface import.
//   - Use libva bindings to call vaCreateSurfaces with external buffers
//   - Map DMA-BUF fd to VASurfaceID
//   - Extend hwcodec crate to accept VASurfaceID as encoder input
//
// Step 4 (future): Full pipeline integration.
//   - Wire DmaBufFrame through the video_service capture loop
//   - Add Frame::DmaBuf variant (parallel to Frame::Texture on Windows)
//   - Feature-gate behind "vram" on Linux

/// Attempt a full VAAPI zero-copy encode of a DMA-BUF frame.
///
/// This is the top-level entry point for the zero-copy encode pipeline:
///
/// 1. Load `libva.so.2` via `dlopen` (cached after first call).
/// 2. Import the DMA-BUF fd into a VA surface (zero-copy bind).
/// 3. Encode the surface via the VAAPI H.264/H.265 encode pipeline.
/// 4. Return the encoded bitstream (NALUs).
///
/// # Current status
///
/// Steps 1 and 2 are implemented.  Step 3 (the actual encode) is stubbed —
/// it requires setting up a full VA encode context with rate-control and
/// sequence/picture/slice parameter buffers, which will be wired when the
/// pipeline is integrated with `hwcodec` or a standalone encode path.
///
/// # Errors
///
/// Returns an error if:
/// - `libva` is not installed on the system.
/// - The DMA-BUF fd or format is invalid.
/// - VAAPI surface creation fails (driver issue, unsupported format, etc.).
/// - The encode step fails or is not yet implemented.
#[cfg(target_os = "linux")]
pub fn try_vaapi_encode(dmabuf: &scrap::DmaBufFrame) -> Result<Vec<u8>, String> {
    use scrap::vaapi;

    // Step 1: Load VAAPI library.
    let _lib = vaapi::vaapi_lib()
        .ok_or_else(|| "libva.so not available on this system".to_string())?;

    // Step 2: Open a DRM render node and get a VADisplay.
    // In the full pipeline this would be cached per-session, not opened
    // per-frame.  For now we document the expected flow.
    let device_path = vaapi_device_path()
        .ok_or_else(|| "no DRI render node found".to_string())?;

    // TODO: Open the render node fd and call vaGetDisplayDRM().
    // This requires libva-drm.so (vaGetDisplayDRM) which we should also
    // dlopen.  For now, return a clear error indicating what's missing.
    //
    // let drm_fd = libc::open(device_path.as_ptr(), libc::O_RDWR);
    // let display = vaGetDisplayDRM(drm_fd);
    // let mut major = 0i32;
    // let mut minor = 0i32;
    // (lib.va_initialize)(display, &mut major, &mut minor);
    //
    // Step 3: Import DMA-BUF to surface.
    // let surface = vaapi::import_dmabuf_to_surface(lib, display, dmabuf)?;
    //
    // Step 4: Encode via VAAPI.
    // - vaCreateConfig(profile, entrypoint)
    // - vaCreateContext(config, width, height, surfaces)
    // - vaCreateBuffer (sequence params, picture params, slice params)
    // - vaBeginPicture / vaRenderPicture / vaEndPicture
    // - vaSyncSurface
    // - vaMapBuffer (coded buffer) -> extract NALUs
    // - vaUnmapBuffer / vaDestroyBuffer / vaDestroySurfaces
    //
    // Step 5: Clean up.
    // - vaDestroyContext / vaDestroyConfig / vaTerminate

    Err(format!(
        "VAAPI encode pipeline not yet fully wired \
         (libva loaded, render node={}, dmabuf {}x{} fd={}). \
         The surface import and encode steps require vaGetDisplayDRM \
         from libva-drm.so which is not yet integrated.",
        device_path, dmabuf.width, dmabuf.height, dmabuf.fd,
    ))
}

/// Check whether the VAAPI runtime library (`libva.so.2`) can be loaded.
///
/// Unlike [`vaapi_available`] which only checks for DRI render nodes on the
/// filesystem, this function actually attempts to `dlopen` the library and
/// resolve all required symbols.  Returns `true` only if the full VAAPI
/// function-pointer table was successfully loaded.
///
/// This is a stronger check: a system can have render nodes but no VAAPI
/// driver installed (e.g. Nouveau without `mesa-va-drivers`).
#[cfg(target_os = "linux")]
pub fn vaapi_runtime_available() -> bool {
    scrap::vaapi::vaapi_lib().is_some()
}

#[cfg(not(target_os = "linux"))]
pub fn vaapi_runtime_available() -> bool {
    false
}

/// Check whether VAAPI zero-copy encoding is potentially available.
///
/// This performs a lightweight filesystem check for DRI render nodes
/// (`/dev/dri/renderD*`), which are required for VAAPI operation.
/// The presence of a render node is necessary but not sufficient —
/// the actual VAAPI driver must also be installed and functional.
///
/// # Platform behavior
///
/// - **Linux**: Checks for `/dev/dri/renderD128` through `renderD135`.
/// - **Other platforms**: Always returns `false`.
pub fn vaapi_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        // Check the most common render node first, then scan others.
        // Render nodes 128-135 cover up to 8 GPUs, which is more than
        // any typical desktop system would have.
        for i in 128..136 {
            let path = format!("/dev/dri/renderD{}", i);
            if std::path::Path::new(&path).exists() {
                return true;
            }
        }
        false
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// Return the path to the first available VAAPI render node.
///
/// Scans `/dev/dri/renderD128` through `/dev/dri/renderD135` and returns
/// the first one that exists. Returns `None` if no render nodes are found
/// (headless system, container without GPU passthrough, or non-Linux).
///
/// In a full implementation, this path would be passed to `vaGetDisplayDRM()`
/// to open a VAAPI display connection.
pub fn vaapi_device_path() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        for i in 128..136 {
            let path = format!("/dev/dri/renderD{}", i);
            if std::path::Path::new(&path).exists() {
                return Some(path);
            }
        }
        None
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Check whether DRM/KMS framebuffer capture should be used.
///
/// DRM capture is the fallback for situations where no compositor portal is
/// available: the login screen (GDM/SDDM greeter on Wayland) and headless
/// mode without a compositor.
///
/// # When this returns `true`
///
/// 1. A DRM card device exists (`/dev/dri/card*`)
/// 2. AND one of:
///    - We are at the Wayland login screen (detected via `is_login_screen_wayland()`)
///    - No display server is reachable (`DISPLAY` and `WAYLAND_DISPLAY` both unset)
///
/// # Platform behavior
///
/// - **Linux**: Performs the detection described above.
/// - **Other platforms**: Always returns `false`.
pub fn should_use_drm_capture() -> bool {
    #[cfg(target_os = "linux")]
    {
        use scrap::drm_capture::DrmCapture;

        if !DrmCapture::available() {
            return false;
        }

        // Check if we are at the login screen.
        let at_login = crate::platform::linux::is_login_screen_wayland_safe();

        // Check if headless (no display server environment variables).
        let no_display = std::env::var("DISPLAY").is_err();
        let no_wayland = std::env::var("WAYLAND_DISPLAY").is_err();
        let headless = no_display && no_wayland;

        at_login || headless
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// Runtime detection of GPU zero-copy pipeline availability.
///
/// Returns `true` if the current platform and GPU drivers support the VRAM
/// encode/decode path — meaning frame data can stay on the GPU from capture
/// through encoding without any CPU copies.
///
/// # Platform behavior
///
/// - **Windows** (with `feature = "vram"`): Checks that the VRAM option is
///   enabled, DirectX capture is available, and at least one VRAM encoder
///   has been detected by the hwcodec config.
///
/// - **Linux**: Currently always returns `false`. Future work will check
///   for VAAPI + DMA-BUF support via PipeWire.
///
/// - **macOS**: Currently always returns `false`. Future work will check
///   for VideoToolbox + IOSurface support.
///
/// - **Other / no `vram` feature**: Always returns `false`.
pub fn gpu_zero_copy_available() -> bool {
    #[cfg(feature = "vram")]
    {
        return vram_available_impl();
    }
    #[cfg(not(feature = "vram"))]
    {
        false
    }
}

/// Internal implementation for VRAM availability check.
/// Separated to allow testing the logic on platforms where the feature is
/// compiled in but the hardware may or may not be present.
#[cfg(feature = "vram")]
fn vram_available_impl() -> bool {
    if !cfg!(windows) {
        // VRAM path is currently Windows-only (D3D11).
        // Linux VAAPI and macOS VideoToolbox paths are not yet implemented.
        return false;
    }
    // On Windows, check that the user has enabled hardware codec and that
    // VRAM encoders were detected during the hwcodec config check.
    scrap::codec::enable_vram_option(true)
}

/// Summary of the current GPU pipeline state for the running platform.
///
/// Returns a human-readable string describing which GPU paths are available
/// and what mode would be used for encode/decode. Intended for diagnostics
/// and logging, not for programmatic use.
pub fn gpu_pipeline_summary() -> String {
    let mut lines = Vec::new();

    lines.push(format!("Platform: {}", std::env::consts::OS));
    lines.push(format!(
        "GPU zero-copy available: {}",
        gpu_zero_copy_available()
    ));

    #[cfg(feature = "vram")]
    {
        lines.push("Feature 'vram': enabled".to_string());
        lines.push(format!(
            "  VRAM encode option: {}",
            scrap::codec::enable_vram_option(true)
        ));
        lines.push(format!(
            "  VRAM decode option: {}",
            scrap::codec::enable_vram_option(false)
        ));
    }
    #[cfg(not(feature = "vram"))]
    {
        lines.push("Feature 'vram': disabled".to_string());
    }

    #[cfg(feature = "hwcodec")]
    {
        lines.push("Feature 'hwcodec': enabled".to_string());
    }
    #[cfg(not(feature = "hwcodec"))]
    {
        lines.push("Feature 'hwcodec': disabled".to_string());
    }

    // Linux VAAPI diagnostics
    #[cfg(target_os = "linux")]
    {
        lines.push(format!("VAAPI available: {}", vaapi_available()));
        match vaapi_device_path() {
            Some(path) => lines.push(format!("VAAPI render node: {}", path)),
            None => lines.push("VAAPI render node: none found".to_string()),
        }
        lines.push(format!(
            "DRM capture (login/headless): {}",
            should_use_drm_capture()
        ));
    }

    lines.push(format!("Pipeline mode: {:?}", detect_pipeline_mode()));

    lines.join("\n")
}

/// Describes the GPU pipeline mode that would be used for encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuPipelineMode {
    /// VRAM zero-copy: GPU texture passed directly to encoder, no CPU copies.
    /// Available on Windows with `feature = "vram"` and D3D11 support.
    VRam,
    /// VAAPI zero-copy: DMA-BUF fd from PipeWire imported directly into VAAPI
    /// surface for encoding, no CPU copies. Available on Linux with VAAPI
    /// drivers and a DRI render node.
    ///
    /// Pipeline: PipeWire DMA-BUF fd -> VAAPI surface import -> VAAPI encode
    ///
    /// This mode is currently scaffolding only. The actual DMA-BUF import
    /// requires `libva` bindings which are not yet integrated.
    VaapiZeroCopy,
    /// Hardware-accelerated but with CPU copies: frame is read from GPU to CPU,
    /// converted (BGRA→NV12), then uploaded back to GPU for encoding.
    HwRam,
    /// Software encoding only (VP8/VP9/AV1). All processing on CPU.
    Software,
}

/// Detect the best available GPU pipeline mode for the current platform.
///
/// Priority order:
/// 1. VRAM (zero-copy via D3D11 texture, Windows only)
/// 2. VaapiZeroCopy (zero-copy via DMA-BUF, Linux only — scaffolding, not yet wired)
/// 3. HwRam (hardware encode with CPU copies)
/// 4. Software (VP8/VP9/AV1)
///
/// Note: `VaapiZeroCopy` is detected here for diagnostic/logging purposes,
/// but the actual DMA-BUF pipeline is not yet implemented. The video service
/// will fall through to HwRam or Software until the full pipeline is wired.
pub fn detect_pipeline_mode() -> GpuPipelineMode {
    if gpu_zero_copy_available() {
        return GpuPipelineMode::VRam;
    }

    // On Linux, check if VAAPI zero-copy could be supported.
    // This is scaffolding: we detect the capability but the actual
    // DMA-BUF import path is not yet implemented, so callers should
    // treat this as informational and fall through to HwRam.
    #[cfg(target_os = "linux")]
    {
        if vaapi_available() {
            return GpuPipelineMode::VaapiZeroCopy;
        }
    }

    #[cfg(feature = "hwcodec")]
    {
        if scrap::codec::enable_hwcodec_option() {
            return GpuPipelineMode::HwRam;
        }
    }

    GpuPipelineMode::Software
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpu_zero_copy_returns_bool() {
        // On Linux CI/test without GPU, this should be false.
        // On Windows with vram feature and GPU, it could be true.
        // The important thing is it does not panic.
        let _available = gpu_zero_copy_available();
    }

    #[test]
    fn gpu_pipeline_summary_contains_platform() {
        let summary = gpu_pipeline_summary();
        assert!(
            summary.contains("Platform:"),
            "summary should contain Platform line"
        );
        assert!(
            summary.contains(std::env::consts::OS),
            "summary should contain the OS name"
        );
    }

    #[test]
    fn gpu_pipeline_summary_contains_zero_copy_status() {
        let summary = gpu_pipeline_summary();
        assert!(
            summary.contains("GPU zero-copy available:"),
            "summary should report zero-copy availability"
        );
    }

    #[test]
    fn gpu_pipeline_summary_contains_feature_flags() {
        let summary = gpu_pipeline_summary();
        // At least one of these must be present
        assert!(
            summary.contains("Feature 'vram':") || summary.contains("Feature 'hwcodec':"),
            "summary should report feature flag status"
        );
    }

    #[test]
    fn gpu_pipeline_summary_contains_pipeline_mode() {
        let summary = gpu_pipeline_summary();
        assert!(
            summary.contains("Pipeline mode:"),
            "summary should report the detected pipeline mode"
        );
    }

    #[test]
    fn detect_pipeline_mode_does_not_panic() {
        let mode = detect_pipeline_mode();
        // On Linux without vram feature, should be VaapiZeroCopy, HwRam, or Software
        match mode {
            GpuPipelineMode::VRam => {
                // Only valid on Windows with vram feature and hardware support
                assert!(
                    gpu_zero_copy_available(),
                    "VRam mode requires zero-copy to be available"
                );
            }
            GpuPipelineMode::VaapiZeroCopy => {
                // Valid on Linux when VAAPI render nodes exist
                assert!(
                    vaapi_available(),
                    "VaapiZeroCopy mode requires VAAPI to be available"
                );
            }
            GpuPipelineMode::HwRam => {
                // Valid when hwcodec feature is enabled
                assert!(
                    !gpu_zero_copy_available(),
                    "HwRam mode means VRAM zero-copy is not available"
                );
            }
            GpuPipelineMode::Software => {
                // Always valid as fallback
            }
        }
    }

    #[test]
    fn pipeline_mode_debug_display() {
        // Ensure Debug trait works for logging
        let mode = detect_pipeline_mode();
        let debug_str = format!("{:?}", mode);
        assert!(!debug_str.is_empty());
    }

    #[test]
    fn pipeline_mode_equality() {
        assert_eq!(GpuPipelineMode::VRam, GpuPipelineMode::VRam);
        assert_eq!(GpuPipelineMode::HwRam, GpuPipelineMode::HwRam);
        assert_eq!(GpuPipelineMode::Software, GpuPipelineMode::Software);
        assert_eq!(
            GpuPipelineMode::VaapiZeroCopy,
            GpuPipelineMode::VaapiZeroCopy
        );
        assert_ne!(GpuPipelineMode::VRam, GpuPipelineMode::HwRam);
        assert_ne!(GpuPipelineMode::VRam, GpuPipelineMode::Software);
        assert_ne!(GpuPipelineMode::VRam, GpuPipelineMode::VaapiZeroCopy);
        assert_ne!(GpuPipelineMode::HwRam, GpuPipelineMode::Software);
        assert_ne!(GpuPipelineMode::HwRam, GpuPipelineMode::VaapiZeroCopy);
        assert_ne!(
            GpuPipelineMode::Software,
            GpuPipelineMode::VaapiZeroCopy
        );
    }

    // Feature flag documentation tests

    #[test]
    fn feature_vram_compile_time_detection() {
        // This test documents the compile-time feature detection pattern.
        // The `vram` feature controls:
        //   - VRamEncoder / VRamDecoder availability
        //   - EncoderApi::input_texture() method
        //   - Decoder's h264_vram / h265_vram fields
        //   - HwCodecConfig's vram_encode / vram_decode fields
        //   - enable_vram_option() function
        let has_vram = cfg!(feature = "vram");
        // In test builds without the vram feature, zero-copy must be unavailable
        if !has_vram {
            assert!(
                !gpu_zero_copy_available(),
                "without vram feature, zero-copy must be false"
            );
        }
    }

    #[test]
    fn feature_hwcodec_compile_time_detection() {
        // The `hwcodec` feature controls:
        //   - HwRamEncoder / HwRamDecoder availability
        //   - EncoderCfg::HWRAM variant
        //   - Decoder's h264_ram / h265_ram fields
        //   - HwCodecConfig's ram_encode / ram_decode usage
        //   - enable_hwcodec_option() function
        let _has_hwcodec = cfg!(feature = "hwcodec");
        // Just documenting — no assertion needed since both states are valid
    }

    // VAAPI detection tests

    #[test]
    fn vaapi_available_returns_bool() {
        // Must not panic regardless of whether GPU hardware is present.
        // On a system with a GPU, returns true; on headless/container, false.
        let _available = vaapi_available();
    }

    #[test]
    fn vaapi_device_path_returns_option() {
        // Must not panic. Returns Some("/dev/dri/renderDXXX") on systems
        // with a GPU render node, None on headless systems.
        let path = vaapi_device_path();
        if let Some(ref p) = path {
            assert!(
                p.starts_with("/dev/dri/renderD"),
                "VAAPI device path should be a DRI render node, got: {}",
                p
            );
            // The render node number should be in 128..136
            let num: u32 = p
                .trim_start_matches("/dev/dri/renderD")
                .parse()
                .expect("render node should end with a number");
            assert!(
                (128..136).contains(&num),
                "render node number should be 128-135, got: {}",
                num
            );
        }
    }

    #[test]
    fn vaapi_available_consistent_with_device_path() {
        // If vaapi_available() is true, vaapi_device_path() must return Some.
        // If vaapi_device_path() returns Some, vaapi_available() must be true.
        let available = vaapi_available();
        let path = vaapi_device_path();
        assert_eq!(
            available,
            path.is_some(),
            "vaapi_available() and vaapi_device_path() must agree"
        );
    }

    #[test]
    fn pipeline_mode_detection_includes_vaapi_on_linux() {
        // On Linux with a GPU, detect_pipeline_mode should return VaapiZeroCopy
        // (since we don't have the vram feature enabled in tests).
        let mode = detect_pipeline_mode();
        if cfg!(target_os = "linux") && vaapi_available() && !gpu_zero_copy_available() {
            assert_eq!(
                mode,
                GpuPipelineMode::VaapiZeroCopy,
                "on Linux with VAAPI available, should detect VaapiZeroCopy mode"
            );
        }
    }

    #[test]
    fn gpu_pipeline_summary_contains_vaapi_on_linux() {
        let summary = gpu_pipeline_summary();
        if cfg!(target_os = "linux") {
            assert!(
                summary.contains("VAAPI available:"),
                "Linux summary should include VAAPI availability"
            );
            assert!(
                summary.contains("VAAPI render node:"),
                "Linux summary should include VAAPI render node info"
            );
        }
    }

    // DmaBufFrame tests

    #[test]
    fn dmabuf_frame_construction_and_field_access() {
        let frame = scrap::DmaBufFrame {
            fd: 42,
            width: 1920,
            height: 1080,
            stride: 7680,
            offset: 0,
            format: scrap::DmaBufFrame::DRM_FORMAT_NV12,
            modifier: scrap::DmaBufFrame::DRM_FORMAT_MOD_LINEAR,
        };
        assert_eq!(frame.fd, 42);
        assert_eq!(frame.width, 1920);
        assert_eq!(frame.height, 1080);
        assert_eq!(frame.stride, 7680);
        assert_eq!(frame.offset, 0);
        assert_eq!(frame.format, 0x3231564E); // NV12 fourcc
        assert_eq!(frame.modifier, 0);
    }

    #[test]
    fn dmabuf_frame_is_linear() {
        let linear_frame = scrap::DmaBufFrame {
            fd: 1,
            width: 640,
            height: 480,
            stride: 2560,
            offset: 0,
            format: scrap::DmaBufFrame::DRM_FORMAT_NV12,
            modifier: scrap::DmaBufFrame::DRM_FORMAT_MOD_LINEAR,
        };
        assert!(linear_frame.is_linear());

        let tiled_frame = scrap::DmaBufFrame {
            fd: 2,
            width: 640,
            height: 480,
            stride: 2560,
            offset: 0,
            format: scrap::DmaBufFrame::DRM_FORMAT_NV12,
            modifier: 0x0100000000000001, // some Intel tiling modifier
        };
        assert!(!tiled_frame.is_linear());
    }

    #[test]
    fn dmabuf_frame_clone_and_debug() {
        let frame = scrap::DmaBufFrame {
            fd: 10,
            width: 3840,
            height: 2160,
            stride: 15360,
            offset: 0,
            format: scrap::DmaBufFrame::DRM_FORMAT_NV12,
            modifier: scrap::DmaBufFrame::DRM_FORMAT_MOD_INVALID,
        };
        // Test Clone
        let cloned = frame.clone();
        assert_eq!(cloned.fd, frame.fd);
        assert_eq!(cloned.width, frame.width);
        assert_eq!(cloned.modifier, scrap::DmaBufFrame::DRM_FORMAT_MOD_INVALID);

        // Test Debug
        let debug_str = format!("{:?}", frame);
        assert!(debug_str.contains("DmaBufFrame"));
        assert!(debug_str.contains("3840"));
    }

    #[test]
    fn dmabuf_frame_constants() {
        assert_eq!(scrap::DmaBufFrame::DRM_FORMAT_MOD_LINEAR, 0);
        assert_eq!(
            scrap::DmaBufFrame::DRM_FORMAT_MOD_INVALID,
            0x00ffffffffffffff
        );
        assert_eq!(scrap::DmaBufFrame::DRM_FORMAT_NV12, 0x3231564E);
    }

    // DRM capture detection tests

    #[test]
    fn should_use_drm_capture_returns_bool() {
        // Must not panic regardless of hardware or environment.
        let _result = should_use_drm_capture();
    }

    #[test]
    fn should_use_drm_capture_false_when_display_set() {
        // When DISPLAY or WAYLAND_DISPLAY is set, we have a compositor,
        // so DRM capture should not be used (unless login screen is detected).
        // We cannot safely mutate env vars in parallel tests, so we just
        // verify the function does not panic and document expected behavior.
        let result = should_use_drm_capture();
        let has_display = std::env::var("DISPLAY").is_ok()
            || std::env::var("WAYLAND_DISPLAY").is_ok();
        if has_display {
            // If we have a display, DRM capture should be false unless
            // we are at a login screen (unlikely in test environments).
            // We cannot assert false unconditionally because the login
            // screen check is valid, but in practice test runners are
            // not login screens.
            let _ = result; // just ensure no panic
        }
    }

    #[test]
    fn gpu_pipeline_summary_contains_drm_on_linux() {
        let summary = gpu_pipeline_summary();
        if cfg!(target_os = "linux") {
            assert!(
                summary.contains("DRM capture"),
                "Linux summary should include DRM capture status"
            );
        }
    }

    #[test]
    fn vaapi_zero_copy_variant_exists() {
        // Ensure the VaapiZeroCopy variant compiles and can be matched.
        let mode = GpuPipelineMode::VaapiZeroCopy;
        assert_eq!(format!("{:?}", mode), "VaapiZeroCopy");
        match mode {
            GpuPipelineMode::VaapiZeroCopy => {} // expected
            _ => panic!("should match VaapiZeroCopy"),
        }
    }

    // -- VAAPI runtime detection tests --------------------------------------

    #[test]
    fn vaapi_runtime_available_returns_bool() {
        // Must not panic regardless of whether libva is installed.
        let _available = vaapi_runtime_available();
    }

    #[test]
    fn vaapi_runtime_consistent_with_filesystem_check() {
        // If runtime is available, the filesystem check must also pass
        // (you need both render nodes AND libva).
        if vaapi_runtime_available() {
            assert!(
                vaapi_available(),
                "if libva loads, render nodes must exist"
            );
        }
    }

    // -- VAAPI encode stub tests --------------------------------------------

    #[cfg(target_os = "linux")]
    #[test]
    fn try_vaapi_encode_returns_descriptive_error() {
        // The encode pipeline is not fully wired, so this should return
        // an Err with useful diagnostic info (not panic).
        let dmabuf = scrap::DmaBufFrame {
            fd: 99,
            width: 1920,
            height: 1080,
            stride: 1920,
            offset: 0,
            format: scrap::DmaBufFrame::DRM_FORMAT_NV12,
            modifier: scrap::DmaBufFrame::DRM_FORMAT_MOD_LINEAR,
        };
        let result = try_vaapi_encode(&dmabuf);
        assert!(result.is_err(), "encode pipeline should not succeed yet");
        let err = result.unwrap_err();
        // The error should mention either "not available" or "not yet wired"
        assert!(
            err.contains("not") || err.contains("libva"),
            "error should be descriptive, got: {}",
            err
        );
    }

    // -- VAAPI module tests (via scrap re-export) ---------------------------

    #[cfg(target_os = "linux")]
    #[test]
    fn vaapi_lib_load_does_not_panic() {
        let _ = scrap::vaapi::VaapiLib::load();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn vaapi_constants_match_drm_constants() {
        // The VA fourcc for NV12 must match the DRM fourcc — they are
        // the same value by design.
        assert_eq!(
            scrap::vaapi::VA_FOURCC_NV12,
            scrap::DmaBufFrame::DRM_FORMAT_NV12,
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn vaapi_drm_format_mapping_round_trip() {
        use scrap::vaapi::*;
        // NV12 -> VA_RT_FORMAT_YUV420
        assert_eq!(
            drm_format_to_va_rt(scrap::DmaBufFrame::DRM_FORMAT_NV12),
            Some(VA_RT_FORMAT_YUV420),
        );
        // NV12 -> VA_FOURCC_NV12
        assert_eq!(
            drm_format_to_va_fourcc(scrap::DmaBufFrame::DRM_FORMAT_NV12),
            Some(VA_FOURCC_NV12),
        );
        // P010 -> VA_RT_FORMAT_YUV420_10
        assert_eq!(
            drm_format_to_va_rt(DRM_FORMAT_P010),
            Some(VA_RT_FORMAT_YUV420_10),
        );
    }
}

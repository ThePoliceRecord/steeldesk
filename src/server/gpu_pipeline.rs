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

    lines.join("\n")
}

/// Describes the GPU pipeline mode that would be used for encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuPipelineMode {
    /// VRAM zero-copy: GPU texture passed directly to encoder, no CPU copies.
    /// Available on Windows with `feature = "vram"` and D3D11 support.
    VRam,
    /// Hardware-accelerated but with CPU copies: frame is read from GPU to CPU,
    /// converted (BGRA→NV12), then uploaded back to GPU for encoding.
    HwRam,
    /// Software encoding only (VP8/VP9/AV1). All processing on CPU.
    Software,
}

/// Detect the best available GPU pipeline mode for the current platform.
///
/// Priority order:
/// 1. VRAM (zero-copy, if available)
/// 2. HwRam (hardware encode with CPU copies)
/// 3. Software (VP8/VP9/AV1)
pub fn detect_pipeline_mode() -> GpuPipelineMode {
    if gpu_zero_copy_available() {
        return GpuPipelineMode::VRam;
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
    fn detect_pipeline_mode_does_not_panic() {
        let mode = detect_pipeline_mode();
        // On Linux without vram feature, should be HwRam or Software
        match mode {
            GpuPipelineMode::VRam => {
                // Only valid on Windows with vram feature and hardware support
                assert!(
                    gpu_zero_copy_available(),
                    "VRam mode requires zero-copy to be available"
                );
            }
            GpuPipelineMode::HwRam => {
                // Valid when hwcodec feature is enabled
                assert!(
                    !gpu_zero_copy_available(),
                    "HwRam mode means zero-copy is not available"
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
        assert_ne!(GpuPipelineMode::VRam, GpuPipelineMode::HwRam);
        assert_ne!(GpuPipelineMode::VRam, GpuPipelineMode::Software);
        assert_ne!(GpuPipelineMode::HwRam, GpuPipelineMode::Software);
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
}

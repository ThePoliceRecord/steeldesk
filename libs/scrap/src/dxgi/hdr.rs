// Windows DXGI 1.6 HDR Capture Support
//
// This module provides HDR capture via DXGI 1.6's DuplicateOutput1 API,
// which can request 10-bit pixel formats (DXGI_FORMAT_R10G10B10A2_UNORM)
// for HDR10 surfaces. The existing code uses the older DuplicateOutput
// which only produces 8-bit BGRA.
//
// Windows DXGI 1.6 HDR Capture Flow:
//   1. Enumerate outputs via IDXGIAdapter::EnumOutputs
//   2. QueryInterface for IDXGIOutput6
//   3. Call GetDesc1() -> check ColorSpace for BT.2020 PQ
//      (DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020 = 12)
//   4. If HDR: use DuplicateOutput1 with DXGI_FORMAT_R10G10B10A2_UNORM
//   5. If SDR: use DuplicateOutput with DXGI_FORMAT_B8G8R8A8_UNORM (current behavior)
//   6. AcquireNextFrame -> texture in HDR or SDR format
//   7. Tag frame with HdrInfo metadata (bit_depth=10, max_luminance from GetDesc1)
//
// Cross-platform format constants and detection helpers live in
// `crate::common::dxgi_hdr_constants` so they can be tested on any platform.
// This file contains only the Windows COM API wrappers.

use std::io;
use std::ptr;

use winapi::shared::dxgi1_2::{IDXGIOutput1, IDXGIOutputDuplication};
use winapi::shared::dxgi1_5::{IDXGIOutput5, IID_IDXGIOutput5};
use winapi::shared::dxgi1_6::{IDXGIOutput6, IID_IDXGIOutput6, DXGI_OUTPUT_DESC1};
use winapi::shared::dxgiformat::DXGI_FORMAT;
use winapi::shared::minwindef::UINT;
use winapi::shared::winerror::S_OK;
use winapi::um::d3d11::ID3D11Device;
use winapi::um::unknwnbase::IUnknown;

// Re-export the cross-platform constants for convenience.
pub use crate::dxgi_hdr_constants::*;

/// Information about a DXGI output's HDR capabilities, obtained from
/// `IDXGIOutput6::GetDesc1()`.
#[derive(Debug, Clone)]
pub struct DxgiHdrOutputInfo {
    /// Whether the output's color space is BT.2020 PQ (HDR).
    pub hdr_enabled: bool,
    /// The DXGI color space type reported by the output.
    pub color_space: u32,
    /// Peak luminance of the display in nits.
    pub max_luminance: f32,
    /// Minimum luminance of the display in nits.
    pub min_luminance: f32,
    /// Maximum full-frame luminance in nits.
    pub max_full_frame_luminance: f32,
    /// Red primary (x, y) chromaticity coordinates.
    pub red_primary: (f32, f32),
    /// Green primary (x, y) chromaticity coordinates.
    pub green_primary: (f32, f32),
    /// Blue primary (x, y) chromaticity coordinates.
    pub blue_primary: (f32, f32),
    /// White point (x, y) chromaticity coordinates.
    pub white_point: (f32, f32),
}

/// Query HDR information from an IDXGIOutput1 by upgrading it to IDXGIOutput6.
///
/// Returns `Ok(Some(info))` if the output supports DXGI 1.6 (IDXGIOutput6),
/// `Ok(None)` if it does not (older DXGI), or `Err` on failure.
///
/// # Safety
///
/// `output` must be a valid, non-null `IDXGIOutput1` pointer.
pub unsafe fn query_hdr_output_info(
    output: *mut IDXGIOutput1,
) -> io::Result<Option<DxgiHdrOutputInfo>> {
    if output.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "null IDXGIOutput1 pointer",
        ));
    }

    let mut output6: *mut IDXGIOutput6 = ptr::null_mut();
    let hr = (*output).QueryInterface(
        &IID_IDXGIOutput6,
        &mut output6 as *mut *mut _ as *mut *mut _,
    );

    if hr != S_OK || output6.is_null() {
        // DXGI 1.6 not available — cannot determine HDR support via this path.
        return Ok(None);
    }

    // Ensure we release the IDXGIOutput6 reference when done.
    struct Output6Guard(*mut IDXGIOutput6);
    impl Drop for Output6Guard {
        fn drop(&mut self) {
            unsafe {
                (*(self.0 as *mut IUnknown)).Release();
            }
        }
    }
    let _guard = Output6Guard(output6);

    let mut desc1: DXGI_OUTPUT_DESC1 = std::mem::zeroed();
    let hr = (*output6).GetDesc1(&mut desc1);
    if hr != S_OK {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("IDXGIOutput6::GetDesc1 failed: {:#X}", hr),
        ));
    }

    let hdr_enabled =
        desc1.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020_VALUE as _;

    Ok(Some(DxgiHdrOutputInfo {
        hdr_enabled,
        color_space: desc1.ColorSpace as u32,
        max_luminance: desc1.MaxLuminance,
        min_luminance: desc1.MinLuminance,
        max_full_frame_luminance: desc1.MaxFullFrameLuminance,
        red_primary: (desc1.RedPrimary[0], desc1.RedPrimary[1]),
        green_primary: (desc1.GreenPrimary[0], desc1.GreenPrimary[1]),
        blue_primary: (desc1.BluePrimary[0], desc1.BluePrimary[1]),
        white_point: (desc1.WhitePoint[0], desc1.WhitePoint[1]),
    }))
}

/// Attempt to create an HDR-capable desktop duplication via `DuplicateOutput1`.
///
/// `DuplicateOutput1` (DXGI 1.5+) lets us specify the pixel formats we
/// accept. When HDR is active on the display, the API will choose
/// `DXGI_FORMAT_R10G10B10A2_UNORM` if we include it in the list.
///
/// If `DuplicateOutput1` is not available (older Windows), this falls back
/// to the standard `DuplicateOutput` which always gives B8G8R8A8.
///
/// # Safety
///
/// `device` and `output` must be valid, non-null pointers to their
/// respective COM objects.
///
/// # Returns
///
/// On success, returns `(duplication_ptr, is_hdr)` where `is_hdr` is true
/// if the duplication was created with an HDR-capable format list.
pub unsafe fn create_hdr_duplication(
    device: *mut ID3D11Device,
    output: *mut IDXGIOutput1,
) -> io::Result<(*mut IDXGIOutputDuplication, bool)> {
    if device.is_null() || output.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "null device or output pointer",
        ));
    }

    // Try to get IDXGIOutput5 which provides DuplicateOutput1.
    let mut output5: *mut IDXGIOutput5 = ptr::null_mut();
    let hr = (*output).QueryInterface(
        &IID_IDXGIOutput5,
        &mut output5 as *mut *mut _ as *mut *mut _,
    );

    if hr == S_OK && !output5.is_null() {
        struct Output5Guard(*mut IDXGIOutput5);
        impl Drop for Output5Guard {
            fn drop(&mut self) {
                unsafe {
                    (*(self.0 as *mut IUnknown)).Release();
                }
            }
        }
        let _guard = Output5Guard(output5);

        // Request HDR format first, with SDR as fallback.
        let formats: [DXGI_FORMAT; 2] = [
            DXGI_FORMAT_R10G10B10A2_UNORM_VALUE as DXGI_FORMAT,
            DXGI_FORMAT_B8G8R8A8_UNORM_VALUE as DXGI_FORMAT,
        ];

        let mut duplication: *mut IDXGIOutputDuplication = ptr::null_mut();
        let hr = (*output5).DuplicateOutput1(
            device as *mut IUnknown,
            0 as UINT, // flags — reserved, must be 0
            formats.len() as UINT,
            formats.as_ptr(),
            &mut duplication,
        );

        if hr == S_OK && !duplication.is_null() {
            return Ok((duplication, true));
        }
        // DuplicateOutput1 failed — fall through to legacy path.
    }

    // Legacy path: DuplicateOutput (always produces B8G8R8A8_UNORM).
    let mut duplication: *mut IDXGIOutputDuplication = ptr::null_mut();
    let hr = (*output).DuplicateOutput(device as *mut IUnknown, &mut duplication);

    if hr != S_OK || duplication.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("DuplicateOutput failed: {:#X}", hr),
        ));
    }

    Ok((duplication, false))
}

/// Convert `DxgiHdrOutputInfo` into a cross-platform `HdrInfo`.
pub fn to_hdr_info(info: &DxgiHdrOutputInfo) -> crate::HdrInfo {
    if info.hdr_enabled {
        crate::HdrInfo::hdr(10, info.max_luminance as u32)
    } else {
        crate::HdrInfo::sdr()
    }
}

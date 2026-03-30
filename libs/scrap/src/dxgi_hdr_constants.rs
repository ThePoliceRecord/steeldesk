// DXGI HDR format constants and detection helpers.
//
// These are defined independently of the Windows SDK so they can be used
// in cross-platform code (e.g., protocol negotiation, format tagging)
// and tested on any platform.

// ---------------------------------------------------------------------------
// DXGI format constants
// ---------------------------------------------------------------------------

/// `DXGI_FORMAT_R10G10B10A2_UNORM` (value 24) — 10-bit per channel, used for
/// HDR10 surfaces on Windows.
///
/// See: <https://learn.microsoft.com/en-us/windows/win32/api/dxgiformat/ne-dxgiformat-dxgi_format>
pub const DXGI_FORMAT_R10G10B10A2_UNORM_VALUE: u32 = 24;

/// `DXGI_FORMAT_B8G8R8A8_UNORM` (value 87) — standard 8-bit BGRA, the current
/// default capture format on Windows.
pub const DXGI_FORMAT_B8G8R8A8_UNORM_VALUE: u32 = 87;

/// `DXGI_FORMAT_R16G16B16A16_FLOAT` (value 10) — 16-bit float per channel
/// (scRGB). Some Windows HDR desktops use this format instead of R10G10B10A2.
pub const DXGI_FORMAT_R16G16B16A16_FLOAT_VALUE: u32 = 10;

// ---------------------------------------------------------------------------
// DXGI color space constants
// ---------------------------------------------------------------------------

/// `DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020` (value 12) — BT.2020 color
/// primaries with PQ (Perceptual Quantizer / SMPTE ST 2084) transfer function.
///
/// When `IDXGIOutput6::GetDesc1()` reports this color space, the output is in
/// HDR mode.
pub const DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020_VALUE: u32 = 12;

// ---------------------------------------------------------------------------
// Detection helpers
// ---------------------------------------------------------------------------

/// Returns `true` if the given DXGI format value is the 10-bit packed HDR
/// format (`DXGI_FORMAT_R10G10B10A2_UNORM`).
pub fn is_hdr_format(format: u32) -> bool {
    format == DXGI_FORMAT_R10G10B10A2_UNORM_VALUE
}

/// Returns `true` if the given DXGI format value is a wide-channel HDR format
/// (either 10-bit packed or 16-bit float).
pub fn is_wide_color_format(format: u32) -> bool {
    format == DXGI_FORMAT_R10G10B10A2_UNORM_VALUE
        || format == DXGI_FORMAT_R16G16B16A16_FLOAT_VALUE
}

/// Returns the preferred list of DXGI format values to request from
/// `DuplicateOutput1` when HDR capture is desired, in priority order.
///
/// The caller should pass this list to `IDXGIOutput5::DuplicateOutput1`.
/// The API will select the first format it supports. We always include the
/// SDR fallback so the call cannot fail due to format mismatch.
pub fn hdr_preferred_formats() -> Vec<u32> {
    vec![
        DXGI_FORMAT_R10G10B10A2_UNORM_VALUE,
        DXGI_FORMAT_B8G8R8A8_UNORM_VALUE,
    ]
}

/// Returns `true` if the given DXGI color space value indicates HDR
/// (BT.2020 + PQ transfer).
pub fn is_hdr_color_space(color_space: u32) -> bool {
    color_space == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020_VALUE
}

// ---------------------------------------------------------------------------
// Tests — run on all platforms
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dxgi_format_constants() {
        // Values from the Windows SDK dxgiformat.h
        assert_eq!(DXGI_FORMAT_R10G10B10A2_UNORM_VALUE, 24);
        assert_eq!(DXGI_FORMAT_B8G8R8A8_UNORM_VALUE, 87);
        assert_eq!(DXGI_FORMAT_R16G16B16A16_FLOAT_VALUE, 10);
    }

    #[test]
    fn test_color_space_constant() {
        assert_eq!(DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020_VALUE, 12);
    }

    #[test]
    fn test_is_hdr_format() {
        assert!(is_hdr_format(DXGI_FORMAT_R10G10B10A2_UNORM_VALUE));
        assert!(!is_hdr_format(DXGI_FORMAT_B8G8R8A8_UNORM_VALUE));
        assert!(!is_hdr_format(0));
        assert!(!is_hdr_format(DXGI_FORMAT_R16G16B16A16_FLOAT_VALUE));
    }

    #[test]
    fn test_is_wide_color_format() {
        assert!(is_wide_color_format(DXGI_FORMAT_R10G10B10A2_UNORM_VALUE));
        assert!(is_wide_color_format(DXGI_FORMAT_R16G16B16A16_FLOAT_VALUE));
        assert!(!is_wide_color_format(DXGI_FORMAT_B8G8R8A8_UNORM_VALUE));
        assert!(!is_wide_color_format(0));
    }

    #[test]
    fn test_is_hdr_color_space() {
        assert!(is_hdr_color_space(
            DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020_VALUE
        ));
        assert!(!is_hdr_color_space(0)); // DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709 (sRGB)
        assert!(!is_hdr_color_space(1));
        assert!(!is_hdr_color_space(99));
    }

    #[test]
    fn test_hdr_preferred_formats() {
        let formats = hdr_preferred_formats();
        assert_eq!(formats.len(), 2);
        // HDR format should be first (highest priority).
        assert_eq!(formats[0], DXGI_FORMAT_R10G10B10A2_UNORM_VALUE);
        // SDR fallback should be second.
        assert_eq!(formats[1], DXGI_FORMAT_B8G8R8A8_UNORM_VALUE);
    }

    #[test]
    fn test_hdr_preferred_formats_contains_sdr_fallback() {
        let formats = hdr_preferred_formats();
        assert!(
            formats.contains(&DXGI_FORMAT_B8G8R8A8_UNORM_VALUE),
            "must include SDR fallback so DuplicateOutput1 never fails due to unsupported format"
        );
    }

    #[test]
    fn test_format_constants_are_distinct() {
        let vals = [
            DXGI_FORMAT_R10G10B10A2_UNORM_VALUE,
            DXGI_FORMAT_B8G8R8A8_UNORM_VALUE,
            DXGI_FORMAT_R16G16B16A16_FLOAT_VALUE,
        ];
        for i in 0..vals.len() {
            for j in (i + 1)..vals.len() {
                assert_ne!(vals[i], vals[j], "format constants must be unique");
            }
        }
    }
}

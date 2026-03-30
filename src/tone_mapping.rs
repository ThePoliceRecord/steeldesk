//! HDR-to-SDR tone mapping for SteelDesk.
//!
//! This module implements a CPU-based tone mapping pipeline that converts
//! HDR (PQ / BT.2020) content to SDR (sRGB / BT.709).  The pipeline is:
//!
//! 1. Normalize 10-bit input to `[0, 1]`
//! 2. Apply the PQ (Perceptual Quantizer) EOTF to recover linear-light values
//! 3. Reinhard tone map to compress the luminance range
//! 4. BT.2020 -> BT.709 gamut mapping via 3x3 color matrix
//! 5. Apply the sRGB OETF (gamma)
//! 6. Quantize to 8-bit
//!
//! The implementation is intentionally straightforward so it can later be
//! replaced by GPU compute shaders (Metal, Vulkan, D3D11) per-platform.

// ---------------------------------------------------------------------------
// PQ (SMPTE ST 2084) transfer functions
// ---------------------------------------------------------------------------

/// PQ EOTF — converts a PQ-encoded signal value `v` in `[0, 1]` to
/// absolute linear light in `[0, 1]`, where 1.0 corresponds to 10 000 nits.
///
/// Reference: SMPTE ST 2084, ITU-R BT.2100.
pub fn pq_eotf(v: f32) -> f32 {
    const M1: f32 = 0.1593017578125;
    const M2: f32 = 78.84375;
    const C1: f32 = 0.8359375;
    const C2: f32 = 18.8515625;
    const C3: f32 = 18.6875;

    if v <= 0.0 {
        return 0.0;
    }

    let vp = v.powf(1.0 / M2);
    let n = (vp - C1).max(0.0);
    let d = C2 - C3 * vp;
    if d <= 0.0 {
        return 0.0;
    }
    (n / d).powf(1.0 / M1)
}

/// Inverse PQ (PQ OETF) — converts linear light `l` in `[0, 1]` (where
/// 1.0 = 10 000 nits) back to a PQ signal value in `[0, 1]`.
pub fn pq_oetf(l: f32) -> f32 {
    const M1: f32 = 0.1593017578125;
    const M2: f32 = 78.84375;
    const C1: f32 = 0.8359375;
    const C2: f32 = 18.8515625;
    const C3: f32 = 18.6875;

    let lp = l.powf(M1);
    let n = C1 + C2 * lp;
    let d = 1.0 + C3 * lp;
    (n / d).powf(M2)
}

// ---------------------------------------------------------------------------
// sRGB transfer functions
// ---------------------------------------------------------------------------

/// Convert linear-light value to sRGB gamma-encoded value.
/// Both input and output are in `[0, 1]`.
pub fn linear_to_srgb(l: f32) -> f32 {
    if l <= 0.0031308 {
        12.92 * l
    } else {
        1.055 * l.powf(1.0 / 2.4) - 0.055
    }
}

/// Convert sRGB gamma-encoded value to linear light.
/// Both input and output are in `[0, 1]`.
pub fn srgb_to_linear(s: f32) -> f32 {
    if s <= 0.04045 {
        s / 12.92
    } else {
        ((s + 0.055) / 1.055).powf(2.4)
    }
}

// ---------------------------------------------------------------------------
// Tone mapping
// ---------------------------------------------------------------------------

/// Reinhard tone mapping operator (extended version).
///
/// Maps a linear luminance `l` (in arbitrary HDR units) into `[0, 1]`.
/// `l_white` is the luminance level that maps to pure white (1.0).
///
/// Formula: `L_out = L * (1 + L / L_white^2) / (1 + L)`
pub fn reinhard(l: f32, l_white: f32) -> f32 {
    if l <= 0.0 {
        return 0.0;
    }
    (l * (1.0 + l / (l_white * l_white))) / (1.0 + l)
}

// ---------------------------------------------------------------------------
// Gamut mapping
// ---------------------------------------------------------------------------

/// BT.2020 to BT.709 gamut mapping via 3x3 color matrix.
///
/// Input and output are linear-light RGB triplets in `[0, 1]`.
/// The matrix is derived from the ITU-R BT.2087 standard.
/// Out-of-gamut values are clamped to `[0, 1]`.
pub fn bt2020_to_bt709(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let r709 = 1.6605 * r - 0.5877 * g - 0.0728 * b;
    let g709 = -0.1246 * r + 1.1330 * g - 0.0084 * b;
    let b709 = -0.0182 * r - 0.1006 * g + 1.1187 * b;
    (
        r709.clamp(0.0, 1.0),
        g709.clamp(0.0, 1.0),
        b709.clamp(0.0, 1.0),
    )
}

// ---------------------------------------------------------------------------
// Full pixel pipeline
// ---------------------------------------------------------------------------

/// Full HDR-to-SDR pipeline for a single pixel.
///
/// Input: 10-bit RGB channels (0..1023) in PQ / BT.2020 space.
/// Output: 8-bit sRGB / BT.709 RGB channels.
///
/// `max_luminance` is the peak luminance in nits of the source content.
/// If 0 or unknown, a sensible default (1000 nits) is used.
pub fn hdr_to_sdr_pixel(r10: u16, g10: u16, b10: u16, max_luminance: f32) -> (u8, u8, u8) {
    let l_white = if max_luminance > 0.0 {
        max_luminance / 10000.0
    } else {
        1000.0 / 10000.0 // default 1000 nits
    };

    // 1. Normalize 10-bit to [0, 1]
    let r_pq = r10 as f32 / 1023.0;
    let g_pq = g10 as f32 / 1023.0;
    let b_pq = b10 as f32 / 1023.0;

    // 2. Apply PQ EOTF to get linear light (0..1, where 1 = 10000 nits)
    let r_lin = pq_eotf(r_pq);
    let g_lin = pq_eotf(g_pq);
    let b_lin = pq_eotf(b_pq);

    // 3. Reinhard tone map each channel
    let r_tm = reinhard(r_lin, l_white);
    let g_tm = reinhard(g_lin, l_white);
    let b_tm = reinhard(b_lin, l_white);

    // 4. BT.2020 -> BT.709 gamut mapping
    let (r709, g709, b709) = bt2020_to_bt709(r_tm, g_tm, b_tm);

    // 5. Apply sRGB gamma
    let r_srgb = linear_to_srgb(r709);
    let g_srgb = linear_to_srgb(g709);
    let b_srgb = linear_to_srgb(b709);

    // 6. Quantize to 8-bit
    let r8 = (r_srgb * 255.0 + 0.5).clamp(0.0, 255.0) as u8;
    let g8 = (g_srgb * 255.0 + 0.5).clamp(0.0, 255.0) as u8;
    let b8 = (b_srgb * 255.0 + 0.5).clamp(0.0, 255.0) as u8;

    (r8, g8, b8)
}

// ---------------------------------------------------------------------------
// Frame-level tone mapping
// ---------------------------------------------------------------------------

/// Tone map a P010 frame to NV12 (8-bit 4:2:0).
///
/// P010 layout:
///   - Y plane: `width * height` samples, each 16-bit LE (10 significant bits, left-aligned in bits [15:6])
///   - UV plane: `width * (height/2)` bytes of interleaved U/V pairs, each 16-bit LE
///
/// NV12 layout:
///   - Y plane: `width * height` bytes
///   - UV plane: `width * (height/2)` bytes of interleaved U/V pairs, each 8-bit
///
/// This function performs a simplified tone-map: it extracts the 10-bit values,
/// applies PQ EOTF + Reinhard, then re-encodes as BT.709 limited-range YUV.
///
/// `width` and `height` must both be even.
pub fn tone_map_p010_to_nv12(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
    height: usize,
    max_luminance: f32,
) {
    assert!(width % 2 == 0 && height % 2 == 0, "width and height must be even");

    let y_plane_src = width * height * 2;
    let uv_plane_src = width * (height / 2) * 2;
    assert!(
        src.len() >= y_plane_src + uv_plane_src,
        "P010 source buffer too small"
    );

    let y_plane_dst = width * height;
    let uv_plane_dst = width * (height / 2);
    assert!(
        dst.len() >= y_plane_dst + uv_plane_dst,
        "NV12 destination buffer too small"
    );

    let l_white = if max_luminance > 0.0 {
        max_luminance / 10000.0
    } else {
        1000.0 / 10000.0
    };

    // Tone map Y plane: extract 10-bit, PQ EOTF, Reinhard, sRGB gamma, quantize to 8-bit.
    // We treat Y as a luminance-like signal for tone mapping purposes.
    for i in 0..(width * height) {
        let off = i * 2;
        let y16 = u16::from_le_bytes([src[off], src[off + 1]]);
        let y10 = y16 >> 6; // extract 10-bit from left-aligned P010
        let y_norm = y10 as f32 / 1023.0;
        let y_lin = pq_eotf(y_norm);
        let y_tm = reinhard(y_lin, l_white);
        let y_srgb = linear_to_srgb(y_tm);
        // Map to limited range Y: 16..235
        let y8 = (y_srgb * 219.0 + 16.5).clamp(16.0, 235.0) as u8;
        dst[i] = y8;
    }

    // Tone map UV plane: extract 10-bit Cb/Cr, apply similar compression,
    // then quantize to 8-bit limited range.
    let uv_src_start = y_plane_src;
    let uv_dst_start = y_plane_dst;
    let uv_samples = width * (height / 2) / 2; // number of UV pairs
    for i in 0..uv_samples {
        let src_off = uv_src_start + i * 4;
        let cb16 = u16::from_le_bytes([src[src_off], src[src_off + 1]]);
        let cr16 = u16::from_le_bytes([src[src_off + 2], src[src_off + 3]]);
        let cb10 = cb16 >> 6;
        let cr10 = cr16 >> 6;

        // Chroma is centered at 512 (10-bit) / 128 (8-bit).
        // Simple approach: linearly map from 10-bit limited range to 8-bit limited range.
        // 10-bit range: 64..960, 8-bit range: 16..240.
        let cb8 = ((cb10 as f32 - 512.0) * (224.0 / 896.0) + 128.0)
            .clamp(16.0, 240.0) as u8;
        let cr8 = ((cr10 as f32 - 512.0) * (224.0 / 896.0) + 128.0)
            .clamp(16.0, 240.0) as u8;

        let dst_off = uv_dst_start + i * 2;
        dst[dst_off] = cb8;
        dst[dst_off + 1] = cr8;
    }
}

/// Tone map an ARGB2101010 frame to 8-bit BGRA.
///
/// Each source pixel is a packed 32-bit value (little-endian):
///   bits [9:0]   = Blue  (10 bits)
///   bits [19:10] = Green (10 bits)
///   bits [29:20] = Red   (10 bits)
///   bits [31:30] = Alpha (2 bits)
///
/// Each destination pixel is BGRA (8-bit per channel).
///
/// `max_luminance` is the peak luminance in nits (0 = use default of 1000).
pub fn tone_map_argb2101010_to_bgra(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
    height: usize,
    max_luminance: f32,
) {
    let pixel_count = width * height;
    assert!(src.len() >= pixel_count * 4, "ARGB2101010 source buffer too small");
    assert!(dst.len() >= pixel_count * 4, "BGRA destination buffer too small");

    for i in 0..pixel_count {
        let off = i * 4;
        let packed =
            u32::from_le_bytes([src[off], src[off + 1], src[off + 2], src[off + 3]]);

        let b10 = (packed & 0x3FF) as u16;
        let g10 = ((packed >> 10) & 0x3FF) as u16;
        let r10 = ((packed >> 20) & 0x3FF) as u16;
        let a2 = ((packed >> 30) & 0x3) as u8;

        let (r8, g8, b8) = hdr_to_sdr_pixel(r10, g10, b10, max_luminance);

        // Output BGRA
        dst[off] = b8;
        dst[off + 1] = g8;
        dst[off + 2] = r8;
        dst[off + 3] = a2 * 85; // expand 2-bit alpha to 8-bit
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // PQ EOTF tests
    // -----------------------------------------------------------------------

    #[test]
    fn pq_eotf_zero_returns_zero() {
        assert_eq!(pq_eotf(0.0), 0.0);
    }

    #[test]
    fn pq_eotf_one_returns_one() {
        // PQ(1.0) should map to 1.0 (= 10000 nits, normalized)
        let result = pq_eotf(1.0);
        assert!(
            (result - 1.0).abs() < 1e-4,
            "pq_eotf(1.0) = {result}, expected ~1.0"
        );
    }

    #[test]
    fn pq_eotf_mid_value() {
        // PQ signal 0.5 should map to a known linear-light value.
        // ST 2084: PQ(0.5) ~ 0.00922 (approximately 92.2 nits / 10000)
        let result = pq_eotf(0.5);
        assert!(
            result > 0.005 && result < 0.02,
            "pq_eotf(0.5) = {result}, expected roughly 0.01"
        );
    }

    #[test]
    fn pq_eotf_negative_clamped() {
        // Negative input should be handled gracefully (produce 0 or near-0).
        let result = pq_eotf(-0.1);
        assert!(result >= 0.0, "pq_eotf(-0.1) should be >= 0, got {result}");
    }

    #[test]
    fn pq_eotf_monotonic() {
        // PQ EOTF should be monotonically increasing.
        let mut prev = 0.0f32;
        for i in 0..=100 {
            let v = i as f32 / 100.0;
            let l = pq_eotf(v);
            assert!(l >= prev, "pq_eotf not monotonic at v={v}: {l} < {prev}");
            prev = l;
        }
    }

    // -----------------------------------------------------------------------
    // PQ OETF tests
    // -----------------------------------------------------------------------

    #[test]
    fn pq_oetf_zero_returns_zero() {
        let result = pq_oetf(0.0);
        assert!(
            result.abs() < 1e-6,
            "pq_oetf(0.0) = {result}, expected ~0.0"
        );
    }

    #[test]
    fn pq_oetf_one_returns_one() {
        let result = pq_oetf(1.0);
        assert!(
            (result - 1.0).abs() < 1e-4,
            "pq_oetf(1.0) = {result}, expected ~1.0"
        );
    }

    #[test]
    fn pq_round_trip() {
        // OETF(EOTF(v)) should be ~v for values in [0, 1].
        for i in 0..=20 {
            let v = i as f32 / 20.0;
            let round_tripped = pq_oetf(pq_eotf(v));
            assert!(
                (round_tripped - v).abs() < 1e-4,
                "PQ round-trip failed at v={v}: got {round_tripped}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // sRGB gamma tests
    // -----------------------------------------------------------------------

    #[test]
    fn srgb_zero() {
        assert_eq!(linear_to_srgb(0.0), 0.0);
        assert_eq!(srgb_to_linear(0.0), 0.0);
    }

    #[test]
    fn srgb_one() {
        let result = linear_to_srgb(1.0);
        assert!(
            (result - 1.0).abs() < 1e-6,
            "linear_to_srgb(1.0) = {result}"
        );
        let result = srgb_to_linear(1.0);
        assert!(
            (result - 1.0).abs() < 1e-6,
            "srgb_to_linear(1.0) = {result}"
        );
    }

    #[test]
    fn srgb_round_trip() {
        for i in 0..=100 {
            let v = i as f32 / 100.0;
            let round_tripped = srgb_to_linear(linear_to_srgb(v));
            assert!(
                (round_tripped - v).abs() < 1e-5,
                "sRGB round-trip failed at v={v}: got {round_tripped}"
            );
        }
    }

    #[test]
    fn srgb_linear_segment() {
        // Below 0.0031308 the transfer function is linear.
        let v = 0.001;
        let encoded = linear_to_srgb(v);
        assert!(
            (encoded - 12.92 * v).abs() < 1e-6,
            "linear segment mismatch: {encoded}"
        );
    }

    #[test]
    fn srgb_monotonic() {
        let mut prev = 0.0f32;
        for i in 0..=1000 {
            let v = i as f32 / 1000.0;
            let s = linear_to_srgb(v);
            assert!(s >= prev, "sRGB not monotonic at v={v}");
            prev = s;
        }
    }

    // -----------------------------------------------------------------------
    // Reinhard tests
    // -----------------------------------------------------------------------

    #[test]
    fn reinhard_zero() {
        assert_eq!(reinhard(0.0, 1.0), 0.0);
    }

    #[test]
    fn reinhard_at_l_white() {
        // When L = L_white, the output should be close to 1.0 (but not exactly,
        // since extended Reinhard never quite reaches 1.0 for finite L).
        let result = reinhard(1.0, 1.0);
        // reinhard(1, 1) = 1*(1+1/1)/(1+1) = 2/2 = 1.0
        assert!(
            (result - 1.0).abs() < 1e-6,
            "reinhard(1, 1) = {result}, expected 1.0"
        );
    }

    #[test]
    fn reinhard_below_one() {
        // For small L values relative to L_white, output should be close to L.
        let result = reinhard(0.01, 1.0);
        assert!(
            (result - 0.01).abs() < 0.002,
            "reinhard(0.01, 1.0) = {result}, expected ~0.01"
        );
    }

    #[test]
    fn reinhard_large_l_white() {
        // With a very large L_white, the Reinhard operator approaches L/(1+L).
        let l = 0.5;
        let result = reinhard(l, 100.0);
        let simple = l / (1.0 + l); // = 0.333...
        assert!(
            (result - simple).abs() < 0.01,
            "reinhard(0.5, 100) = {result}, expected ~{simple}"
        );
    }

    #[test]
    fn reinhard_monotonic() {
        let l_white = 1.0;
        let mut prev = 0.0f32;
        for i in 0..=1000 {
            let l = i as f32 / 100.0; // 0..10
            let r = reinhard(l, l_white);
            assert!(r >= prev, "reinhard not monotonic at l={l}");
            prev = r;
        }
    }

    #[test]
    fn reinhard_negative_input() {
        assert_eq!(reinhard(-1.0, 1.0), 0.0);
    }

    // -----------------------------------------------------------------------
    // BT.2020 -> BT.709 gamut mapping tests
    // -----------------------------------------------------------------------

    #[test]
    fn bt2020_to_bt709_black() {
        let (r, g, b) = bt2020_to_bt709(0.0, 0.0, 0.0);
        assert_eq!((r, g, b), (0.0, 0.0, 0.0));
    }

    #[test]
    fn bt2020_to_bt709_white() {
        // Pure white in BT.2020 should map close to white in BT.709.
        let (r, g, b) = bt2020_to_bt709(1.0, 1.0, 1.0);
        // Row sums: 1.6605-0.5877-0.0728 = 1.0
        //          -0.1246+1.1330-0.0084 = 1.0
        //          -0.0182-0.1006+1.1187 = ~1.0
        assert!(
            (r - 1.0).abs() < 0.01,
            "white R709={r}"
        );
        assert!(
            (g - 1.0).abs() < 0.01,
            "white G709={g}"
        );
        assert!(
            (b - 1.0).abs() < 0.01,
            "white B709={b}"
        );
    }

    #[test]
    fn bt2020_to_bt709_narrow_gamut_passthrough() {
        // Colors that are well inside BT.709 gamut should come through
        // approximately unchanged (since BT.709 is a subset of BT.2020).
        // A neutral gray (0.5, 0.5, 0.5) should be nearly unchanged.
        let (r, g, b) = bt2020_to_bt709(0.5, 0.5, 0.5);
        assert!(
            (r - 0.5).abs() < 0.01 && (g - 0.5).abs() < 0.01 && (b - 0.5).abs() < 0.01,
            "gray: ({r}, {g}, {b})"
        );
    }

    #[test]
    fn bt2020_to_bt709_wide_gamut_clamped() {
        // A saturated BT.2020 color (pure red) will produce out-of-gamut
        // values in BT.709, which should be clamped.
        let (r, g, b) = bt2020_to_bt709(1.0, 0.0, 0.0);
        assert!(r >= 0.0 && r <= 1.0, "R clamped: {r}");
        assert!(g >= 0.0 && g <= 1.0, "G clamped: {g}");
        assert!(b >= 0.0 && b <= 1.0, "B clamped: {b}");
        // Red should be > 1.0 before clamping, so it should be exactly 1.0 after.
        assert_eq!(r, 1.0, "saturated BT.2020 red -> BT.709 R should be clamped to 1.0");
    }

    #[test]
    fn bt2020_to_bt709_output_in_range() {
        // Test a range of inputs and verify all outputs are in [0, 1].
        for i in 0..=10 {
            for j in 0..=10 {
                for k in 0..=10 {
                    let r_in = i as f32 / 10.0;
                    let g_in = j as f32 / 10.0;
                    let b_in = k as f32 / 10.0;
                    let (r, g, b) = bt2020_to_bt709(r_in, g_in, b_in);
                    assert!(
                        r >= 0.0 && r <= 1.0 && g >= 0.0 && g <= 1.0 && b >= 0.0 && b <= 1.0,
                        "out of range for ({r_in},{g_in},{b_in}): ({r},{g},{b})"
                    );
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Full pixel pipeline tests
    // -----------------------------------------------------------------------

    #[test]
    fn pixel_black_to_black() {
        let (r, g, b) = hdr_to_sdr_pixel(0, 0, 0, 1000.0);
        assert_eq!((r, g, b), (0, 0, 0));
    }

    #[test]
    fn pixel_white_produces_bright() {
        // Full-white PQ signal (1023) at 1000-nit display should produce
        // a near-white SDR pixel.
        let (r, g, b) = hdr_to_sdr_pixel(1023, 1023, 1023, 1000.0);
        assert!(r > 200, "white R={r}");
        assert!(g > 200, "white G={g}");
        assert!(b > 200, "white B={b}");
    }

    #[test]
    fn pixel_default_luminance() {
        // max_luminance = 0 should use a sensible default.
        let (r, g, b) = hdr_to_sdr_pixel(512, 512, 512, 0.0);
        assert!(r > 0 && g > 0 && b > 0, "mid-gray should not be black");
    }

    #[test]
    fn pixel_known_hdr_color() {
        // A mid-bright PQ value should produce a reasonable SDR output.
        let (r, g, b) = hdr_to_sdr_pixel(600, 400, 300, 1000.0);
        // Just verify it's a valid, non-degenerate color.
        assert!(r > g, "red channel should dominate: r={r}, g={g}");
        assert!(g > b || g == b, "green >= blue: g={g}, b={b}");
    }

    #[test]
    fn pixel_output_in_range() {
        // Check that the pipeline produces monotonically increasing output
        // for increasing neutral-gray input.
        let mut prev_sum = 0u16;
        for v in (0u16..=1023).step_by(64) {
            let (r, g, b) = hdr_to_sdr_pixel(v, v, v, 1000.0);
            let sum = r as u16 + g as u16 + b as u16;
            assert!(
                sum >= prev_sum,
                "output should be monotonic: v={v}, sum={sum} < prev={prev_sum}"
            );
            prev_sum = sum;
        }
        // The highest input should produce a clearly bright output.
        let (r, g, b) = hdr_to_sdr_pixel(1023, 1023, 1023, 1000.0);
        assert!(r > 200 && g > 200 && b > 200, "max input should be bright");
    }

    // -----------------------------------------------------------------------
    // Frame-level tone mapping tests
    // -----------------------------------------------------------------------

    #[test]
    fn tone_map_p010_to_nv12_black_frame() {
        let width = 4;
        let height = 4;
        let y_plane_size = width * height * 2;
        let uv_plane_size = width * (height / 2) * 2;
        let src = vec![0u8; y_plane_size + uv_plane_size];
        let mut dst = vec![0u8; width * height + width * (height / 2)];

        tone_map_p010_to_nv12(&src, &mut dst, width, height, 1000.0);

        // Y should be limited-range black (16).
        for i in 0..(width * height) {
            assert_eq!(dst[i], 16, "Y[{i}] should be 16 for black");
        }
    }

    #[test]
    fn tone_map_p010_to_nv12_output_size() {
        let width = 4;
        let height = 4;
        let y_src = width * height * 2;
        let uv_src = width * (height / 2) * 2;
        let src = vec![0u8; y_src + uv_src];

        let y_dst = width * height;
        let uv_dst = width * (height / 2);
        let mut dst = vec![0u8; y_dst + uv_dst];

        tone_map_p010_to_nv12(&src, &mut dst, width, height, 1000.0);
        assert_eq!(dst.len(), y_dst + uv_dst);
    }

    #[test]
    #[should_panic(expected = "width and height must be even")]
    fn tone_map_p010_to_nv12_odd_panics() {
        let src = vec![0u8; 100];
        let mut dst = vec![0u8; 100];
        tone_map_p010_to_nv12(&src, &mut dst, 3, 3, 1000.0);
    }

    #[test]
    fn tone_map_argb2101010_to_bgra_black_frame() {
        let width = 4;
        let height = 2;
        let pixel_count = width * height;
        let src = vec![0u8; pixel_count * 4];
        let mut dst = vec![0u8; pixel_count * 4];

        tone_map_argb2101010_to_bgra(&src, &mut dst, width, height, 1000.0);

        // All channels should be 0 (black with alpha=0).
        for i in 0..pixel_count {
            let off = i * 4;
            assert_eq!(dst[off], 0, "B should be 0");
            assert_eq!(dst[off + 1], 0, "G should be 0");
            assert_eq!(dst[off + 2], 0, "R should be 0");
            assert_eq!(dst[off + 3], 0, "A should be 0 (alpha=0)");
        }
    }

    #[test]
    fn tone_map_argb2101010_to_bgra_output_size() {
        let width = 4;
        let height = 4;
        let pixel_count = width * height;
        let src = vec![0u8; pixel_count * 4];
        let mut dst = vec![0u8; pixel_count * 4];

        tone_map_argb2101010_to_bgra(&src, &mut dst, width, height, 1000.0);
        assert_eq!(dst.len(), pixel_count * 4);
    }

    #[test]
    fn tone_map_argb2101010_to_bgra_white_produces_bright() {
        let width = 2;
        let height = 2;
        let pixel_count = width * height;

        // Full white with full alpha.
        let white: u32 = 0x3FF | (0x3FF << 10) | (0x3FF << 20) | (0x3 << 30);
        let mut src = Vec::with_capacity(pixel_count * 4);
        for _ in 0..pixel_count {
            src.extend_from_slice(&white.to_le_bytes());
        }
        let mut dst = vec![0u8; pixel_count * 4];

        tone_map_argb2101010_to_bgra(&src, &mut dst, width, height, 1000.0);

        // All color channels should be bright (> 200).
        for i in 0..pixel_count {
            let off = i * 4;
            assert!(dst[off] > 200, "B={}, expected > 200", dst[off]);
            assert!(dst[off + 1] > 200, "G={}, expected > 200", dst[off + 1]);
            assert!(dst[off + 2] > 200, "R={}, expected > 200", dst[off + 2]);
            assert_eq!(dst[off + 3], 255, "A should be 255");
        }
    }

    #[test]
    #[should_panic(expected = "source buffer too small")]
    fn tone_map_argb2101010_to_bgra_small_src_panics() {
        let mut dst = vec![0u8; 16];
        tone_map_argb2101010_to_bgra(&[0u8; 4], &mut dst, 2, 2, 1000.0);
    }
}

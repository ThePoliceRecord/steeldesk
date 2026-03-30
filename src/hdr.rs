//! HDR (High Dynamic Range) support module.
//!
//! This module provides:
//! - Re-exports of HDR types and functions from the `scrap` crate
//! - HDR metadata handling for the video pipeline
//! - Conversion utilities for 10-bit to 8-bit fallback
//!
//! # Environment variable
//!
//! Set `STEELDESK_HDR=1` to force-enable HDR capture on any platform.
//! Without this (or native display detection), HDR is off by default.

pub use scrap::{current_hdr_info, hdr_pixfmt, is_display_hdr, HdrInfo, Pixfmt};

#[cfg(test)]
mod tests {
    use scrap::{argb2101010_to_bgra, HdrInfo, Pixfmt};

    // -----------------------------------------------------------------------
    // Pixfmt enum tests
    // -----------------------------------------------------------------------

    #[test]
    fn argb2101010_exists_in_pixfmt_enum() {
        let fmt = Pixfmt::ARGB2101010;
        assert_eq!(format!("{:?}", fmt), "ARGB2101010");
    }

    #[test]
    fn p010_exists_in_pixfmt_enum() {
        let fmt = Pixfmt::P010;
        assert_eq!(format!("{:?}", fmt), "P010");
    }

    #[test]
    fn argb2101010_is_32bpp() {
        assert_eq!(Pixfmt::ARGB2101010.bpp(), 32);
        assert_eq!(Pixfmt::ARGB2101010.bytes_per_pixel(), 4);
    }

    #[test]
    fn p010_is_24bpp_stored() {
        // P010 is 10-bit 4:2:0, stored in 16-bit words.
        // bpp is 24 (Y plane 16-bit + UV plane 16-bit at half resolution).
        assert_eq!(Pixfmt::P010.bpp(), 24);
        assert_eq!(Pixfmt::P010.bytes_per_pixel(), 3);
    }

    #[test]
    fn is_10bit_identifies_hdr_formats() {
        assert!(Pixfmt::ARGB2101010.is_10bit());
        assert!(Pixfmt::P010.is_10bit());
        assert!(!Pixfmt::BGRA.is_10bit());
        assert!(!Pixfmt::RGBA.is_10bit());
        assert!(!Pixfmt::I420.is_10bit());
        assert!(!Pixfmt::NV12.is_10bit());
        assert!(!Pixfmt::I444.is_10bit());
        assert!(!Pixfmt::RGB565LE.is_10bit());
    }

    #[test]
    fn existing_pixfmts_unchanged() {
        // Make sure we didn't break existing format properties.
        assert_eq!(Pixfmt::BGRA.bpp(), 32);
        assert_eq!(Pixfmt::RGBA.bpp(), 32);
        assert_eq!(Pixfmt::RGB565LE.bpp(), 16);
        assert_eq!(Pixfmt::I420.bpp(), 12);
        assert_eq!(Pixfmt::NV12.bpp(), 12);
        assert_eq!(Pixfmt::I444.bpp(), 24);
    }

    #[test]
    fn pixfmt_equality() {
        assert_eq!(Pixfmt::ARGB2101010, Pixfmt::ARGB2101010);
        assert_ne!(Pixfmt::ARGB2101010, Pixfmt::BGRA);
        assert_eq!(Pixfmt::P010, Pixfmt::P010);
        assert_ne!(Pixfmt::P010, Pixfmt::NV12);
    }

    #[test]
    fn pixfmt_copy_clone() {
        let a = Pixfmt::ARGB2101010;
        let b = a; // Copy
        let c = a.clone(); // Clone
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    // -----------------------------------------------------------------------
    // HDR detection tests
    // -----------------------------------------------------------------------

    #[test]
    fn is_display_hdr_returns_false_by_default() {
        // Clear the env var to ensure default behavior.
        std::env::remove_var("STEELDESK_HDR");
        assert!(!scrap::is_display_hdr());
    }

    #[test]
    fn is_display_hdr_respects_env_var() {
        std::env::set_var("STEELDESK_HDR", "1");
        assert!(scrap::is_display_hdr());
        // Clean up.
        std::env::remove_var("STEELDESK_HDR");
    }

    #[test]
    fn is_display_hdr_ignores_other_values() {
        std::env::set_var("STEELDESK_HDR", "true");
        assert!(!scrap::is_display_hdr());
        std::env::set_var("STEELDESK_HDR", "yes");
        assert!(!scrap::is_display_hdr());
        std::env::set_var("STEELDESK_HDR", "0");
        assert!(!scrap::is_display_hdr());
        std::env::set_var("STEELDESK_HDR", "");
        assert!(!scrap::is_display_hdr());
        // Clean up.
        std::env::remove_var("STEELDESK_HDR");
    }

    #[test]
    fn hdr_pixfmt_returns_bgra_by_default() {
        std::env::remove_var("STEELDESK_HDR");
        assert_eq!(scrap::hdr_pixfmt(), Pixfmt::BGRA);
    }

    #[test]
    fn hdr_pixfmt_returns_argb2101010_when_hdr() {
        std::env::set_var("STEELDESK_HDR", "1");
        assert_eq!(scrap::hdr_pixfmt(), Pixfmt::ARGB2101010);
        std::env::remove_var("STEELDESK_HDR");
    }

    // -----------------------------------------------------------------------
    // HdrInfo struct tests
    // -----------------------------------------------------------------------

    #[test]
    fn hdr_info_default_is_sdr() {
        let info = HdrInfo::default();
        assert!(!info.enabled);
        assert_eq!(info.bit_depth, 8);
        assert_eq!(info.max_luminance, 0);
    }

    #[test]
    fn hdr_info_sdr_matches_default() {
        assert_eq!(HdrInfo::sdr(), HdrInfo::default());
    }

    #[test]
    fn hdr_info_hdr_constructor() {
        let info = HdrInfo::hdr(10, 1000);
        assert!(info.enabled);
        assert_eq!(info.bit_depth, 10);
        assert_eq!(info.max_luminance, 1000);
    }

    #[test]
    fn hdr_info_clone_and_eq() {
        let a = HdrInfo::hdr(10, 1600);
        let b = a.clone();
        assert_eq!(a, b);
    }

    #[test]
    fn hdr_info_ne() {
        assert_ne!(HdrInfo::sdr(), HdrInfo::hdr(10, 1000));
        assert_ne!(HdrInfo::hdr(10, 1000), HdrInfo::hdr(10, 1600));
        assert_ne!(HdrInfo::hdr(8, 1000), HdrInfo::hdr(10, 1000));
    }

    #[test]
    fn hdr_info_debug_format() {
        let info = HdrInfo::hdr(10, 1000);
        let dbg = format!("{:?}", info);
        assert!(dbg.contains("enabled: true"));
        assert!(dbg.contains("bit_depth: 10"));
        assert!(dbg.contains("max_luminance: 1000"));
    }

    #[test]
    fn current_hdr_info_sdr_by_default() {
        std::env::remove_var("STEELDESK_HDR");
        let info = scrap::current_hdr_info();
        assert!(!info.enabled);
        assert_eq!(info.bit_depth, 8);
    }

    #[test]
    fn current_hdr_info_hdr_when_enabled() {
        std::env::set_var("STEELDESK_HDR", "1");
        let info = scrap::current_hdr_info();
        assert!(info.enabled);
        assert_eq!(info.bit_depth, 10);
        std::env::remove_var("STEELDESK_HDR");
    }

    // -----------------------------------------------------------------------
    // ARGB2101010 → BGRA conversion tests
    // -----------------------------------------------------------------------

    #[test]
    fn argb2101010_to_bgra_empty_input() {
        let src: &[u8] = &[];
        let mut dst = Vec::new();
        argb2101010_to_bgra(src, &mut dst);
        assert!(dst.is_empty());
    }

    #[test]
    fn argb2101010_to_bgra_single_black_pixel() {
        // All zeros = black with alpha 0.
        let src = 0u32.to_le_bytes();
        let mut dst = Vec::new();
        argb2101010_to_bgra(&src, &mut dst);
        assert_eq!(dst, vec![0, 0, 0, 0]); // B=0, G=0, R=0, A=0
    }

    #[test]
    fn argb2101010_to_bgra_full_white_full_alpha() {
        // Alpha = 3 (2 bits max), R=G=B = 1023 (10 bits max).
        //   bits [9:0]   = B = 1023 = 0x3FF
        //   bits [19:10] = G = 1023 = 0x3FF << 10
        //   bits [29:20] = R = 1023 = 0x3FF << 20
        //   bits [31:30] = A = 3    = 0x3   << 30
        let packed: u32 = 0x3FF | (0x3FF << 10) | (0x3FF << 20) | (0x3 << 30);
        let src = packed.to_le_bytes();
        let mut dst = Vec::new();
        argb2101010_to_bgra(&src, &mut dst);
        // 1023 >> 2 = 255, alpha 3*85=255.
        assert_eq!(dst, vec![255, 255, 255, 255]);
    }

    #[test]
    fn argb2101010_to_bgra_known_color() {
        // Encode a specific color:
        //   B = 512 (10-bit), G = 256 (10-bit), R = 768 (10-bit), A = 2 (2-bit)
        let b: u32 = 512;
        let g: u32 = 256;
        let r: u32 = 768;
        let a: u32 = 2;
        let packed = b | (g << 10) | (r << 20) | (a << 30);
        let src = packed.to_le_bytes();
        let mut dst = Vec::new();
        argb2101010_to_bgra(&src, &mut dst);
        assert_eq!(dst[0], (512 >> 2) as u8); // B = 128
        assert_eq!(dst[1], (256 >> 2) as u8); // G = 64
        assert_eq!(dst[2], (768 >> 2) as u8); // R = 192
        assert_eq!(dst[3], 2 * 85);           // A = 170
    }

    #[test]
    fn argb2101010_to_bgra_precision_loss() {
        // 10-bit value 1023 → 8-bit 255 (no loss for max).
        // 10-bit value 4    → 8-bit 1   (4 >> 2 = 1).
        // 10-bit value 3    → 8-bit 0   (3 >> 2 = 0, precision lost).
        // 10-bit value 1    → 8-bit 0   (1 >> 2 = 0, precision lost).
        let b: u32 = 3; // will become 0
        let g: u32 = 4; // will become 1
        let r: u32 = 1023; // will become 255
        let a: u32 = 1; // will become 85
        let packed = b | (g << 10) | (r << 20) | (a << 30);
        let src = packed.to_le_bytes();
        let mut dst = Vec::new();
        argb2101010_to_bgra(&src, &mut dst);
        assert_eq!(dst[0], 0);   // B: 3 >> 2 = 0 (lost 2 LSBs)
        assert_eq!(dst[1], 1);   // G: 4 >> 2 = 1
        assert_eq!(dst[2], 255); // R: 1023 >> 2 = 255
        assert_eq!(dst[3], 85);  // A: 1 * 85 = 85
    }

    #[test]
    fn argb2101010_to_bgra_multiple_pixels() {
        // Two pixels: first black (alpha 0), second white (alpha 3).
        let black: u32 = 0;
        let white: u32 = 0x3FF | (0x3FF << 10) | (0x3FF << 20) | (0x3 << 30);
        let mut src = Vec::new();
        src.extend_from_slice(&black.to_le_bytes());
        src.extend_from_slice(&white.to_le_bytes());
        let mut dst = Vec::new();
        argb2101010_to_bgra(&src, &mut dst);
        assert_eq!(dst.len(), 8);
        // Pixel 0: black
        assert_eq!(&dst[0..4], &[0, 0, 0, 0]);
        // Pixel 1: white
        assert_eq!(&dst[4..8], &[255, 255, 255, 255]);
    }

    #[test]
    fn argb2101010_to_bgra_round_trip_top_8_bits() {
        // If we encode an 8-bit value v into 10-bit as (v << 2),
        // then converting back should give us exactly v.
        for v in (0u32..=255).step_by(17) {
            let b10 = v << 2;
            let g10 = v << 2;
            let r10 = v << 2;
            let packed = b10 | (g10 << 10) | (r10 << 20) | (0x3 << 30);
            let src = packed.to_le_bytes();
            let mut dst = Vec::new();
            argb2101010_to_bgra(&src, &mut dst);
            assert_eq!(dst[0], v as u8, "B channel round-trip failed for {v}");
            assert_eq!(dst[1], v as u8, "G channel round-trip failed for {v}");
            assert_eq!(dst[2], v as u8, "R channel round-trip failed for {v}");
        }
    }

    #[test]
    #[should_panic(expected = "multiple of 4")]
    fn argb2101010_to_bgra_panics_on_bad_length() {
        let src = vec![0u8; 5]; // not a multiple of 4
        let mut dst = Vec::new();
        argb2101010_to_bgra(&src, &mut dst);
    }

    #[test]
    fn argb2101010_to_bgra_alpha_expansion() {
        // Test all 4 possible alpha values (2-bit → 8-bit).
        for a2 in 0u32..=3 {
            let packed = 0x3FF | (0x3FF << 10) | (0x3FF << 20) | (a2 << 30);
            let src = packed.to_le_bytes();
            let mut dst = Vec::new();
            argb2101010_to_bgra(&src, &mut dst);
            let expected_alpha = (a2 as u8) * 85;
            assert_eq!(
                dst[3], expected_alpha,
                "alpha {a2} should map to {expected_alpha}"
            );
        }
    }

    #[test]
    fn argb2101010_to_bgra_large_buffer() {
        // 100 pixels, each with a different color.
        let mut src = Vec::with_capacity(400);
        for i in 0u32..100 {
            let b = i * 10;
            let g = i * 5;
            let r = 1023 - i * 10;
            let a = i % 4;
            let packed = (b & 0x3FF) | ((g & 0x3FF) << 10) | ((r & 0x3FF) << 20) | ((a & 0x3) << 30);
            src.extend_from_slice(&packed.to_le_bytes());
        }
        let mut dst = Vec::new();
        argb2101010_to_bgra(&src, &mut dst);
        assert_eq!(dst.len(), 400);
        // Spot-check pixel 50.
        let i = 50u32;
        let off = (i as usize) * 4;
        assert_eq!(dst[off], ((i * 10) >> 2) as u8); // B
        assert_eq!(dst[off + 1], ((i * 5) >> 2) as u8); // G
        assert_eq!(dst[off + 2], ((1023 - i * 10) >> 2) as u8); // R
        assert_eq!(dst[off + 3], ((i % 4) as u8) * 85); // A
    }

    #[test]
    fn argb2101010_to_bgra_dst_reuse() {
        // Verify that calling twice reuses/resizes the dst buffer correctly.
        let white: u32 = 0x3FF | (0x3FF << 10) | (0x3FF << 20) | (0x3 << 30);
        let mut src = Vec::new();
        src.extend_from_slice(&white.to_le_bytes());
        src.extend_from_slice(&white.to_le_bytes());

        let mut dst = Vec::new();
        argb2101010_to_bgra(&src, &mut dst);
        assert_eq!(dst.len(), 8);

        // Now convert a single pixel — dst should shrink.
        let single = white.to_le_bytes();
        argb2101010_to_bgra(&single, &mut dst);
        assert_eq!(dst.len(), 4);
    }

    // -----------------------------------------------------------------------
    // bgra_to_p010 conversion tests
    // -----------------------------------------------------------------------

    #[test]
    fn bgra_to_p010_black_2x2() {
        // 2x2 black image with full alpha.
        let src = vec![0u8, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255];
        let mut dst = Vec::new();
        scrap::bgra_to_p010(&src, 2, 2, &mut dst);

        // Y plane: 4 samples * 2 bytes = 8 bytes
        // UV plane: 1 chroma sample pair * 4 bytes = 4 bytes
        assert_eq!(dst.len(), 12);

        // Black in BT.601: Y = 16, Cb = Cr = 128.
        // As 10-bit left-shifted: Y = 16 << 2 = 64, Cb = 128 << 2 = 512, Cr = 128 << 2 = 512.
        for i in 0..4 {
            let y = u16::from_le_bytes([dst[i * 2], dst[i * 2 + 1]]);
            assert_eq!(y, 64, "Y sample {i} should be 64 (16 << 2)");
        }
        let cb = u16::from_le_bytes([dst[8], dst[9]]);
        let cr = u16::from_le_bytes([dst[10], dst[11]]);
        assert_eq!(cb, 512, "Cb should be 512 (128 << 2)");
        assert_eq!(cr, 512, "Cr should be 512 (128 << 2)");
    }

    #[test]
    fn bgra_to_p010_white_2x2() {
        // 2x2 white image: BGRA = (255, 255, 255, 255).
        let src = vec![255u8; 16]; // 4 pixels * 4 bytes
        let mut dst = Vec::new();
        scrap::bgra_to_p010(&src, 2, 2, &mut dst);

        // White in BT.601: Y = 235 (limited range max).
        // Y10 = 235 << 2 = 940.
        for i in 0..4 {
            let y = u16::from_le_bytes([dst[i * 2], dst[i * 2 + 1]]);
            // Y = ((66*255 + 129*255 + 25*255 + 128) >> 8) + 16
            //   = ((16830 + 32895 + 6375 + 128) >> 8) + 16
            //   = (56228 >> 8) + 16 = 219 + 16 = 235
            assert_eq!(y, 235 << 2, "Y sample {i} should be 940 (235 << 2)");
        }
    }

    #[test]
    fn bgra_to_p010_output_size() {
        let width = 4;
        let height = 4;
        let src = vec![128u8; width * height * 4];
        let mut dst = Vec::new();
        scrap::bgra_to_p010(&src, width, height, &mut dst);

        let expected_y_size = width * height * 2;
        let expected_uv_size = width * (height / 2) * 2;
        assert_eq!(dst.len(), expected_y_size + expected_uv_size);
    }

    #[test]
    #[should_panic(expected = "width and height must be even")]
    fn bgra_to_p010_odd_width_panics() {
        let src = vec![0u8; 3 * 2 * 4];
        let mut dst = Vec::new();
        scrap::bgra_to_p010(&src, 3, 2, &mut dst);
    }

    #[test]
    #[should_panic(expected = "width and height must be even")]
    fn bgra_to_p010_odd_height_panics() {
        let src = vec![0u8; 2 * 3 * 4];
        let mut dst = Vec::new();
        scrap::bgra_to_p010(&src, 2, 3, &mut dst);
    }

    // -----------------------------------------------------------------------
    // argb2101010_to_p010 conversion tests
    // -----------------------------------------------------------------------

    #[test]
    fn argb2101010_to_p010_black_2x2() {
        // All zeros = black (alpha 0, but alpha is ignored in YUV conversion).
        let src = vec![0u8; 16]; // 4 pixels
        let mut dst = Vec::new();
        scrap::argb2101010_to_p010(&src, 2, 2, &mut dst);

        // Y plane: 4 samples * 2 bytes = 8 bytes
        // UV plane: 1 chroma sample pair * 4 bytes = 4 bytes
        assert_eq!(dst.len(), 12);

        // Black: R=G=B=0, Y_10 = 64, Cb = Cr = 512 (limited range offsets).
        // Left-aligned in 16-bit: Y = 64 << 6 = 4096, Cb = 512 << 6 = 32768, Cr = 512 << 6 = 32768.
        for i in 0..4 {
            let y = u16::from_le_bytes([dst[i * 2], dst[i * 2 + 1]]);
            assert_eq!(y, 64 << 6, "Y sample {i} should be 4096 (64 << 6)");
        }
        let cb = u16::from_le_bytes([dst[8], dst[9]]);
        let cr = u16::from_le_bytes([dst[10], dst[11]]);
        assert_eq!(cb, 512 << 6, "Cb should be 32768 (512 << 6)");
        assert_eq!(cr, 512 << 6, "Cr should be 32768 (512 << 6)");
    }

    #[test]
    fn argb2101010_to_p010_output_size() {
        let width = 4;
        let height = 4;
        let src = vec![0u8; width * height * 4];
        let mut dst = Vec::new();
        scrap::argb2101010_to_p010(&src, width, height, &mut dst);

        let expected_y_size = width * height * 2;
        let expected_uv_size = width * (height / 2) * 2;
        assert_eq!(dst.len(), expected_y_size + expected_uv_size);
    }

    #[test]
    fn argb2101010_to_p010_white_2x2() {
        // Full white: R=G=B=1023 (10-bit max), alpha=3.
        let white: u32 = 0x3FF | (0x3FF << 10) | (0x3FF << 20) | (0x3 << 30);
        let mut src = Vec::new();
        for _ in 0..4 {
            src.extend_from_slice(&white.to_le_bytes());
        }
        let mut dst = Vec::new();
        scrap::argb2101010_to_p010(&src, 2, 2, &mut dst);

        // Y plane: white should be near max limited range (940).
        for i in 0..4 {
            let y = u16::from_le_bytes([dst[i * 2], dst[i * 2 + 1]]);
            let y10 = y >> 6; // extract 10-bit value
            // BT.2020: Y = ((17224*1023 + 44437*1023 + 3886*1023 + 32768) >> 16) + 64
            //          = ((65547*1023 + 32768) >> 16) + 64
            //          = ((67044381 + 32768) >> 16) + 64
            //          = 1023 + 64 but clamped to 940
            // Actually: (17224+44437+3886)*1023 = 65547*1023 = 67,044,681
            // (67,044,681 + 32768) >> 16 = 67,077,449 >> 16 = 1023
            // + 64 = 1087 clamped to 940
            assert_eq!(y10, 940, "Y sample {i} should be 940 (limited range max)");
        }
    }

    #[test]
    fn argb2101010_to_p010_known_color() {
        // Pure red: R=1023, G=0, B=0 (10-bit).
        let red: u32 = 0 | (0 << 10) | (0x3FF << 20) | (0x3 << 30);
        let mut src = Vec::new();
        for _ in 0..4 {
            src.extend_from_slice(&red.to_le_bytes());
        }
        let mut dst = Vec::new();
        scrap::argb2101010_to_p010(&src, 2, 2, &mut dst);

        // For pure red with BT.2020:
        // Y = (17224*1023 + 0 + 0 + 32768) >> 16 + 64
        //   = (17,612,152 + 32768) >> 16 + 64
        //   = 17,644,920 >> 16 + 64
        //   = 269 + 64 = 333
        let y = u16::from_le_bytes([dst[0], dst[1]]);
        let y10 = y >> 6;
        assert_eq!(y10, 333, "Y for pure red should be 333");

        // Cb should be < 512 (blue component is 0, red pulls Cb down)
        let cb = u16::from_le_bytes([dst[8], dst[9]]);
        let cb10 = cb >> 6;
        assert!(cb10 < 512, "Cb for pure red should be below neutral (512), got {cb10}");

        // Cr should be > 512 (red component pushes Cr up)
        let cr = u16::from_le_bytes([dst[10], dst[11]]);
        let cr10 = cr >> 6;
        assert!(cr10 > 512, "Cr for pure red should be above neutral (512), got {cr10}");
    }

    #[test]
    #[should_panic(expected = "width and height must be even")]
    fn argb2101010_to_p010_odd_dimensions_panics() {
        let src = vec![0u8; 3 * 3 * 4];
        let mut dst = Vec::new();
        scrap::argb2101010_to_p010(&src, 3, 3, &mut dst);
    }

    #[test]
    #[should_panic(expected = "ARGB2101010 data length must be a multiple of 4")]
    fn argb2101010_to_p010_bad_length_panics() {
        let src = vec![0u8; 5];
        let mut dst = Vec::new();
        scrap::argb2101010_to_p010(&src, 2, 2, &mut dst);
    }
}

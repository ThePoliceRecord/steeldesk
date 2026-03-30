#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(non_upper_case_globals)]
#![allow(improper_ctypes)]
#![allow(dead_code)]

include!(concat!(env!("OUT_DIR"), "/yuv_ffi.rs"));

#[cfg(not(target_os = "ios"))]
use crate::PixelBuffer;
use crate::{generate_call_macro, EncodeYuvFormat, TraitPixelBuffer};
use hbb_common::{bail, log, ResultType};

generate_call_macro!(call_yuv, false);

#[cfg(not(target_os = "ios"))]
pub fn convert_to_yuv(
    captured: &PixelBuffer,
    dst_fmt: EncodeYuvFormat,
    dst: &mut Vec<u8>,
    mid_data: &mut Vec<u8>,
) -> ResultType<()> {
    let src = captured.data();
    let src_stride = captured.stride();
    let src_pixfmt = captured.pixfmt();
    let src_width = captured.width();
    let src_height = captured.height();
    if src_width > dst_fmt.w || src_height > dst_fmt.h {
        bail!(
            "src rect > dst rect: ({src_width}, {src_height}) > ({},{})",
            dst_fmt.w,
            dst_fmt.h
        );
    }
    if src_pixfmt == crate::Pixfmt::BGRA
        || src_pixfmt == crate::Pixfmt::RGBA
        || src_pixfmt == crate::Pixfmt::RGB565LE
        || src_pixfmt == crate::Pixfmt::ARGB2101010
    {
        // stride is calculated, not real, so we need to check it
        if src_stride[0] < src_width * src_pixfmt.bytes_per_pixel() {
            bail!(
                "src_stride too small: {} < {}",
                src_stride[0],
                src_width * src_pixfmt.bytes_per_pixel()
            );
        }
        if src.len() < src_stride[0] * src_height {
            bail!(
                "wrong src len, {} < {} * {}",
                src.len(),
                src_stride[0],
                src_height
            );
        }
    }
    let align = |x: usize| (x + 63) / 64 * 64;
    let unsupported = format!(
        "unsupported pixfmt conversion: {src_pixfmt:?} -> {:?}",
        dst_fmt.pixfmt
    );

    match (src_pixfmt, dst_fmt.pixfmt) {
        (crate::Pixfmt::BGRA, crate::Pixfmt::I420)
        | (crate::Pixfmt::RGBA, crate::Pixfmt::I420)
        | (crate::Pixfmt::RGB565LE, crate::Pixfmt::I420) => {
            let dst_stride_y = dst_fmt.stride[0];
            let dst_stride_uv = dst_fmt.stride[1];
            dst.resize(dst_fmt.h * dst_stride_y * 2, 0); // waste some memory to ensure memory safety
            let dst_y = dst.as_mut_ptr();
            let dst_u = dst[dst_fmt.u..].as_mut_ptr();
            let dst_v = dst[dst_fmt.v..].as_mut_ptr();
            let f = match src_pixfmt {
                crate::Pixfmt::BGRA => ARGBToI420,
                crate::Pixfmt::RGBA => ABGRToI420,
                crate::Pixfmt::RGB565LE => RGB565ToI420,
                _ => bail!(unsupported),
            };
            call_yuv!(f(
                src.as_ptr(),
                src_stride[0] as _,
                dst_y,
                dst_stride_y as _,
                dst_u,
                dst_stride_uv as _,
                dst_v,
                dst_stride_uv as _,
                src_width as _,
                src_height as _,
            ));
        }
        (crate::Pixfmt::BGRA, crate::Pixfmt::NV12)
        | (crate::Pixfmt::RGBA, crate::Pixfmt::NV12)
        | (crate::Pixfmt::RGB565LE, crate::Pixfmt::NV12) => {
            let dst_stride_y = dst_fmt.stride[0];
            let dst_stride_uv = dst_fmt.stride[1];
            dst.resize(
                align(dst_fmt.h) * (align(dst_stride_y) + align(dst_stride_uv / 2)),
                0,
            );
            let dst_y = dst.as_mut_ptr();
            let dst_uv = dst[dst_fmt.u..].as_mut_ptr();
            let (input, input_stride) = match src_pixfmt {
                crate::Pixfmt::BGRA => (src.as_ptr(), src_stride[0]),
                crate::Pixfmt::RGBA => (src.as_ptr(), src_stride[0]),
                crate::Pixfmt::RGB565LE => {
                    let mid_stride = src_width * 4;
                    mid_data.resize(mid_stride * src_height, 0);
                    call_yuv!(RGB565ToARGB(
                        src.as_ptr(),
                        src_stride[0] as _,
                        mid_data.as_mut_ptr(),
                        mid_stride as _,
                        src_width as _,
                        src_height as _,
                    ));
                    (mid_data.as_ptr(), mid_stride)
                }
                _ => bail!(unsupported),
            };
            let f = match src_pixfmt {
                crate::Pixfmt::BGRA => ARGBToNV12,
                crate::Pixfmt::RGBA => ABGRToNV12,
                crate::Pixfmt::RGB565LE => ARGBToNV12,
                _ => bail!(unsupported),
            };
            call_yuv!(f(
                input,
                input_stride as _,
                dst_y,
                dst_stride_y as _,
                dst_uv,
                dst_stride_uv as _,
                src_width as _,
                src_height as _,
            ));
        }
        (crate::Pixfmt::BGRA, crate::Pixfmt::I444)
        | (crate::Pixfmt::RGBA, crate::Pixfmt::I444)
        | (crate::Pixfmt::RGB565LE, crate::Pixfmt::I444) => {
            let dst_stride_y = dst_fmt.stride[0];
            let dst_stride_u = dst_fmt.stride[1];
            let dst_stride_v = dst_fmt.stride[2];
            dst.resize(
                align(dst_fmt.h)
                    * (align(dst_stride_y) + align(dst_stride_u) + align(dst_stride_v)),
                0,
            );
            let dst_y = dst.as_mut_ptr();
            let dst_u = dst[dst_fmt.u..].as_mut_ptr();
            let dst_v = dst[dst_fmt.v..].as_mut_ptr();
            let (input, input_stride) = match src_pixfmt {
                crate::Pixfmt::BGRA => (src.as_ptr(), src_stride[0]),
                crate::Pixfmt::RGBA => {
                    mid_data.resize(src.len(), 0);
                    call_yuv!(ABGRToARGB(
                        src.as_ptr(),
                        src_stride[0] as _,
                        mid_data.as_mut_ptr(),
                        src_stride[0] as _,
                        src_width as _,
                        src_height as _,
                    ));
                    (mid_data.as_ptr(), src_stride[0])
                }
                crate::Pixfmt::RGB565LE => {
                    let mid_stride = src_width * 4;
                    mid_data.resize(mid_stride * src_height, 0);
                    call_yuv!(RGB565ToARGB(
                        src.as_ptr(),
                        src_stride[0] as _,
                        mid_data.as_mut_ptr(),
                        mid_stride as _,
                        src_width as _,
                        src_height as _,
                    ));
                    (mid_data.as_ptr(), mid_stride)
                }
                _ => bail!(unsupported),
            };

            call_yuv!(ARGBToI444(
                input,
                input_stride as _,
                dst_y,
                dst_stride_y as _,
                dst_u,
                dst_stride_u as _,
                dst_v,
                dst_stride_v as _,
                src_width as _,
                src_height as _,
            ));
        }
        // P010 conversion paths for HDR encoding.
        (crate::Pixfmt::BGRA, crate::Pixfmt::P010) => {
            bgra_to_p010(src, src_width, src_height, dst);
        }
        (crate::Pixfmt::ARGB2101010, crate::Pixfmt::P010) => {
            argb2101010_to_p010(src, src_width, src_height, dst);
        }
        _ => {
            bail!(unsupported);
        }
    }
    Ok(())
}

/// Convert ARGB 2:10:10:10 pixel data to 8-bit BGRA.
///
/// Each source pixel is a packed 32-bit value laid out (from MSB to LSB) as:
///
///   `[AA RRRRRRRRRR GGGGGGGGGG BBBBBBBBBB]`
///
/// i.e. 2 bits alpha, then 10 bits each for R, G, B.  The 10-bit channels are
/// down-shifted to 8-bit (keeping the top 8 of 10 bits).  Alpha is expanded
/// from 2-bit (0..3) to 8-bit (0..255).
///
/// This is the tone-mapping fallback path: when the remote captures in HDR
/// (`ARGB2101010`) but the client can only display SDR (`BGRA`), we lose
/// dynamic range but keep the spatial image.
///
/// `src` and `dst` are both densely-packed pixel arrays (stride == width * 4).
/// `dst` will be resized to match `src` length.
pub fn argb2101010_to_bgra(src: &[u8], dst: &mut Vec<u8>) {
    assert!(
        src.len() % 4 == 0,
        "ARGB2101010 data length must be a multiple of 4"
    );
    dst.resize(src.len(), 0);

    let pixel_count = src.len() / 4;
    for i in 0..pixel_count {
        let off = i * 4;
        // Read as little-endian u32.
        let packed = u32::from_le_bytes([src[off], src[off + 1], src[off + 2], src[off + 3]]);

        // macOS ARGB2101010 ('l10r') layout (little-endian):
        //   bits [9:0]   = Blue  (10 bits)
        //   bits [19:10] = Green (10 bits)
        //   bits [29:20] = Red   (10 bits)
        //   bits [31:30] = Alpha (2 bits)

        // Down-shift 10-bit → 8-bit: keep top 8 of 10 significant bits.
        // For a 10-bit value v, `(v >> 2)` maps 0..1023 → 0..255.
        let b = ((packed & 0x3FF) >> 2) as u8;
        let g = (((packed >> 10) & 0x3FF) >> 2) as u8;
        let r = (((packed >> 20) & 0x3FF) >> 2) as u8;
        // Expand 2-bit alpha → 8-bit: 0→0, 1→85, 2→170, 3→255.
        let a2 = ((packed >> 30) & 0x3) as u8;
        let a = a2 * 85;

        // Output in BGRA order.
        dst[off] = b;
        dst[off + 1] = g;
        dst[off + 2] = r;
        dst[off + 3] = a;
    }
}

/// Convert 8-bit BGRA pixel data to P010 (10-bit 4:2:0 semi-planar).
///
/// This is the "zero-extend" path: each 8-bit channel value `v` becomes
/// `(v as u16) << 2`, stored in a little-endian 16-bit word with the low 6
/// bits zero.  The resulting P010 buffer has the standard layout:
///
///   - Y plane:  `width * height` samples, each 16-bit LE
///   - UV plane: `width * (height / 2)` bytes (interleaved U, V pairs, each 16-bit LE)
///
/// `width` and `height` must both be even.  The output is written into `dst`,
/// which is resized as needed.
///
/// Color conversion uses the BT.601 matrix (same as libyuv's `ARGBToI420`):
///   Y  =  0.257*R + 0.504*G + 0.098*B + 16
///   Cb = -0.148*R - 0.291*G + 0.439*B + 128
///   Cr =  0.439*R - 0.368*G - 0.071*B + 128
/// The result is then left-shifted by 2 to fill the 10-bit range.
pub fn bgra_to_p010(src: &[u8], width: usize, height: usize, dst: &mut Vec<u8>) {
    assert!(width % 2 == 0 && height % 2 == 0, "width and height must be even");
    assert!(
        src.len() >= width * height * 4,
        "source buffer too small: {} < {}",
        src.len(),
        width * height * 4
    );

    let y_plane_size = width * height * 2; // 16-bit per sample
    let uv_plane_size = width * (height / 2) * 2; // interleaved UV, 16-bit each, half height
    dst.resize(y_plane_size + uv_plane_size, 0);

    let src_stride = width * 4;

    // Pass 1: compute Y plane and accumulate UV sums for chroma subsampling.
    let (y_dst, uv_dst) = dst.split_at_mut(y_plane_size);

    for row in 0..height {
        for col in 0..width {
            let src_off = row * src_stride + col * 4;
            let b = src[src_off] as i32;
            let g = src[src_off + 1] as i32;
            let r = src[src_off + 2] as i32;

            // BT.601 Y with rounding: Y = ((66*R + 129*G + 25*B + 128) >> 8) + 16
            let y8 = ((66 * r + 129 * g + 25 * b + 128) >> 8) + 16;
            let y8 = y8.clamp(16, 235) as u16;
            let y10 = y8 << 2;

            let y_off = (row * width + col) * 2;
            y_dst[y_off] = y10 as u8;
            y_dst[y_off + 1] = (y10 >> 8) as u8;
        }
    }

    // Pass 2: compute UV plane with 2x2 subsampling.
    for row in (0..height).step_by(2) {
        for col in (0..width).step_by(2) {
            // Average the 2x2 block of RGB values.
            let mut r_sum: i32 = 0;
            let mut g_sum: i32 = 0;
            let mut b_sum: i32 = 0;
            for dy in 0..2 {
                for dx in 0..2 {
                    let src_off = (row + dy) * src_stride + (col + dx) * 4;
                    b_sum += src[src_off] as i32;
                    g_sum += src[src_off + 1] as i32;
                    r_sum += src[src_off + 2] as i32;
                }
            }
            let r = (r_sum + 2) >> 2; // average with rounding
            let g = (g_sum + 2) >> 2;
            let b = (b_sum + 2) >> 2;

            // BT.601 Cb/Cr
            let cb8 = ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
            let cr8 = ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
            let cb8 = cb8.clamp(16, 240) as u16;
            let cr8 = cr8.clamp(16, 240) as u16;
            let cb10 = cb8 << 2;
            let cr10 = cr8 << 2;

            let uv_row = row / 2;
            let uv_col = col / 2;
            let uv_off = (uv_row * width + uv_col * 2) * 2;
            uv_dst[uv_off] = cb10 as u8;
            uv_dst[uv_off + 1] = (cb10 >> 8) as u8;
            uv_dst[uv_off + 2] = cr10 as u8;
            uv_dst[uv_off + 3] = (cr10 >> 8) as u8;
        }
    }
}

/// Convert ARGB 2:10:10:10 pixel data to P010 (10-bit 4:2:0 semi-planar).
///
/// This is the proper 10-bit path: the 10-bit R/G/B channels from the packed
/// ARGB2101010 source are converted to 10-bit Y/Cb/Cr and written directly
/// into the P010 buffer without any intermediate 8-bit truncation.
///
/// Layout of each source pixel (little-endian u32):
///   bits [9:0]   = Blue  (10 bits)
///   bits [19:10] = Green (10 bits)
///   bits [29:20] = Red   (10 bits)
///   bits [31:30] = Alpha (2 bits, ignored)
///
/// The P010 output has the standard layout:
///   - Y plane:  `width * height` samples, each 16-bit LE (10 significant bits, left-aligned)
///   - UV plane: `width * (height / 2)` bytes (interleaved U, V pairs, each 16-bit LE)
///
/// `width` and `height` must both be even.
///
/// Color conversion uses BT.2020 coefficients for HDR:
///   Y  = 0.2627*R + 0.6780*G + 0.0593*B
///   Cb = (B - Y) / 1.8814 + 512
///   Cr = (R - Y) / 1.4746 + 512
/// Scaled to fixed-point for integer arithmetic.
pub fn argb2101010_to_p010(src: &[u8], width: usize, height: usize, dst: &mut Vec<u8>) {
    assert!(width % 2 == 0 && height % 2 == 0, "width and height must be even");
    assert!(src.len() % 4 == 0, "ARGB2101010 data length must be a multiple of 4");
    assert!(
        src.len() >= width * height * 4,
        "source buffer too small: {} < {}",
        src.len(),
        width * height * 4
    );

    let y_plane_size = width * height * 2;
    let uv_plane_size = width * (height / 2) * 2;
    dst.resize(y_plane_size + uv_plane_size, 0);

    let src_stride = width * 4;

    let (y_dst, uv_dst) = dst.split_at_mut(y_plane_size);

    // Helper: unpack ARGB2101010 pixel to (R10, G10, B10).
    let unpack = |off: usize| -> (i32, i32, i32) {
        let packed = u32::from_le_bytes([src[off], src[off + 1], src[off + 2], src[off + 3]]);
        let b = (packed & 0x3FF) as i32;
        let g = ((packed >> 10) & 0x3FF) as i32;
        let r = ((packed >> 20) & 0x3FF) as i32;
        (r, g, b)
    };

    // BT.2020 non-constant luminance (NCL) coefficients, scaled by 2^16 = 65536:
    //   Y  = (0.2627*R + 0.6780*G + 0.0593*B) * (876/1023) + 64
    //   Cb = (-0.2627*R * Kb_factor ...) + 512
    //   Cr = ...
    //
    // For 10-bit full-range to limited-range:
    //   Y_10bit  = ((17224*R + 44437*G + 3886*B + 32768) >> 16) + 64
    //   Cb_10bit = ((-9440*R - 24354*G + 33794*B + 32768) >> 16) + 512
    //   Cr_10bit = ((33794*R - 30757*G - 3037*B + 32768) >> 16) + 512
    //
    // These coefficients map the 0..1023 input range to the limited range
    // Y: 64..940, Cb/Cr: 64..960 for 10-bit.

    // Y plane
    for row in 0..height {
        for col in 0..width {
            let src_off = row * src_stride + col * 4;
            let (r, g, b) = unpack(src_off);

            let y10 = ((17224 * r + 44437 * g + 3886 * b + 32768) >> 16) + 64;
            let y10 = y10.clamp(64, 940) as u16;
            // P010 stores 10-bit values left-aligned in 16-bit words (bits [15:6]).
            let y16 = y10 << 6;

            let y_off = (row * width + col) * 2;
            y_dst[y_off] = y16 as u8;
            y_dst[y_off + 1] = (y16 >> 8) as u8;
        }
    }

    // UV plane with 2x2 chroma subsampling
    for row in (0..height).step_by(2) {
        for col in (0..width).step_by(2) {
            let mut r_sum: i32 = 0;
            let mut g_sum: i32 = 0;
            let mut b_sum: i32 = 0;
            for dy in 0..2 {
                for dx in 0..2 {
                    let src_off = (row + dy) * src_stride + (col + dx) * 4;
                    let (r, g, b) = unpack(src_off);
                    r_sum += r;
                    g_sum += g;
                    b_sum += b;
                }
            }
            let r = (r_sum + 2) >> 2;
            let g = (g_sum + 2) >> 2;
            let b = (b_sum + 2) >> 2;

            let cb10 = ((-9440 * r - 24354 * g + 33794 * b + 32768) >> 16) + 512;
            let cr10 = ((33794 * r - 30757 * g - 3037 * b + 32768) >> 16) + 512;
            let cb10 = cb10.clamp(64, 960) as u16;
            let cr10 = cr10.clamp(64, 960) as u16;
            let cb16 = cb10 << 6;
            let cr16 = cr10 << 6;

            let uv_row = row / 2;
            let uv_col = col / 2;
            let uv_off = (uv_row * width + uv_col * 2) * 2;
            uv_dst[uv_off] = cb16 as u8;
            uv_dst[uv_off + 1] = (cb16 >> 8) as u8;
            uv_dst[uv_off + 2] = cr16 as u8;
            uv_dst[uv_off + 3] = (cr16 >> 8) as u8;
        }
    }
}

#[cfg(not(target_os = "ios"))]
pub fn convert(captured: &PixelBuffer, pixfmt: crate::Pixfmt, dst: &mut Vec<u8>) -> ResultType<()> {
    if captured.pixfmt() == pixfmt {
        dst.extend_from_slice(captured.data());
        return Ok(());
    }

    let src = captured.data();
    let src_stride = captured.stride();
    let src_pixfmt = captured.pixfmt();
    let src_width = captured.width();
    let src_height = captured.height();

    let unsupported = format!(
        "unsupported pixfmt conversion: {src_pixfmt:?} -> {:?}",
        pixfmt
    );

    match (src_pixfmt, pixfmt) {
        (crate::Pixfmt::BGRA, crate::Pixfmt::RGBA) | (crate::Pixfmt::RGBA, crate::Pixfmt::BGRA) => {
            dst.resize(src.len(), 0);
            call_yuv!(ABGRToARGB(
                src.as_ptr(),
                src_stride[0] as _,
                dst.as_mut_ptr(),
                src_stride[0] as _,
                src_width as _,
                src_height as _,
            ));
        }
        _ => {
            bail!(unsupported);
        }
    }
    Ok(())
}

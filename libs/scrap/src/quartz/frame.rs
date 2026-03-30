use std::{ops, ptr, slice};

use super::ffi::*;

pub struct Frame {
    surface: IOSurfaceRef,
    inner: &'static [u8],
    bgra: Vec<u8>,
    bgra_stride: usize,
}

impl Frame {
    pub unsafe fn new(surface: IOSurfaceRef) -> Frame {
        CFRetain(surface);
        IOSurfaceIncrementUseCount(surface);

        IOSurfaceLock(surface, SURFACE_LOCK_READ_ONLY, ptr::null_mut());

        let inner = slice::from_raw_parts(
            IOSurfaceGetBaseAddress(surface) as *const u8,
            IOSurfaceGetAllocSize(surface),
        );

        Frame {
            surface,
            inner,
            bgra: Vec::new(),
            bgra_stride: 0,
        }
    }

    #[inline]
    pub fn inner(&self) -> &[u8] {
        self.inner
    }

    pub fn stride(&self) -> usize {
        self.bgra_stride
    }

    pub fn surface_to_bgra<'a>(&'a mut self, h: usize) {
        unsafe {
            let plane0 = IOSurfaceGetBaseAddressOfPlane(self.surface, 0);
            self.bgra_stride = IOSurfaceGetBytesPerRowOfPlane(self.surface, 0);
            self.bgra.resize(self.bgra_stride * h, 0);
            std::ptr::copy_nonoverlapping(
                plane0 as _,
                self.bgra.as_mut_ptr(),
                self.bgra_stride * h,
            );
        }
    }

    /// Convert an ARGB2101010 (10-bit HDR) surface to 8-bit BGRA.
    ///
    /// The surface data is in packed ARGB 2:10:10:10 format (`'l10r'`).
    /// We copy it out, then convert in-place to BGRA using the
    /// `argb2101010_to_bgra` helper (right-shift 10→8 per channel).
    pub fn surface_to_bgra_from_2101010<'a>(&'a mut self, h: usize) {
        unsafe {
            let plane0 = IOSurfaceGetBaseAddressOfPlane(self.surface, 0);
            let surface_stride = IOSurfaceGetBytesPerRowOfPlane(self.surface, 0);
            let src_len = surface_stride * h;
            // Read raw 10-bit data from the surface.
            let mut raw_2101010 = vec![0u8; src_len];
            std::ptr::copy_nonoverlapping(plane0 as _, raw_2101010.as_mut_ptr(), src_len);
            // Convert to BGRA.
            crate::argb2101010_to_bgra(&raw_2101010, &mut self.bgra);
            // Stride stays the same — both formats are 4 bytes per pixel.
            self.bgra_stride = surface_stride;
        }
    }
}

impl ops::Deref for Frame {
    type Target = [u8];
    fn deref<'a>(&'a self) -> &'a [u8] {
        &self.bgra
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        unsafe {
            IOSurfaceUnlock(self.surface, SURFACE_LOCK_READ_ONLY, ptr::null_mut());

            IOSurfaceDecrementUseCount(self.surface);
            CFRelease(self.surface);
        }
    }
}

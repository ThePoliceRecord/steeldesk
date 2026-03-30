use crate::{
    common::{
        drm_capture,
        wayland,
        x11::{self},
        TraitCapturer,
    },
    Frame,
};
use std::{io, time::Duration};

pub enum Capturer {
    X11(x11::Capturer),
    WAYLAND(wayland::Capturer),
    DRM(drm_capture::DrmCapturer),
}

impl Capturer {
    pub fn new(display: Display) -> io::Result<Capturer> {
        Ok(match display {
            Display::X11(d) => Capturer::X11(x11::Capturer::new(d)?),
            Display::WAYLAND(d) => Capturer::WAYLAND(wayland::Capturer::new(d)?),
            Display::DRM(d) => Capturer::DRM(drm_capture::DrmCapturer::new(d)?),
        })
    }

    pub fn width(&self) -> usize {
        match self {
            Capturer::X11(d) => d.width(),
            Capturer::WAYLAND(d) => d.width(),
            Capturer::DRM(d) => d.width(),
        }
    }

    pub fn height(&self) -> usize {
        match self {
            Capturer::X11(d) => d.height(),
            Capturer::WAYLAND(d) => d.height(),
            Capturer::DRM(d) => d.height(),
        }
    }
}

impl TraitCapturer for Capturer {
    fn frame<'a>(&'a mut self, timeout: Duration) -> io::Result<Frame<'a>> {
        match self {
            Capturer::X11(d) => d.frame(timeout),
            Capturer::WAYLAND(d) => d.frame(timeout),
            Capturer::DRM(d) => d.frame(timeout),
        }
    }
}

pub enum Display {
    X11(x11::Display),
    WAYLAND(wayland::Display),
    DRM(drm_capture::DrmDisplay),
}

impl Display {
    pub fn primary() -> io::Result<Display> {
        // Check if DRM capture should be used (login screen / headless).
        // DRM is tried first because at the login screen neither X11 nor
        // Wayland portal is available.
        if should_use_drm() {
            if let Ok(d) = drm_capture::DrmDisplay::primary() {
                return Ok(Display::DRM(d));
            }
            // Fall through to X11/Wayland if DRM fails (e.g. no active CRTC)
        }

        Ok(if super::is_x11() {
            Display::X11(x11::Display::primary()?)
        } else {
            Display::WAYLAND(wayland::Display::primary()?)
        })
    }

    // Currently, wayland need to call wayland::clear() before call Display::all()
    pub fn all() -> io::Result<Vec<Display>> {
        // If DRM mode is active, enumerate DRM displays.
        if should_use_drm() {
            let drm_displays = drm_capture::DrmDisplay::all()?;
            if !drm_displays.is_empty() {
                return Ok(drm_displays
                    .into_iter()
                    .map(|x| Display::DRM(x))
                    .collect());
            }
            // Fall through if no DRM displays found
        }

        Ok(if super::is_x11() {
            x11::Display::all()?
                .drain(..)
                .map(|x| Display::X11(x))
                .collect()
        } else {
            wayland::Display::all()?
                .drain(..)
                .map(|x| Display::WAYLAND(x))
                .collect()
        })
    }

    pub fn width(&self) -> usize {
        match self {
            Display::X11(d) => d.width(),
            Display::WAYLAND(d) => d.width(),
            Display::DRM(d) => d.width(),
        }
    }

    pub fn height(&self) -> usize {
        match self {
            Display::X11(d) => d.height(),
            Display::WAYLAND(d) => d.height(),
            Display::DRM(d) => d.height(),
        }
    }

    pub fn scale(&self) -> f64 {
        match self {
            Display::X11(_d) => 1.0,
            Display::WAYLAND(d) => d.scale(),
            Display::DRM(d) => d.scale(),
        }
    }

    pub fn logical_width(&self) -> usize {
        match self {
            Display::X11(d) => d.width(),
            Display::WAYLAND(d) => d.logical_width(),
            Display::DRM(d) => d.logical_width(),
        }
    }

    pub fn logical_height(&self) -> usize {
        match self {
            Display::X11(d) => d.height(),
            Display::WAYLAND(d) => d.logical_height(),
            Display::DRM(d) => d.logical_height(),
        }
    }

    pub fn origin(&self) -> (i32, i32) {
        match self {
            Display::X11(d) => d.origin(),
            Display::WAYLAND(d) => d.origin(),
            Display::DRM(d) => d.origin(),
        }
    }

    pub fn is_online(&self) -> bool {
        match self {
            Display::X11(d) => d.is_online(),
            Display::WAYLAND(d) => d.is_online(),
            Display::DRM(d) => d.is_online(),
        }
    }

    pub fn is_primary(&self) -> bool {
        match self {
            Display::X11(d) => d.is_primary(),
            Display::WAYLAND(d) => d.is_primary(),
            Display::DRM(d) => d.is_primary(),
        }
    }

    pub fn name(&self) -> String {
        match self {
            Display::X11(d) => d.name(),
            Display::WAYLAND(d) => d.name(),
            Display::DRM(d) => d.name(),
        }
    }
}

/// Check if DRM capture should be used instead of X11/PipeWire.
///
/// DRM capture is appropriate when:
/// 1. At the login screen (no user session, no portal)
/// 2. Headless with no compositor (no DISPLAY or WAYLAND_DISPLAY)
/// 3. A DRM device is accessible
///
/// This function is intentionally conservative — it only returns true when
/// we are confident that portal-based capture cannot work.
fn should_use_drm() -> bool {
    // Quick check: is a DRM device even present?
    if !drm_capture::DrmCapture::available() {
        return false;
    }

    // Check for headless: no display server at all.
    let no_display = std::env::var("DISPLAY").is_err();
    let no_wayland = std::env::var("WAYLAND_DISPLAY").is_err();

    if no_display && no_wayland {
        // No compositor is reachable — DRM is the only option.
        return true;
    }

    // If we're not in a headless state, we could still be at the login screen.
    // The login screen detection lives in the main crate (platform::linux),
    // which the scrap library cannot directly call.  Instead, we check the
    // `STEELDESK_DRM_CAPTURE` env var, which the service sets when it
    // detects a login screen.
    //
    // This avoids a circular dependency between scrap and the main crate.
    if std::env::var("STEELDESK_DRM_CAPTURE").unwrap_or_default() == "1" {
        return true;
    }

    false
}

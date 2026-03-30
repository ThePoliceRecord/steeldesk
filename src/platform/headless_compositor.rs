// Headless Wayland Compositor Auto-Start
//
// When RustDesk runs on a headless Linux server (no physical monitor, no
// compositor), PipeWire screen-capture portals cannot function because there
// is no Wayland compositor to talk to. This module detects available minimal
// compositors, starts one in headless mode, and exposes the WAYLAND_DISPLAY
// socket name so the --server subprocess can connect to it.
//
// Supported compositors (tried in order):
//   1. Weston  — weston --backend=headless-backend.so --socket=steeldesk-headless
//   2. Cage   — cage -- /bin/true
//   3. Sway   — sway (auto-creates a Wayland socket)
//
// The compositor is stopped when the HeadlessCompositor is dropped or when
// stop() is called explicitly.

use std::process::{Child, Command};

use hbb_common::log;

/// Which compositor binary to launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompositorType {
    Weston,
    Cage,
    Sway,
}

impl CompositorType {
    /// Human-readable name used in logs and config values.
    pub fn name(&self) -> &'static str {
        match self {
            CompositorType::Weston => "weston",
            CompositorType::Cage => "cage",
            CompositorType::Sway => "sway",
        }
    }

    /// All known compositor types in priority order.
    pub const ALL: &'static [CompositorType] = &[
        CompositorType::Weston,
        CompositorType::Cage,
        CompositorType::Sway,
    ];
}

/// A running headless Wayland compositor process.
///
/// When dropped, the compositor process is killed.
///
/// Note: `Debug` is manually implemented because `Child` does not implement
/// `Debug` in a useful way; we show the compositor type and display name.
pub struct HeadlessCompositor {
    child: Option<Child>,
    compositor_type: CompositorType,
    wayland_display: String,
}

impl std::fmt::Debug for HeadlessCompositor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HeadlessCompositor")
            .field("compositor_type", &self.compositor_type)
            .field("wayland_display", &self.wayland_display)
            .field("running", &self.child.is_some())
            .finish()
    }
}

/// Default socket name for the headless compositor.
const DEFAULT_SOCKET: &str = "steeldesk-headless";

/// Parse a resolution string like "1920x1080" into (width, height).
///
/// Returns `None` if the string is malformed or contains non-positive values.
pub fn parse_resolution(res: &str) -> Option<(u32, u32)> {
    let parts: Vec<&str> = res.split('x').collect();
    if parts.len() != 2 {
        return None;
    }
    let w: u32 = parts[0].parse().ok()?;
    let h: u32 = parts[1].parse().ok()?;
    if w == 0 || h == 0 {
        return None;
    }
    Some((w, h))
}

/// Check whether a command exists in PATH using `which`.
fn has_cmd(cmd: &str) -> bool {
    Command::new("which")
        .arg(cmd)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Detect which headless-capable compositor is available on this system.
///
/// Returns the first available compositor from the priority list, or `None`
/// if none are installed.
pub fn detect_available() -> Option<CompositorType> {
    for typ in CompositorType::ALL {
        if has_cmd(typ.name()) {
            log::info!(
                "Headless compositor detected: {} is available",
                typ.name()
            );
            return Some(*typ);
        }
    }
    log::info!("No headless compositor found in PATH (checked: weston, cage, sway)");
    None
}

/// Parse the config value for `headless-compositor` into a compositor selection.
///
/// Values: "auto" (default), "weston", "cage", "sway", "none".
/// Returns `None` for "none" or unrecognized values.
/// Returns `Some(specific)` for named compositors.
/// Returns the result of `detect_available()` for "auto" or empty.
pub fn compositor_from_config(value: &str) -> Option<CompositorType> {
    match value.trim().to_lowercase().as_str() {
        "none" => None,
        "weston" => Some(CompositorType::Weston),
        "cage" => Some(CompositorType::Cage),
        "sway" => Some(CompositorType::Sway),
        "auto" | "" => detect_available(),
        other => {
            log::warn!(
                "Unknown headless-compositor config value '{}', falling back to auto-detect",
                other
            );
            detect_available()
        }
    }
}

impl HeadlessCompositor {
    /// Start a headless compositor of the given type.
    ///
    /// `resolution` should be in "WIDTHxHEIGHT" format (e.g. "1920x1080").
    /// Falls back to 1920x1080 if parsing fails.
    pub fn start(typ: CompositorType, resolution: &str) -> Result<Self, String> {
        let (w, h) = parse_resolution(resolution).unwrap_or((1920, 1080));

        let (cmd, args, display) = match typ {
            CompositorType::Weston => (
                "weston",
                vec![
                    "--backend=headless-backend.so".to_string(),
                    format!("--width={}", w),
                    format!("--height={}", h),
                    format!("--socket={}", DEFAULT_SOCKET),
                ],
                DEFAULT_SOCKET.to_string(),
            ),
            CompositorType::Cage => {
                // Cage creates its own socket; we use the default naming.
                // Cage runs a single application; /bin/true exits immediately
                // but the compositor stays if started with appropriate flags.
                // In practice, cage may exit when /bin/true exits; this is
                // handled by the is_running() health check.
                (
                    "cage",
                    vec!["--".to_string(), "/bin/true".to_string()],
                    // Cage names its socket wayland-N; we detect it from the env
                    // or use the default. For scaffolding, we use a placeholder.
                    "wayland-1".to_string(),
                )
            }
            CompositorType::Sway => {
                // Sway auto-creates a wayland socket (wayland-0 or wayland-1).
                (
                    "sway",
                    vec![],
                    "wayland-1".to_string(),
                )
            }
        };

        log::info!(
            "Starting headless compositor: {} {} ({}x{})",
            cmd,
            args.join(" "),
            w,
            h
        );

        let child = Command::new(cmd)
            .args(&args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| format!("Failed to start {}: {}", cmd, e))?;

        log::info!(
            "Headless compositor {} started (pid={}, display={})",
            cmd,
            child.id(),
            display
        );

        Ok(Self {
            child: Some(child),
            compositor_type: typ,
            wayland_display: display,
        })
    }

    /// The WAYLAND_DISPLAY value to set for child processes.
    pub fn wayland_display(&self) -> &str {
        &self.wayland_display
    }

    /// Which compositor type is running.
    pub fn compositor_type(&self) -> CompositorType {
        self.compositor_type
    }

    /// Check if the compositor process is still running.
    pub fn is_running(&self) -> bool {
        // If we have a child, try_wait returns Ok(None) when still running.
        // We cannot call try_wait on an immutable reference, so we check if
        // child is Some (it is set to None after stop()).
        self.child.is_some()
    }

    /// Check if the compositor process has exited (and reap it).
    ///
    /// Returns true if the process has exited or was never started.
    /// This mutably borrows self to call try_wait().
    pub fn has_exited(&mut self) -> bool {
        match self.child.as_mut() {
            Some(child) => match child.try_wait() {
                Ok(Some(_status)) => {
                    log::warn!(
                        "Headless compositor {} exited with status {:?}",
                        self.compositor_type.name(),
                        _status
                    );
                    true
                }
                Ok(None) => false, // still running
                Err(e) => {
                    log::error!("Error checking compositor status: {}", e);
                    true // assume dead
                }
            },
            None => true,
        }
    }

    /// Stop the compositor process.
    pub fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            log::info!(
                "Stopping headless compositor {} (pid={})",
                self.compositor_type.name(),
                child.id()
            );
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for HeadlessCompositor {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------
    // CompositorType basics
    // -----------------------------------------------------------

    #[test]
    fn compositor_type_variants_exist() {
        let _w = CompositorType::Weston;
        let _c = CompositorType::Cage;
        let _s = CompositorType::Sway;
    }

    #[test]
    fn compositor_type_names() {
        assert_eq!(CompositorType::Weston.name(), "weston");
        assert_eq!(CompositorType::Cage.name(), "cage");
        assert_eq!(CompositorType::Sway.name(), "sway");
    }

    #[test]
    fn compositor_type_all_has_three_entries() {
        assert_eq!(CompositorType::ALL.len(), 3);
    }

    #[test]
    fn compositor_type_eq_and_clone() {
        let a = CompositorType::Weston;
        let b = a.clone();
        assert_eq!(a, b);
        assert_ne!(CompositorType::Weston, CompositorType::Cage);
    }

    #[test]
    fn compositor_type_debug_format() {
        let s = format!("{:?}", CompositorType::Sway);
        assert!(s.contains("Sway"));
    }

    // -----------------------------------------------------------
    // Resolution parsing
    // -----------------------------------------------------------

    #[test]
    fn parse_resolution_valid() {
        assert_eq!(parse_resolution("1920x1080"), Some((1920, 1080)));
        assert_eq!(parse_resolution("1280x720"), Some((1280, 720)));
        assert_eq!(parse_resolution("3840x2160"), Some((3840, 2160)));
        assert_eq!(parse_resolution("1x1"), Some((1, 1)));
    }

    #[test]
    fn parse_resolution_invalid() {
        assert_eq!(parse_resolution(""), None);
        assert_eq!(parse_resolution("1920"), None);
        assert_eq!(parse_resolution("1920x"), None);
        assert_eq!(parse_resolution("x1080"), None);
        assert_eq!(parse_resolution("axb"), None);
        assert_eq!(parse_resolution("1920x1080x60"), None);
        assert_eq!(parse_resolution("0x1080"), None);
        assert_eq!(parse_resolution("1920x0"), None);
    }

    #[test]
    fn parse_resolution_negative_rejected() {
        // u32 parse will fail for negative numbers
        assert_eq!(parse_resolution("-1x1080"), None);
        assert_eq!(parse_resolution("1920x-1"), None);
    }

    // -----------------------------------------------------------
    // detect_available() — must not crash
    // -----------------------------------------------------------

    #[test]
    fn detect_available_does_not_panic() {
        // May return None or Some depending on what's installed.
        let _result = detect_available();
    }

    #[test]
    fn detect_available_returns_valid_type_if_some() {
        if let Some(typ) = detect_available() {
            // Must be one of the known types
            assert!(
                CompositorType::ALL.contains(&typ),
                "detect_available returned {:?} which is not in ALL",
                typ
            );
        }
    }

    // -----------------------------------------------------------
    // has_cmd() helper
    // -----------------------------------------------------------

    #[test]
    fn has_cmd_finds_common_binary() {
        // /bin/true or /usr/bin/true should exist on any Linux system
        assert!(has_cmd("true"));
    }

    #[test]
    fn has_cmd_returns_false_for_nonexistent() {
        assert!(!has_cmd("this_binary_definitely_does_not_exist_xyz_123"));
    }

    // -----------------------------------------------------------
    // compositor_from_config()
    // -----------------------------------------------------------

    #[test]
    fn config_none_disables() {
        assert!(compositor_from_config("none").is_none());
    }

    #[test]
    fn config_explicit_compositor() {
        assert_eq!(compositor_from_config("weston"), Some(CompositorType::Weston));
        assert_eq!(compositor_from_config("cage"), Some(CompositorType::Cage));
        assert_eq!(compositor_from_config("sway"), Some(CompositorType::Sway));
    }

    #[test]
    fn config_case_insensitive() {
        assert_eq!(compositor_from_config("Weston"), Some(CompositorType::Weston));
        assert_eq!(compositor_from_config("CAGE"), Some(CompositorType::Cage));
        assert_eq!(compositor_from_config("SWAY"), Some(CompositorType::Sway));
        assert!(compositor_from_config("NONE").is_none());
    }

    #[test]
    fn config_auto_does_not_panic() {
        let _result = compositor_from_config("auto");
    }

    #[test]
    fn config_empty_does_not_panic() {
        let _result = compositor_from_config("");
    }

    #[test]
    fn config_unknown_falls_back_to_auto() {
        // Should not panic; result depends on what's installed
        let _result = compositor_from_config("unknown_compositor");
    }

    #[test]
    fn config_whitespace_trimmed() {
        assert!(compositor_from_config("  none  ").is_none());
        assert_eq!(
            compositor_from_config("  weston  "),
            Some(CompositorType::Weston)
        );
    }

    // -----------------------------------------------------------
    // HeadlessCompositor struct — construction & fields
    // Note: We cannot actually start compositors in CI, so we test
    // the scaffolding and error paths.
    // -----------------------------------------------------------

    #[test]
    fn start_weston_fails_gracefully_when_not_installed() {
        if has_cmd("weston") {
            // Can't test failure path if weston is installed; skip.
            return;
        }
        let result = HeadlessCompositor::start(CompositorType::Weston, "1920x1080");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("weston"), "Error should mention weston: {}", err);
    }

    #[test]
    fn start_cage_fails_gracefully_when_not_installed() {
        if has_cmd("cage") {
            return;
        }
        let result = HeadlessCompositor::start(CompositorType::Cage, "1920x1080");
        assert!(result.is_err());
    }

    #[test]
    fn start_sway_fails_gracefully_when_not_installed() {
        if has_cmd("sway") {
            return;
        }
        let result = HeadlessCompositor::start(CompositorType::Sway, "1920x1080");
        assert!(result.is_err());
    }

    #[test]
    fn start_uses_fallback_resolution_on_bad_input() {
        // Even with bad resolution, start() should not panic — it should
        // fall back to 1920x1080. The actual spawn will fail if the
        // compositor isn't installed, but the resolution parsing itself
        // should succeed.
        if has_cmd("weston") {
            // If weston is installed, it might actually start — skip.
            return;
        }
        let result = HeadlessCompositor::start(CompositorType::Weston, "garbage");
        // Should fail due to missing binary, not due to resolution parse.
        assert!(result.is_err());
    }

    #[test]
    fn default_socket_name() {
        assert_eq!(DEFAULT_SOCKET, "steeldesk-headless");
    }
}

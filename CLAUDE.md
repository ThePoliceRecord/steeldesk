# CLAUDE.md — RustDesk Client

## Quick Start

```bash
nix develop                                    # Enter dev shell (all deps via nixpkgs)
git submodule update --init --recursive        # Init hbb_common
cargo build --features linux-pkg-config        # Build with system libs (no vcpkg)
cargo test --features linux-pkg-config         # Run tests
cargo build --features linux-pkg-config,flutter # With Flutter UI
```

The flake mirrors the official nixpkgs `rustdesk` package (`pkgs/by-name/ru/rustdesk/package.nix`).
Key: use `--features linux-pkg-config` to use nix-provided system libs instead of vcpkg.

Without nix, install deps manually: see "System Dependencies" below.

### How nixpkgs builds RustDesk

Reference: `nix eval --raw nixpkgs#rustdesk.meta.position` → `pkgs/by-name/ru/rustdesk/package.nix`

Key details from the official nix package:
- Uses `rustPlatform.bindgenHook` for LIBCLANG_PATH (not manual env vars)
- Uses `linux-pkg-config` feature flag (no vcpkg needed)
- Sets `SODIUM_USE_PKG_CONFIG=true` and `ZSTD_SYS_USE_PKG_CONFIG=true`
- Patches webm-sys to add `#include <cstdint>` for GCC 13+ compat
- Disables tests (`doCheck = false` — they require X server)
- Links `libsciter-gtk.so` next to binary for legacy UI
- Uses `libayatana-appindicator` (not `libappindicator-gtk3`)

## Build Commands

| Command | What |
|---|---|
| `cargo build` | Debug Rust binary |
| `cargo build --release` | Release binary |
| `cargo build --features flutter` | Flutter UI |
| `cargo build --features hwcodec` | Hardware codec |
| `cargo build --features vram` | VRAM path (Windows) |
| `cargo build --features unix-file-copy-paste` | File clipboard (Linux) |
| `cargo build --features screencapturekit` | ScreenCaptureKit (macOS) |
| `python3 build.py --flutter` | Full Flutter build |
| `python3 build.py --flutter --release` | Release Flutter build |
| `cargo test` | Rust tests |
| `cd flutter && flutter test` | Flutter tests (mostly absent) |
| `cd flutter && flutter run` | Run Flutter dev |

## Architecture

```
src/
├── client.rs                 # Peer connection, video/audio streaming (152KB)
├── server.rs                 # Local service provisioning
├── server/
│   ├── connection.rs         # Main connection handler, auth, permissions (235KB)
│   ├── video_service.rs      # Screen capture loop, encoding, frame control
│   ├── video_qos.rs          # FPS/bitrate adaptation, RTT calculation
│   ├── audio_service.rs      # Opus encoding, low-delay audio
│   ├── input_service.rs      # Keyboard/mouse simulation
│   └── terminal_service.rs   # Remote shell access
├── rendezvous_mediator.rs    # Server communication, NAT traversal
├── hbbs_http/                # HTTP API client (account, sync, audit, recording)
├── auth_2fa.rs               # TOTP 2FA implementation
├── flutter_ffi.rs            # FFI bridge to Flutter UI (96KB)
├── ipc.rs                    # Inter-process communication (58KB)
├── platform/                 # Windows/Linux/macOS platform code
└── plugin/                   # Plugin framework

flutter/
├── lib/desktop/              # Desktop UI pages
├── lib/mobile/               # Mobile UI pages
├── lib/common/widgets/       # Shared widgets
├── lib/models/               # State management (ab_model, group_model, user_model, etc.)
└── lib/web/                  # Web bridge

libs/
├── hbb_common/               # Shared: config, network, protobuf, crypto (git submodule)
├── scrap/                    # Screen capture (DXGI, ScreenCaptureKit, X11, PipeWire)
├── enigo/                    # Input simulation
├── clipboard/                # Cross-platform clipboard
├── virtual_display/          # Virtual display drivers
└── remote_printer/           # Remote printing
```

## Key Subsystems

**Video pipeline:** `video_service.rs` → `libs/scrap/` (capture) → `codec.rs`/`hwcodec.rs` (encode) → network → `client.rs` (decode) → `flutter.rs` (render)

**QoS:** `video_qos.rs` — adaptive FPS (1-120), bitrate ratio, RTT calculation. Adjusts every 1-3 seconds. INIT_FPS=15, ramps up based on network delay thresholds.

**Auth flow:** `connection.rs` — password validation → optional 2FA → session creation. `LOGIN_FAILURES` tracks brute force (in-memory only). Trusted devices bypass 2FA via HWID match.

**Pro detection:** `hbbs_http/sync.rs` → `is_pro()` flag from heartbeat. Gates: address book, groups, strategies, CM hiding.

**Transport:** TCP primary, KCP (UDP) optional. `rendezvous_mediator.rs` handles P2P establishment, relay fallback. TCP_NODELAY enabled everywhere.

## Feature Flags

| Flag | What |
|---|---|
| `flutter` | Flutter UI (required for modern UI) |
| `hwcodec` | Hardware video codec (NVENC, QSV, VAAPI, VideoToolbox) |
| `vram` | GPU texture path, Windows only |
| `unix-file-copy-paste` | File clipboard on Unix |
| `screencapturekit` | macOS ScreenCaptureKit |
| `cli` | CLI interface |
| `inline` | Sciter inline (legacy) |

## Config System

4 config types in `libs/hbb_common/src/config.rs`:
- **Settings** — user preferences
- **Local** — per-peer settings
- **Display** — display-specific settings
- **Built-in** — compile-time defaults

Runtime feature gates: `isDisableAccount()`, `isDisableAb()`, `isDisableGroupPanel()`, `isDisableSettings()`

## System Dependencies (Linux, without nix)

```bash
# Build tools
apt install build-essential clang cmake ninja-build nasm pkg-config python3 git

# Audio
apt install libasound2-dev libpulse-dev

# Video / HW accel
apt install libva-dev libvdpau-dev libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev

# X11 / GTK
apt install libgtk-3-dev libxcb-randr0-dev libxcb-shape0-dev libxcb-xfixes0-dev libxdo-dev libxfixes-dev

# System
apt install libpam0g-dev libssl-dev libclang-dev libayatana-appindicator3-dev
```

Plus vcpkg for: `libvpx`, `libyuv`, `opus`, `aom`. Set `VCPKG_ROOT`.

**NASM must be 2.x, NOT 3.x** (breaks aom/ffmpeg assembly).

## Testing

**826 tests**, all passing (1 pre-existing X11 cursor test needs display). Run with:
```bash
nix develop --command bash -c 'cargo test --lib --features linux-pkg-config -p rustdesk'
```

Coverage: video_qos (72), video_service (55), input_service (106), connection (53), keyboard (76), ipc (93), client (83), common (47), auth_2fa (28), custom_server (20), rendezvous_mediator (35), platform/linux (67), transport (50), gpu_pipeline (8), ei_input (11).

## Performance Fixes Applied

- Frame ACK blocking: 3s→500ms normal, 0ms in `low-latency-mode=Y`
- HW codec: dynamic FPS from QoS (was hardcoded 30)
- Gaming mode: INIT_FPS cap bypassed, +10 ramp-up, queue=3
- QoS adaptation: 3s→1s interval, 10→5 RTT samples in gaming mode
- Repeat encoding: skipped in gaming mode
- NVENC: low-latency quality preset wired (hwcodec TODO for full support)

## Security Fixes Applied

- IPC socket: 0o777→0o600
- Protocol downgrade: connection refused on key mismatch (was silent cleartext fallback)
- TLS: auto-retry with invalid certs removed
- Port forward: SSRF blocked (cloud metadata, link-local, public IPs)
- TOTP: machine-derived encryption key (was hardcoded "00"), backward-compatible migration
- Path traversal: `../` and null bytes blocked in file operations
- See `docs/security-review.md` for full 55 findings

## New Subsystems

- **Transport layer** (`src/transport/`): trait abstraction, TCP impl, FEC with XOR parity recovery, QUIC scaffold
- **GPU pipeline** (`src/server/gpu_pipeline.rs`): capability detection, pipeline mode enum
- **libei input** (`src/server/ei_input.rs`): scaffolded behind `libei` feature flag, fallback to clipboard hack

## Ignore Patterns

- `target/` — Rust build artifacts
- `flutter/build/` — Flutter build output
- `flutter/.dart_tool/` — Flutter tooling
- `libs/hbb_common/` — git submodule (don't edit in-tree)

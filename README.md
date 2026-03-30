# SteelDesk

Open-source remote desktop built for gaming, video production, and enterprise management. Fork of [RustDesk](https://github.com/rustdesk/rustdesk) with Pro features, security hardening, performance optimizations, and QUIC transport.

## What's Different from RustDesk

### Performance (Gaming/Studio Mode)
- **Low-latency mode** (`low-latency-mode=Y`) — optimized for remote gaming and video work
- Frame ACK blocking eliminated (was 3 seconds, now 0ms in gaming mode)
- Hardware codec FPS uncapped (was hardcoded 30fps, now dynamic up to 240fps)
- Video queue reduced from 120 frames (4 seconds!) to 3 frames in gaming mode
- QoS adaptation 3x faster (1s interval vs 3s)
- Repeat encoding of stale frames skipped
- NVENC low-latency quality preset
- Client-side cursor prediction with snap/lerp server reconciliation
- Decoupled capture/encode via frame buffer

### Security (9 Fixes)
- IPC socket permissions: 0o777 → 0o600
- Protocol downgrade to cleartext refused (was silent fallback)
- TLS auto-retry with invalid certs removed
- Port forward SSRF blocked (cloud metadata, link-local, public IPs)
- TOTP secret encryption with machine-derived key (was hardcoded "00")
- Path traversal blocked in file operations

### Transport
- **QUIC transport** via quinn — BBR congestion control, unreliable datagrams for video, reliable streams for control, TLS 1.3 built-in
- **FEC** — XOR parity with single-packet-loss recovery, frame fragmentation/reassembly
- **Transport trait** abstraction — TCP and QUIC backends, automatic fallback
- Full QUIC session integration — video/audio/control flows over QUIC when enabled

### Testing
- **889 tests** (from ~35 in upstream)
- **14 criterion benchmarks** for performance regression detection

## Quick Start

```bash
# With Nix (recommended)
nix develop
git submodule update --init --recursive
cargo build --features linux-pkg-config
cargo test --features linux-pkg-config

# Gaming mode — set in config:
# low-latency-mode=Y

# QUIC transport (experimental)
cargo build --features linux-pkg-config,quic-transport
# Set in config: prefer-quic=Y
```

### System Dependencies (without Nix)

```bash
apt install build-essential clang cmake ninja-build nasm pkg-config python3 git perl
apt install libasound2-dev libpulse-dev
apt install libva-dev libvdpau-dev libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev
apt install libgtk-3-dev libxcb-randr0-dev libxcb-shape0-dev libxcb-xfixes0-dev libxdo-dev libxfixes-dev libxtst-dev
apt install libpam0g-dev libssl-dev libclang-dev libayatana-appindicator3-dev
```

## Feature Flags

| Flag | What |
|---|---|
| `linux-pkg-config` | Use system libs via pkg-config (no vcpkg) |
| `quic-transport` | Enable QUIC transport via quinn |
| `libei` | Enable libei input for Wayland |
| `flutter` | Flutter UI |
| `hwcodec` | Hardware video codec (NVENC, QSV, VAAPI) |

## Documentation

See `docs/` for 17 documentation files covering performance, security, Wayland, HDR, unattended access, Pro features, and architecture plans.

## Benchmark Results

| Component | Latency |
|---|---|
| QoS network delay processing | 215 ns |
| Cursor prediction (mouse move) | 41 ns |
| FEC fragment 64KB frame | 3.5 µs (17 GiB/s) |
| FEC header serialize | 5.2 ns |
| Frame buffer store 1080p | 401 µs |

## License

AGPL-3.0 — same as upstream RustDesk.

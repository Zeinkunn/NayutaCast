# AGENTS.md - Nayutacast Project Context

## Project Overview

**Nayutacast** is a cross-platform screen casting library implemented in Rust. It provides low-latency (80-250ms) screen mirroring capabilities across Windows, macOS, Linux, and Android.

### Key Characteristics
- **Language**: Rust (primary), Kotlin (Android SDK)
- **License**: LGPL-2.1-only
- **Repository**: https://github.com/nayutacast/nayutacast
- **Current Version**: 0.4.0 (SDK), 0.3.1 (modules)

## Architecture

```
nayutacast/
├── common/          # Shared types, utilities, codec enums
├── codec/           # Video/Audio encoding & decoding (FFmpeg)
├── capture/         # Screen, camera, audio capture (platform-specific)
├── renderer/        # GPU rendering with wgpu
├── transport/       # Network transport (SRT, UDP multicast)
├── discovery/       # Service discovery with libp2p
├── resample/        # Audio resampling
├── nayutacast/      # Main SDK (high-level API)
├── server/          # Relay server
├── examples/        # CLI sender/receiver example
├── android/         # Android SDK (Kotlin + JNI)
└── app/             # Desktop application (CEF + winit)
```

## Core Modules

### 1. capture - Media Capture
Platform-specific implementations for capturing screen, audio, and camera.

| Platform | Screen Capture | Audio Capture | Camera |
|----------|---------------|---------------|--------|
| Windows | windows-capture (D3D11) | cpal | - |
| macOS | ScreenCaptureKit | core-media | - |
| Linux | **PipeWire / X11** | **PipeWire** | **V4L2** |
| Android | - | - | - |

**Key files for Linux:**
- `capture/src/linux/pipewire_screen.rs` - PipeWire screen capture (Wayland)
- `capture/src/linux/x11_screen.rs` - X11/XRandR screen capture (X11 fallback)
- `capture/src/linux/pipewire_audio.rs` - PipeWire audio capture
- `capture/src/linux/v4l2_camera.rs` - V4L2 camera capture

### 2. codec - Video/Audio Codecs
FFmpeg-based encoding and decoding with hardware acceleration.

**Video Encoders:**
- `VideoEncoderType::X264` - Software H264
- `VideoEncoderType::X265` - Software H265/HEVC
- `VideoEncoderType::Nvenc` - NVIDIA NVENC (Windows)
- `VideoEncoderType::Qsv` - Intel Quick Sync (Windows)

**Video Decoders:**
- `VideoDecoderType::H264` - Software H264
- `VideoDecoderType::D3D11` - Direct3D 11 VA (Windows)
- `VideoDecoderType::Qsv` - Intel Quick Sync (Windows)
- `VideoDecoderType::VideoToolBox` - macOS VideoToolBox

**Linux Hardware Acceleration (planned):**
- VAAPI for Intel/AMD GPUs
- NVENC/NVDEC for NVIDIA GPUs

### 3. transport - Network Layer
Three transport strategies for different use cases:

| Strategy | Description | Use Case |
|----------|-------------|----------|
| `Direct` | SRT server, receiver connects directly | 1-to-1, low latency |
| `Relay` | Data through relay server | NAT traversal, multi-network |
| `Multicast` | UDP multicast | 1-to-many, bandwidth efficient |

### 4. renderer - GPU Rendering
wgpu-based rendering supporting RGBA, BGRA, NV12, I420 formats.

### 5. discovery - Service Discovery
libp2p-based peer discovery using mDNS and Gossipsub.

## Main SDK API

```rust
use nayutacast::{
    startup, shutdown,
    create_sender, create_receiver,
    NayutacastSender, NayutacastReceiver,
    DiscoveryService, DiscoveryObserver,
    TransportOptions, TransportStrategy,
    VideoEncoderType, VideoDecoderType,
    NayutacastSenderOptions, NayutacastReceiverOptions,
};

// Initialize
startup()?;

// Create sender (screen capture)
let sender = create_sender(
    &NayutacastSenderOptions { ... },
    view,
    observer,
)?;

// Create receiver
let receiver = create_receiver(
    &description,
    &NayutacastReceiverOptions { ... },
    view,
    observer,
)?;

// Cleanup
shutdown()?;
```

## Transport Strategies

```rust
// Direct mode - sender as SRT server
TransportStrategy::Direct("0.0.0.0:8080".parse().unwrap())

// Relay mode - through relay server
TransportStrategy::Relay("192.168.1.100:9000".parse().unwrap())

// Multicast mode - UDP multicast
TransportStrategy::Multicast("239.0.0.1:8080".parse().unwrap())
```

## Building & Remote Fast Compilation

### Remote Compilation Server (`nayuta`)
For fast builds, remote compilation is configured on server `nayuta` (16 vCPUs):
- SSH host alias: `ssh nayuta` (configured via Cloudflare Tunnel `ssh-nayuta.space.my.id`)
- Server workspace path: `/home/nayuta/nayutacast`
- Sync local to server: `rsync -avz --delete --exclude 'target' --exclude '.git' ./ nayuta:/home/nayuta/nayutacast/`
- Build command on server: `ssh nayuta "cd /home/nayuta/nayutacast && cargo build --workspace -j 16"`

### Prerequisites

**All platforms:**
- Rust 1.85+
- FFmpeg development libraries

**Linux:**
```bash
# Ubuntu/Debian
sudo apt install build-essential pkg-config clang cmake tclsh \
                 libpipewire-0.3-dev libva-dev libx11-dev libxrandr-dev \
                 libxfixes-dev libxdamage-dev libv4l-dev libasound2-dev libglib2.0-dev \
                 libavcodec-dev libavutil-dev libavdevice-dev libswresample-dev
```

**Windows:**
- Visual Studio Build Tools
- Windows SDK

**macOS:**
- Xcode Command Line Tools
- `brew install cmake ffmpeg@7`

### Build Commands

```bash
# Check compilation
cargo check

# Build all
cargo build --release

# Build specific crate
cargo build -p nayutacast-capture

# Run tests
cargo test

# Format code
cargo +nightly fmt
```

## Current Development Status

### Linux Support

**Completed:**
- [x] Project structure for Linux capture modules
- [x] PipeWire screen capture (XDG Desktop Portal screencast integration & fallback loop)
- [x] X11 screen capture with MIT-SHM high performance zero-copy capture
- [x] Linux audio capture (system monitor & microphone via cpal and native resampler)
- [x] V4L2 camera capture (memory-mapped buffer streaming & YUYV to NV12)
- [x] Multi-monitor detection and selection (`--monitor`)
- [x] Audio source selection (`--audio-source`)
- [x] VAAPI hardware acceleration integration (`h264_vaapi`)
- [x] Native audio resampling (no FFmpeg dependency)
- [x] Full rebranding to Nayutacast (crates, packages, Android SDK)
- [x] Dedicated remote build server configuration (`nayuta`)

**In Progress:**
- [ ] DMA-BUF zero-copy capture between PipeWire and GPU
- [ ] Wayland-native window-specific capture

## Code Conventions

- Use `thiserror` for error types
- Use `parking_lot` for synchronization primitives
- Platform-specific code in `src/{platform}/` directories
- All public APIs documented with rustdoc
- Follow Rust 2024 edition conventions

## Dependencies Tree

```
nayutacast (SDK)
├── capture (screen/audio/camera)
│   ├── common
│   ├── resample
│   └── platform-specific crates
├── codec (FFmpeg wrappers)
│   └── common
├── renderer (wgpu)
│   ├── common
│   └── resample
├── transport (SRT, multicast)
│   └── common
└── discovery (libp2p)
    └── common
```

## Platform Support Matrix

| Feature | Windows | macOS | Linux | Android |
|---------|---------|-------|-------|---------|
| Screen Sender | ✅ | ✅ | ✅ | ❌ |
| Screen Receiver | ✅ | ✅ | ✅ | ✅ |
| Audio Sender | ✅ | ✅ | ✅ | ❌ |
| Audio Receiver | ✅ | ✅ | ✅ | ✅ |
| Camera Sender | ✅ | ❌ | ✅ | ❌ |
| HW Encode | ✅ QSV | ✅ VT | ✅ VAAPI | ❌ |
| HW Decode | ✅ D3D11 | ✅ VT | ✅ VAAPI | ✅ |

Legend: ✅ Working | 🚧 In Progress | ❌ Not Supported

# Video Transcoder

**Video Transcoder** is a fork of [Honey, I Shrunk The Vids](https://github.com/obelisk-complex/video-transcoder-universal), developed by obelisk-complex and contributors. It builds on the original batch encoder with ongoing Linux/AMD GPU detection, HDR handling, and size-reporting improvements.

A cross-platform batch video encoder that shrinks your video files to a target quality level. No need to babysit the queue or tweak settings per file - just pick a target bitrate, add your files, and hit start.

Available as a desktop app and a headless CLI for servers (see [CLI-README.md](CLI-README.md)) The CLI can be used with Sonarr/Radarr via Custom Script; please see the [Sonarr-Radarr Guide](Sonarr-Radarr-Integration.md) for details. Both GUI and CLI use the same encoding engine for compatibility and maintainability. Portable binaries are on the GitHub [Releases page](https://github.com/kalzEOS/video-transcoder/releases).

---

### Screenshots
<details>

![](/screenshots/gui-1.png)

![](/screenshots/gui-2.png)

![](/screenshots/gui-3.png)

![](/screenshots/gui-4.png)

![](/screenshots/gui-5.png)

![](/screenshots/gui-6.png)

![](/screenshots/cli-1.png)

![](/screenshots/cli-2.png)

![](/screenshots/cli-3.png)

</details>

## Features

<details>
<summary><strong>Full feature list</strong></summary>

- **Queue management** - drag-and-drop, clipboard paste (Ctrl+V), file/folder picker. Click, Shift+click, Ctrl+click, Ctrl+A, Delete. Right-click context menu (also Shift+F10 for keyboard users). Drag to reorder.
- **GIF/APNG/WebP support** - animated GIFs, APNGs, and WebPs are converted to proper video files automatically.
- **Per-file codec resolution** - the app decides the codec, container, and audio handling for each file individually based on its source properties. H.264 stays H.264, HEVC stays HEVC, and everything else converts to HEVC. No manual codec selection needed.
- **AV1 support** - first-class AV1 encoding via libsvtav1 and hardware AV1 encoders (AMF, NVENC, QSV, VideoToolbox, VAAPI). Enable Preserve AV1 to keep AV1 sources as AV1 instead of converting to HEVC.
- **Compatibility Mode** - one checkbox to force H.264/MP4/AAC output for maximum device compatibility.
- **Smart decisions** - the app figures out what to do with each file before you start. Files that are already small enough get copied untouched. Files that are too big get shrunk. The queue shows the plan in real time as you change settings.
- **GPU encoding** - tests your GPU at startup and picks the fastest working encoder for each codec family (HEVC, H.264, AV1). Falls back to software automatically if the GPU encoder fails mid-file.
- **QP / CRF quality modes** - two ways to control quality when files are below your target bitrate. QP gives predictable sizes; CRF gives better-looking results with less predictable sizes.
- **Precision mode** - one checkbox for the best possible quality. Uses software encoding with smart analysis that adapts to your system's RAM. Tests a few short clips first to make sure it won't make the file bigger. Slower, but produces the smallest file that still looks great.
- **VBR peak ceiling** - controls how much the bitrate can spike on action-heavy scenes (1.5x to 3x). Higher values look better on complex content.
- **Audio handling** - each audio track is handled separately. Tracks already below 640kbps are copied as-is. Tracks above the cap are re-encoded to the same codec at the cap. Codecs with no ffmpeg encoder (DTS, TrueHD) fall back to EAC3. Compatibility Mode forces AAC.
- **HDR10 and bit depth** - Preserve compatible 8-bit/10-bit color properties and HDR10 signaling; optional HDR10+ preservation is available for supported sources (see below).
- **Pre-flight checks** - Warn about unsupported Dolby Vision base layers and other combinations that could produce incorrect output.
- **Subtitles** - all subtitle tracks are kept.
- **Output options** - save to a folder, next to the source file, or replace the source. Container is preserved per-file (MKV stays MKV, MP4 stays MP4, MOV stays MOV; other formats default to MKV).
- **Queue columns** - source size, estimated output size, resolution, HDR badge, source/target bitrate and resizable headers. Estimated sizes are not guaranteed.
- **Size safety net** - if the encoded file ends up meaningfully bigger than the original (accounting for container overhead based on frame rate and audio streams), the app throws it away and copies the original into the new container instead. Files already below the target threshold are copied without re-encoding.
- **MKV tag repair** - detects and corrects stale stream statistics tags left behind by ffmpeg and third-party muxing tools, so bitrate decisions use the real numbers. Runs automatically at import and after each encode, with optional manual deep repair for severely corrupted metadata.
- **Performance controls** - limit CPU threads and/or run encoding at low priority so your PC stays usable.
- **ETA** - shows estimated time remaining in the progress bar and the window title, so you can see it from the taskbar.
- **Batch controls** - start, pause, cancel current file, cancel everything. Progress bars per file and overall. Files added mid-batch are picked up automatically.
- **Dry run** - shows what the app would do without actually encoding anything.
- **Network drive support** - detects files on network shares and stages them locally in waves before encoding, so slow networks don't bottleneck the encoder. Wave planning groups files to fit available staging space, stages an entire wave at once, encodes, cleans up, and repeats.
- **Disk space monitoring** - pauses encoding if your drive gets too full, resumes when space frees up.
- **Post-batch actions** - shut down, sleep, log out, or run a custom command when the batch finishes.
- **Log console** - colour-coded log with filters, optional file export.
- **Notifications** - system notification when the batch finishes.
- **Auto-clear** - optionally clears finished items from the queue.
- **Flatpak support** - available as a Flatpak bundle for Linux with host filesystem access so ffmpeg subprocesses can reach your files directly.
- **Themes** - six built-in themes (dark, light, and four community-inspired palettes). Create your own with just 6 colour values - the app derives everything else automatically. See [THEMES.md](THEMES.md).
- **ffmpeg downloader** - offers to download ffmpeg for you if it's not installed.
- **Remembers settings** - settings are saved to the platform's application data directory and restored on the next launch.
- **CLI version** - same engine as a command-line tool for servers and scripts. Shows a persistent queue table during encoding matching the GUI's columns. See [CLI-README.md](CLI-README.md).

</details>

> **Build availability:** Check this fork's [Releases](https://github.com/kalzEOS/video-transcoder/releases) for actual downloads. Some names below describe intended packaging; they do not guarantee that a particular build has been published. The full internal rename is under review in [PR #10](https://github.com/kalzEOS/video-transcoder/pull/10), so existing builds may still use legacy filenames.

## Download

Grab the latest build from the GitHub **[Releases page](https://github.com/kalzEOS/video-transcoder/releases)**.

Each platform has a standard build (you provide ffmpeg) and a **-full** build (ffmpeg included). The full builds are larger but work out of the box with no extra setup.

**Standard builds** - ffmpeg not included:

| Platform | GUI | CLI |
|----------|-----|-----|
| **Windows** | `video-transcoder-windows.exe` | `video-transcoder-cli-windows.exe` |
| **Linux** | `.AppImage` | `video-transcoder-cli-linux` |
| **macOS (Apple Silicon)** | `.dmg` (arm64) | `video-transcoder-cli-macos-arm64` |
| **macOS (Intel)** | `.dmg` (x64) | `video-transcoder-cli-macos-x64` |

**Full builds** - ffmpeg included, no extra dependencies:

| Platform | GUI | CLI |
|----------|-----|-----|
| **Windows** | `video-transcoder-windows-full.zip` | `video-transcoder-cli-windows-full.zip` |
| **Linux** | `video-transcoder-linux-full.AppImage` | `video-transcoder-cli-linux-full.tar.gz` |
| **macOS (Apple Silicon)** | `video-transcoder-macos-arm64-full.dmg` | `video-transcoder-cli-macos-arm64-full.tar.gz` |
| **macOS (Intel)** | `video-transcoder-macos-x64-full.dmg` | `video-transcoder-cli-macos-x64-full.tar.gz` |

**Flatpak** - sandboxed Linux build with ffmpeg included:

| Platform | GUI |
|----------|-----|
| **Linux** | `video-transcoder-linux.flatpak` |

The Flatpak bundles ffmpeg and has host filesystem access for encoding. The CLI is not included in the Flatpak.

Most downloadable builds are intended to be portable; packaging requirements vary by platform. On Linux, mark the AppImage executable (`chmod +x`). On macOS, open the DMG and drag to Applications. On Windows, extract the zip to a folder.

<details>
<summary><strong>Linux display troubleshooting</strong></summary>

Video Transcoder sets three environment variables before GTK initialises to work around WebKitGTK rendering issues on Wayland. These are only set if you haven't already set them yourself:

| Variable | Default | What it does |
|----------|---------|-------------|
| `WEBKIT_DISABLE_DMABUF_RENDERER` | `1` | Disables DMA-buf buffer sharing. Fixes `EGL_BAD_PARAMETER` and GBM buffer failures on AMD RDNA3+, some Intel iGPUs, and NVIDIA with Wayland. |
| `WEBKIT_DISABLE_COMPOSITING_MODE` | `1` | Disables GPU compositing entirely. Catches EGL display creation failures that the DMA-buf fix alone doesn't cover. |
| `__NV_DISABLE_EXPLICIT_SYNC` | `1` | Disables NVIDIA explicit sync. Fixes blank/flickering windows on NVIDIA 545+ drivers with Wayland. |

If your system handles Wayland rendering correctly and you want GPU compositing back, override any of these before launching:

```bash
WEBKIT_DISABLE_COMPOSITING_MODE=0 WEBKIT_DISABLE_DMABUF_RENDERER=0 ./video-transcoder
```

The Flatpak respects these overrides via `flatpak run --env=VARIABLE=VALUE com.kalzi.videotranscoder` or by setting them in your shell before `flatpak run`.

</details>

If you use a standard build, ffmpeg is required. The GUI offers to download it on first launch. The CLI expects it on your PATH (`apt install ffmpeg`, `brew install ffmpeg`, `choco install ffmpeg`, etc.).

| OS | Architecture | GPU encoders |
|----|-------------|-------------|
| Windows | x86_64 | AMF, NVENC, QSV |
| Linux | x86_64 | VAAPI, NVENC, QSV |
| macOS | x86_64 / ARM64 | VideoToolbox |

Hardware encoder detection covers HEVC, H.264, and AV1 codec families on all platforms. Each encoder is verified with a test encode at startup.

<details>
<summary><strong>Building from source</strong></summary>

Requires Rust (stable) and the Tauri v2 CLI. See the [Tauri prerequisites guide](https://v2.tauri.app/start/prerequisites/) for platform-specific dependencies.

```bash
# GUI
cargo install tauri-cli --version "^2"
cargo tauri build

# CLI (no GUI/Tauri/WebKit dependencies)
cargo build --manifest-path src-tauri/Cargo.toml --release --bin video-transcoder-cli --no-default-features --features cli
```


**Note:** Until [PR #10](https://github.com/kalzEOS/video-transcoder/pull/10) is merged, the actual CLI executable and Cargo target on `main` are still `histv-cli`. Use the name corresponding to the branch you are building.

**Cargo features:**

| Feature | Default | What it gates |
|---------|---------|---------------|
| `custom-protocol` | GUI | Tauri custom-protocol handler (required for release GUI builds) |
| `downloader` | GUI | In-app ffmpeg downloader (pulls in `reqwest` + `futures-util`); GUI-only — the CLI expects ffmpeg on your PATH |
| `cli` | CLI | Builds the headless `video-transcoder-cli` binary without any Tauri/GTK/WebKit dependencies |

</details>

## How it works

You set a target bitrate and the app decides what to do with each file:

| Situation | What happens |
|-----------|-------------|
| Already small enough, same codec family | **Copied as-is** - no re-encoding |
| Already small enough, different codec | **Re-encoded for quality** using QP or CRF into the target codec |
| Too big | **Shrunk** to hit the target bitrate using VBR encoding |
| Zero bitrate / unreadable header | **Re-encoded for quality** using QP or CRF |
| GIF, APNG, or animated WebP | **Always re-encoded** into a proper video |

The target codec is determined per-file: H.264 sources stay H.264, HEVC sources stay HEVC, and everything else (MPEG-2, VP9, etc.) converts to HEVC. With Preserve AV1 enabled, AV1 sources stay AV1. With Compatibility Mode, everything becomes H.264/MP4.

Files within 15% above the threshold that are already in the target codec are also copied rather than re-encoded, to avoid wasting time on marginal gains.

If an encode makes the file meaningfully bigger than it was (accounting for container overhead), the app throws away the encode and copies the original into the new container instead. Small size increases from container format differences are tolerated.

### HDR10 and HDR10+ handling

Video Transcoder aims to keep compatible HDR10 sources in HDR10 when re-encoding, including 10-bit color signaling and supported static metadata. HDR10+ adds dynamic metadata, which is not automatically retained by an ordinary HEVC re-encode.

An **optional HDR10+ Preservation** workflow can extract and reinsert compatible HDR10+ metadata for supported 10-bit HEVC sources. It requires additional processing and temporary disk space. **This workflow is still being tested on full-length movies**, so check the completed output before deleting your source.

For Dolby Vision Profile 8.1 files with a verified HDR10-compatible base layer, HDR10 output may be possible; preserving Dolby Vision metadata is not promised. Unsupported Dolby Vision profiles are not silently treated as HDR10.

On Linux, Hardware Decoding (GPU) can use a compatible VAAPI render device. If GPU decoding is unavailable, the app can fall back to CPU decoding while retaining GPU encoding.

### File-size estimates

The size shown before encoding is an estimate, not a promise. VBR settings specify a bitrate **target**, and actual output size depends on the video and encoder. Quality-based CRF/QP encodes have variable output sizes. The fork is improving these labels so the estimate is easier to interpret.

### Precision mode

One checkbox for the best quality the app can produce. It picks the smartest encoding strategy based on how much RAM your system has:

| Your RAM | What it does |
|----------|-------------|
| 16GB or more | Looks 250 frames ahead to plan bitrate (single pass) |
| 8-16GB | Looks 120 frames ahead to plan bitrate (single pass) |
| Under 8GB | Scans the whole file first, then encodes (two passes) |

Before starting the full encode, it tests a few short clips to make sure the result won't end up bigger than the original. If it would, the app switches to a safer mode automatically.

### Who Video Transcoder is for

People with a pile of video files at different sizes and formats who want to shrink the big ones without fiddling with every file individually. Set the target, add the files, walk away.

### How Video Transcoder compares to other tools
<details>

**vs. HandBrake** - HandBrake is built for tweaking every file individually. Video Transcoder is built for doing a whole folder at once without thinking about it. It figures out per-file whether to shrink, copy, or re-encode. If you want fine control over every setting on every file, HandBrake is the better tool.

**vs. Tdarr** - Tdarr runs as a server with plugins, Docker, and a web dashboard. Video Transcoder is an app you open, use, and close.

**vs. writing your own ffmpeg script** - Video Transcoder handles the parts that are annoying to script: figuring out what each file needs, picking the right encoder, falling back when hardware fails, prompting for overwrites, pausing/resuming, and cleaning up after itself.

</details>

<details>
<summary><strong>Project structure</strong></summary>

```
├── src/                        # Frontend (HTML + CSS + JS)
│   ├── index.html
│   ├── js/app.js
│   └── css/app.css
├── src-tauri/
│   ├── Cargo.toml
│   ├── tauri.conf.json
│   ├── build.rs
│   └── src/
│       ├── main.rs             # GUI entry point
│       ├── cli_main.rs         # CLI entry point
│       ├── lib.rs              # Tauri commands, app setup, shared state
│       ├── encoder.rs          # Encoder detection, flag mapping, batch loop
│       ├── ffmpeg.rs           # Binary resolution, download, spawning
│       ├── probe.rs            # Media probing via ffprobe (DV/HDR10+ detection)
│       ├── queue.rs            # Queue data structures, file collection
│       ├── encoder_devices.rs  # Dedicated GPU ranking, independent Intel QSV
│       ├── video_format.rs     # Bit depth, HDR10 policy and color validation
│       ├── pipeline_progress.rs # UI stage and cancellation forwarding
│       ├── mkv_tags.rs         # MKV stream statistics tag repair (EBML)
│       ├── webp_decode.rs      # Animated WebP RIFF parser + decode pipeline
│       ├── events.rs           # EventSink + BatchControl traits
│       ├── config.rs           # GUI persistent settings
│       ├── themes.rs           # Theme loading + built-in themes
│       ├── tauri_sink.rs       # GUI EventSink (Tauri emit)
│       ├── tauri_batch_control.rs  # GUI BatchControl (Mutex + emit)
│       ├── cli.rs              # CLI arg parsing (clap) + job files
│       ├── cli_sink.rs         # CLI EventSink (stderr + indicatif)
│       ├── batch_control.rs    # CLI BatchControl (atomics + TTY prompts)
│       ├── remote.rs           # Network mount detection + caching
│       ├── staging.rs          # Wave-based local staging for remote files
│       └── disk_monitor.rs     # Disk-space estimation + runtime monitoring
├── .github/workflows/          # CI: per-platform builds
├── THEMES.md                   # Theme creation guide
├── CLI-README.md               # CLI documentation
├── Sonarr-Radarr-Integration.md
└── README.md
```

</details>

### Fork and upstream credits

Video Transcoder is based on [Honey, I Shrunk The Vids](https://github.com/obelisk-complex/histv-universal) by obelisk-complex and contributors. The original project supplies the foundation for the desktop UI, queue engine, CLI and cross-platform encoding workflows. This fork is evolving the Linux/AMD experience and HDR and size-reporting behavior. Upstream credits, commit history and license obligations are retained.

### Licence

[GPL-3.0-or-later](LICENSE)

### Third-party software

Depending on the release, **-full** builds may bundle [FFmpeg](https://ffmpeg.org) and [MP4Box](https://gpac.io) (GPAC). FFmpeg is licensed under the [GNU General Public Licence v2+](https://www.gnu.org/licenses/old-licenses/gpl-2.0.html). The bundled FFmpeg binaries are unmodified static GPL builds from [BtbN/FFmpeg-Builds](https://github.com/BtbN/FFmpeg-Builds) (Windows/Linux) and [evermeet.cx](https://evermeet.cx/ffmpeg/) (macOS). MP4Box is licensed under the [GNU Lesser General Public Licence v2.1+](https://www.gnu.org/licenses/old-licenses/lgpl-2.1.html). The corresponding source code is available from those repositories. For source and license compliance information, see the linked upstream projects and the license files in this repository.

The optional HDR10+ workflow may require GPAC/MP4Box. The exact bundled tools depend on the build; check your release before relying on them.

This software is based in part on the work of the Independent JPEG Group. See [THIRD-PARTY-LICENCES](THIRD-PARTY-LICENCES) for full details.

# Video Transcoder

A cross-platform batch video encoder built around **FFmpeg**, with a desktop interface and a headless command-line tool. Add a queue, choose a target bitrate or quality mode, and let the application decide whether each file should be copied or encoded.

> **Fork status:** This project is a fork of [obelisk-complex/histv-universal](https://github.com/obelisk-complex/histv-universal). The **Video Transcoder** rebrand, including executable names and application identity, is being developed separately in [PR #10](https://github.com/kalzEOS/video-transcoder/pull/10). Until that PR is merged and tested, builds from `main` may still carry the original HISTV identifiers. The README work itself changes no application code.

## Highlights

- **Batch processing:** add files and folders, inspect per-file decisions, reorder the queue, pause or cancel, and monitor progress.
- **Smart bitrate decisions:** copy compatible files that are already below the threshold; use VBR for larger sources and QP/CRF for quality-based encodes.
- **Hardware encoding:** detect and test supported encoders for AMD, Intel, NVIDIA and Apple platforms, with software fallback options. On Linux, VAAPI can use a selected render device.
- **HDR handling:** preserve HDR10 color signaling and supported static metadata. Optional HDR10+ preservation for compatible 10-bit HEVC sources uses a separate metadata extraction, reinsertion and packaging pipeline.
- **Audio and subtitles:** retain or convert audio according to selected settings and preserve compatible subtitle tracks.
- **Output controls:** choose an output folder, write beside the source, or replace the source when requested.
- **Safety features:** output-size checks, disk-space monitoring, MKV statistics-tag repair, and staging for network sources.
- **Desktop and CLI:** the same encoding engine supports interactive desktop use and automation.

## Important HDR details

**HDR10 and HDR10+ are different.** HDR10 uses static metadata; HDR10+ adds dynamic scene/frame metadata. Re-encoding an HDR10+ source without the optional preservation pipeline generally produces HDR10 rather than preserving the HDR10+ metadata.

The HDR10+ pipeline is more demanding than ordinary encoding: it needs extra disk space for intermediate files and packaging, and it can fail after the main video encode. Recent changes on `main` address disk-backed packaging workspace and file-size estimates, but **full-length output validation is still recommended**. Do not assume HDR10+ preservation succeeded based solely on an HDR badge or successful HEVC encoding.

Dolby Vision is not the same as HDR10+. The supported HDR10-compatible base layer may be retained as HDR10, but Dolby Vision metadata preservation is not promised by this fork's documented workflow. Unsupported base layers should not be silently converted.

## Screenshots

These images are inherited from the upstream project and may not reflect the latest fork UI or branding.

<details>
<summary>View screenshots</summary>

![Desktop interface](screenshots/gui-1.png)
![Desktop queue](screenshots/gui-2.png)
![Desktop settings](screenshots/gui-3.png)
![CLI interface](screenshots/cli-1.png)

</details>

## Getting the application

Check this fork's [Releases](https://github.com/kalzEOS/video-transcoder/releases) for any published builds. **A release may not yet be available for the latest changes.** Development builds can be created from source as described below. Do not confuse upstream releases with releases of this fork.

On Linux, the AppImage can be launched after granting execute permission:

```sh
chmod +x ./Video-Transcoder*.AppImage
./Video-Transcoder*.AppImage
```

Replace the wildcard with the actual filename if your downloaded or locally built AppImage uses another name.

FFmpeg and FFprobe are required for media processing. The desktop application can look for available binaries or offer downloads, depending on build configuration. HDR10+ packaging may also require **MP4Box (GPAC)**, supplied by the build or installed separately. Tool availability varies by package.

## Build from source

Prerequisites: a stable Rust toolchain, the Tauri v2 CLI, and [Tauri's platform-specific build dependencies](https://v2.tauri.app/start/prerequisites/).

From the repository root:

```sh
# Check the Rust code
cd src-tauri
cargo check --release
cd ..

# Run frontend tests
node --test src/js/*.test.js

# Build a Linux AppImage
cd src-tauri
cargo tauri build --bundles appimage
```

The Linux AppImage normally appears under `src-tauri/target/release/bundle/appimage/`.

For a headless CLI build on the **current main branch**:

```sh
cargo build --manifest-path src-tauri/Cargo.toml --release --bin histv-cli --no-default-features --features cli
```

After the separate rebrand PR is merged, the CLI target will be `video-transcoder-cli`. Do not change the command until the branch you are building actually contains that rename.

## How decisions work

| Source / setting | Typical behavior |
| --- | --- |
| Compatible file already small enough | Copy without re-encoding |
| Source above target bitrate | Re-encode using VBR |
| Quality-based mode | Encode using QP or CRF, depending on encoder/settings |
| Compatibility mode | Prefer H.264/MP4/AAC for broad playback support |
| HDR10 source | Preserve compatible 10-bit HDR10 signaling |
| HDR10+ preservation explicitly enabled | Run additional metadata processing for supported HEVC inputs |

**File-size estimates are estimates, not guarantees.** Actual VBR output depends on encoder rate control and source complexity. The interface should distinguish target-budget projections from measured sizes.

## CLI and integrations

The headless CLI supports dry runs, batch settings, file/folder input, overwrite policies, hardware encoding and scripting. See the existing [CLI guide](CLI-README.md) and [Sonarr/Radarr integration guide](Sonarr-Radarr-Integration.md).

> Those guides are inherited from upstream and may still use the legacy `histv-cli` command and describe behavior from older versions. Verify flags with `--help` for the exact binary you built.

## Linux and Wayland

The application contains WebKitGTK compatibility workarounds for some Linux GPU/Wayland combinations. If the UI has display problems, consult the Tauri/WebKitGTK prerequisites and the application's launch environment before changing your GPU encoding settings. UI rendering and FFmpeg's video encoder are separate subsystems.

## Development and testing

Before publishing a build, verify:

1. Rust compilation and tests, including CLI-feature builds.
2. JavaScript tests.
3. Desktop startup, queue import, settings persistence, and encoder detection.
4. A short real-source encode and playback check, including audio/subtitle sync.
5. For HDR10+, actual dynamic metadata presence and playback across the resulting file, not just the initial frames.

The [rebranding PR](https://github.com/kalzEOS/video-transcoder/pull/10) additionally needs settings-migration and packaging checks before merging.

## Project background and credit

Video Transcoder is a fork of **[Honey, I Shrunk The Vids](https://github.com/obelisk-complex/histv-universal)** by **obelisk-complex** and contributors. The original project provided the batch-encoding architecture and much of the codebase. This fork develops Linux/AMD detection, HDR workflows, size-reporting and packaging improvements. Credit and license notices remain in the repository.

This project is distributed under **[GPL-3.0-or-later](LICENSE)**. See [THIRD-PARTY-LICENCES](THIRD-PARTY-LICENCES) for third-party notices. FFmpeg and GPAC have their own licenses and redistribution requirements.

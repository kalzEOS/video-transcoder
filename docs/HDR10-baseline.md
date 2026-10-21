# Fast HDR10 AMD baseline

This branch restores the fast direct FFmpeg approach of the dedicated-GPU fix,
with later fixes retained where testing demonstrated a correctness problem.
It is not a byte-for-byte restoration of the first executable.

The pre-change source archive and binary Git diff are preserved under
`recovery/before-hdr10-baseline-20261021/`. Its AppImage checksum manifest covers
all previously existing AppImage artifacts. Nothing in that snapshot is required
by the application at runtime. Previous builds remain in `dist/`.

## Retained and removed behavior

- Retain 1920×1080 hardware capability probes, per-device testing, dedicated GPU
  preference, verified-device binding through encoding, and independent QSV.
- Retain the optional GPU-decoding checkbox and CPU-decoding retry on the same
  GPU encoder. Eligible 10-bit VAAPI frames stay on the GPU using P010 surfaces.
- Retain source depth, chroma, HDR/SDR color and static HDR checks. No implicit
  8-to-10-bit conversion, tone mapping, or unsupported precision reduction.
- Retain HEVC VAAPI MP4 `hev1` tagging. Previously forcing `hvc1` on this machine
  stripped required in-band parameter sets and caused green/corrupt pictures.
- Remove Dolby Vision extraction, reinsertion, packaging, full-movie metadata
  scans, MP4Box discovery/download and dependencies. Remove the HDR10+ processing
  pipeline as well; no optional expensive preservation mode is enabled.
- Profile 8.1 DV may be re-encoded as HDR10 only with a confirmed 10-bit
  BT.2020/PQ compatible base. Unknown/incompatible profiles block re-encoding.
- Preserve the chosen container. MKV remains MKV in Auto mode. Explicit Audio
  Copy now bypasses bitrate caps; automatic conversion still honors its cap.
- Diagnostic files require Save log. Normal successful output folders contain
  only the requested movie output.
- Permit a measured initial timestamp rebase up to one second in duration
  validation; larger offsets cannot mask truncation. Ordinary duration tolerance
  remains two frames or 100 ms, whichever is larger.

## HDR10+ investigation

The original source implementation attempted HDR10+ preservation through a
separate extraction/reinsertion/remux pipeline when its probe detected HDR10+.
Its stream-only ffprobe detection does **not** detect the frame-side HDR10+
metadata in the representative Emancipation sample: stream side data reports
the Dolby Vision configuration only, while independent full decoded-frame
probing finds HDR10+ on all 242 pictures. The original fast FFmpeg VAAPI path
therefore cannot be treated as a reliable HDR10+ preservation guarantee. No
previous user-produced compressed movie was available to certify its metadata.

Installed FFmpeg n9.0.2 `hevc_vaapi -h` advertises static HDR mastering/content
light SEI, not HDR10+ dynamic SEI. This matches the [FFmpeg encoder implementation](https://github.com/FFmpeg/FFmpeg/blob/master/libavcodec/vaapi_encode_h265.c).
Independent probing of this baseline's scene conversion found:

| Frame metadata | Source | Encoded output |
| --- | ---: | ---: |
| Mastering display | 242 | 242 |
| Content light level | 242 | 242 |
| HDR10+ SMPTE 2094-40 | 242 | 0 |
| Dolby Vision RPU | 242 | 0 |

Output remains Main 10 HEVC, BT.2020/PQ, limited range. It is deliberately HDR10.
Reliable HDR10+ retention would require a separate payload/timeline processing
mechanism; it is not a free consequence of preserving 10-bit pixels or HDR tags.

Earlier measured work on the identical 15-minute source/clean encoded
intermediate cost 19.36 seconds for separate metadata pipelines and 5.93 seconds
for the experimental shared pipeline, excluding video encoding. On the entire
Emancipation source, preparation alone previously cost 539.11 seconds separately
or 198.86 seconds shared. These are earlier recorded runs, not new full-film
measurements. Cache and disk conditions strongly affect the overhead. The
archived strict metadata benchmark also refused the new seek-trimmed scene for
nonuniform picture timing; ordinary FFmpeg conversion of it succeeds and keeps
all 242 frames. No preservation pipeline was reintroduced to work around this.

This baseline performs zero dynamic metadata passes. Its real scene job took
3.715 seconds including startup/detection/probes, with 1.811 seconds reported for
FFmpeg encoding. The source contained 242 pictures (about ten seconds including
keyframe preroll after a six-second stream-copy trim). These short timings are
not a full-film speed prediction.

## Validation on the user's machine

- 361 standard Rust tests, JavaScript tests (33), formatting and Clippy with
  warnings denied passed.
- The opt-in `hdr10_baseline` integration test passed 14 actual conversions:
  SDR 8-bit, SDR 10-bit, HDR10, dual HDR/DV Profile 8.1, a real movie scene, HLG,
  and unsupported High-10 H.264 GPU decoding, each with CPU/GPU decode selected.
- Every output was fully decoded without errors. The test checks source bit
  depth, frame count, dimensions, aspect ratio, defined HDR/color flags, duration,
  container, audio/subtitle codec/language/title, and exact copied packet hashes.
- Output directories contained only the movie. Fixtures and diagnostic results
  were isolated under `/tmp`, not the user's movie output directory.
- Real scene command selected AMD `/dev/dri/renderD129` for GPU decoding and
  `hevc_vaapi` encoding. SSIM against the source was 0.994755 at 4 Mbps. Source
  and output preview frames were visually inspected; neither was green/corrupt.
- Static HDR loss, incompatible DV base layers, 8/10-bit conversion, multiple
  GPU selection/fallback, cancellation, stage/error forwarding, audio Copy above
  the cap, and measured timestamp rebasing have regression coverage.

Physical tests were short clips. No full-length conversion of this new build,
HDR television playback comparison, or physical NVIDIA/QSV test was performed.
QSV initialization currently fails on this machine and falls back through the
existing detection logic. HDR10+ and Dolby Vision retention on re-encoding are
explicitly outside this baseline's promises. Lossy compression quality still
depends on the selected bitrate/quality settings.

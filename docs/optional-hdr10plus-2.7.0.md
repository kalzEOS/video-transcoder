# HISTV 2.7.0: optional HDR10+ preservation

The new **Preserve HDR10+ metadata (slower)** checkbox is usable, saved, and
off by default. Unchecked retains the stable 2.6.1 fast HDR10 workflow with
no additional full-video metadata passes. Checking it accepts extraction,
frame alignment, reinsertion, packaging and full metadata verification.
Hardware encoding, optional hardware decoding and CPU decode fallback are
independent. Dolby Vision restoration is not enabled by this option.

Original HDR10+ SEI bytes are retained, indexed by presentation-frame order.
The pipeline handles B-frame reordering and sparse metadata, including
metadata beginning after the initial format probe. Output metadata must
match the source on every presentation frame before output is accepted or
source replacement/deletion is allowed. Existing source-depth, static HDR10,
color, duration and bounded playback validation still runs afterward.

GPAC reconstructs video composition timestamps after raw HEVC injection.
The remux retains the encoded video start offset and copies the selected
audio/subtitles, container metadata and chapters. MKV remains MKV in Auto
mode. The bundled static MP4Box includes its source, licence and build
instructions. No system packages are installed. The AppImage continues to
use CachyOS's system FFmpeg/FFprobe and matching GPU driver stack.

Limitations: metadata preservation currently supports 10-bit HEVC with
constant frame timing. Ambiguous/missing timestamps, variable frame timing,
changed picture counts, failed packaging, or changed metadata stop the file
and retain the source. This mode needs additional disk space and processing
time. Bitstream working directories use the output filesystem, with no
silent fallback to system tmpfs, and are removed after success/failure/cancel.
A preparation failure also retains an existing destination file.

Validation on the RX 9070 XT:

- 389 normal Rust tests passed; strict Clippy and Rust formatting passed.
- 33 existing JavaScript tests and app.js syntax validation passed.
- Four two-second HEVC VAAPI conversions passed: preservation on/off, each
  with CPU or GPU decoding. The selected device was `/dev/dri/renderD129`.
- 48 pictures, with 23 distinct HDR10+ payloads beginning at frame 25:
  exact payload/frame comparison and independent FFprobe side-data checks
  passed. Unchecked outputs had no HDR10+ metadata passes.
- Full short-output decoding reported no errors. 10-bit BT.2020/PQ, MKV,
  copied FLAC/subtitle packet hashes, languages and source hashes passed.
- Metadata-only insertion produced identical decoded pixel hashes for MKV
  and explicit MP4. A separate 250 ms video/audio offset test preserved
  each track's packet timestamps. Deliberately misaligned metadata failed
  validation; an ordinary HDR10-only source returned no dynamic metadata.
- Cancellation tests cover preprocessing/reinsertion/verification workspace
  cleanup, input retention, and killing/reaping metadata subprocesses.

These are short representative tests, not a full-movie certification or
performance benchmark. No full-length movie conversion was initiated.
The exact source hashes, release checksum and test logs are retained in dist.
All nine previous AppImages are preserved. Nothing was pushed to GitHub.

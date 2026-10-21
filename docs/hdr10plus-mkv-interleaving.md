# HDR10+ MKV intermittent audio: diagnosis and fix

Branch: `codex/fix-hdr10plus-mkv-audio`, based on `origin/main` at
`68ef011`. The Rust security refresh commits from PR #3 are excluded.
The GUI's `AudioStrategy::CopyAll` default remains unchanged.

## Confirmed failure mode

The final HDR10+ remux combines video from a GPAC MP4 with audio/subtitles
from the encoded MKV. Sparse subtitles can advance the second input's
audio far ahead of the separate video input. FFmpeg's default ten-second
interleave timeout forces queued packets out before earlier packets from
the other input arrive. Each stream's timestamps and encoded payloads
can remain correct while their physical order in the MKV is unsuitable
for bounded player demux queues.

A bounded sample of 12,000 packets from the supplied Onslaught files showed:

- Original: maximum first-audio/video packet lead of 0.473 seconds.
- Affected output: audio through 357.760 seconds physically precedes the
  second video packet, at 0.042 seconds. A/V lead reaches 357.760 seconds.

This explains why an audio stream hash, initial timestamps, and ordinary
FFmpeg decoding can all pass while a player encounters stalls/dropouts.
It also explains why copying the existing streams into a new container
can fix playback without replacing or re-encoding audio.

## Controlled reproduction

A 32-second 384x216 10-bit HEVC fixture uses B-frames, 24000/1001 fps,
768 kbps 5.1 E-AC3 and subtitles at 0 and 24 seconds. GPAC repackages its
video exactly as the production HDR10+ pipeline does.

| Mux configuration | Maximum physical A/V packet lead |
| --- | ---: |
| Existing dual-input remux | 12.895 s |
| Dual-input without `-copyts` | 12.895 s |
| Dual-input with a shorter 1 s interleave timeout | 21.887 s |
| Corrective single-input remux, default timeout | 2.879 s |
| Dual-input with `-max_interleave_delta 0` | 0.172 s |
| Corrective single-input remux with the same setting | 0.172 s |

FFmpeg debug logging recorded 453 forced-output events in the existing
synthetic dual-input remux, explicitly citing the 10,000,000 microsecond
timeout. A headless mpv stress check with a 512 KiB demux queue overflowed
on the bad mux and lost video reference packets. The corrected mux did not.
This is a controlled queue stress test, not an audible Haruna listening test.

FFmpeg documents that a zero interleave delta disables this premature
timeout, rather than disabling interleaving:
[format documentation](https://ffmpeg.org/ffmpeg-formats.html#Format-Options).
The corresponding forced-flush branch is in
[FFmpeg's muxer implementation](https://github.com/FFmpeg/FFmpeg/blob/n9.0/libavformat/mux.c).
GPAC's [import documentation](https://wiki.gpac.io/MP4Box/mp4box-import-opts/)
describes the raw-video `fps` option used by the existing pipeline.

## Minimal production change

Only `hdr10plus_pipeline::inject_hdr10plus()` changes runtime behavior:
the final remux adds `-max_interleave_delta 0` for MKV outputs. It does not
add another full-file remux. MP4 output behavior is unchanged.

`-copyts` and the video-only `-itsoffset` remain intact. These preserve
the encoded timeline and the original video start offset relative to
audio/subtitles; removing them does not address the reproduced ordering
failure and risks changing synchronization. Stream maps, metadata/chapter
mapping, `-c copy`, encoder/device selection and audio defaults are unchanged.

The surrounding finalization code can fall back to a source remux if an
encode grows larger, then patches fixed-length statistics tags and verifies
metadata/output properties. The reproduced failure occurs in the HDR10+
remux itself, before those steps. Statistics-tag patching does not move
packets or change their timestamps. No unrelated finalization changes were
made.

## Validation

- The new ignored integration regression failed before the production fix
  with the measured 12.895-second lead and passed afterward.
- It executes real HDR10+ injection and full per-frame metadata comparison,
  checks physical A/V ordering, compares every audio/subtitle packet hash,
  and checks every stream's PTS/DTS/duration. It also verifies 10-bit color,
  decoded mastering-display/content-light metadata, audio format/layout,
  languages, full short-output decoding, and mpv's bounded queue behavior.
- Existing integration coverage passed for preservation on/off with CPU
  and AMD GPU decoding, copied tracks, delayed video timing and MP4 output.
- 390 ordinary Rust tests passed. Both ignored HDR10+ integration tests
  were explicitly executed and passed. Strict Clippy and formatting passed.
- A separate 12-second stream-copy remux of the supplied affected movie
  preserved all 375 original E-AC3 Atmos packets and all 288 original
  video packets, including HDR10+ SEI bytes and their timestamps. Physical
  A/V lead was 0.041 seconds; all short-clip audio/video decoded cleanly.
  No audio or video was re-encoded, and both full movie files were read-only.

Commands, from `src-tauri` with the available Rust toolchain/cache:

```text
cargo test --features cli --locked --offline
cargo clippy --features cli --locked --offline --all-targets -- -D warnings
cargo fmt --all -- --check
cargo test --features cli --locked --offline --test optional_hdr10plus -- --ignored --nocapture
```

## Limits

Sparse subtitles can require buffering until their next packet or EOF;
disabling the timeout trades premature output for potentially higher RAM
use on long files with sparse tracks. This fix is scoped to offline HDR10+
MKV finalization. No new queue-size heuristics or additional passes were added.

Synthetic E-AC3 does not contain Atmos objects. Actual Atmos packet copying
was checked separately using the supplied movie. Audible uninterrupted
playback of a newly transcoded full movie in Haruna remains a user validation
step; no full-length conversion or GUI listening test was performed.
Existing release AppImages were not rebuilt or modified by this source fix.

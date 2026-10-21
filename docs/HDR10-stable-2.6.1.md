# HISTV 2.6.1 stable HDR10 review

The established GPU pipeline, codec/rate-control settings, GPU preference,
P010 processing and CPU-decoding fallback are unchanged. No performance
optimization, new metadata pipeline or full-movie conversion was initiated.

## Confirmed correctness fix

Cancellation testing exposed pre-existing source writes: importing an MKV and
starting an encode automatically repaired its statistics tags. Partial output
was removed correctly on cancellation, but the source hash could change before
encoding began. Import and encode decisions now estimate bitrate read-only.
Explicit Repair/Deep repair commands and completed-output tag repair remain.
The bitrate formula is unchanged; the automatic source write is removed.

A read-only-source unit regression and the existing real-media integration
suite's source SHA-256 assertions cover this change. Physical cancellation tests
used deliberately stale source tags and a test-only paced FFmpeg input, then
sent SIGINT to the CLI. Both normal folder and replace-source modes removed
their actual partial output, reaped FFmpeg, preserved exact source hashes and
left no source backup file. The GUI uses the same encoding/cancellation engine.

## Quality and reliability evidence

- 362 standard Rust tests and 33 JavaScript tests passed. Clippy with warnings
  denied, formatting and diff checks passed.
- The existing media integration test passed eight short SDR8, SDR10, static
  HDR10 and real dual-HDR Profile 8.1 conversions, each with GPU decoding on/off.
  All output frames decoded without errors. Frame counts, dimensions, display
  and sample aspect ratio, defined color flags, depth, duration and container
  matched. Audio/subtitle codecs, counts, languages/titles and copied packet
  hashes matched. Source hashes remained unchanged. Output folders held only
  the requested movie.
- Two additional short real-movie runs recorded actual command lines: GPU
  decoding used AMD renderD129 with GPU-resident P010; disabled GPU decoding used
  CPU decoding plus P010 upload to the same AMD HEVC encoder. Both decoded fully
  without errors and left only their output movie.
- The user's existing full Emancipation conversion was still running during
  review. Its currently written beginning and ten-minute sections decoded
  cleanly and preserved Main 10, BT.2020/PQ, range, static mastering/content-light
  metadata, frame dimensions/aspect ratio, English EAC3 and both English SRT
  tracks/titles. Matching preview frames were visually inspected: no green
  screen or unexpected crop. The active command used AMD VAAPI decode and HEVC
  encode on renderD129, with EAC3 conversion at the configured automatic cap.
  It was not interrupted. A premature-EOF warning from initially probing beyond
  its written end is not a completed-file failure; final full-movie/end-of-file
  validation remains pending until the user's conversion completes.

The application maps supported audio/subtitle tracks; it does not provide a
new per-track selection interface. Explicit Audio Copy bypasses the automatic
cap and was verified with exact packet hashes. MKV Auto output remains MKV;
explicit container choices retain existing compatibility behavior.

The HDR checkbox label/tooltip and HDR10+ log now state the intentional policy:
re-encoding discards Dolby Vision/HDR10+ dynamic metadata, uses a verified
Profile 8.1 HDR10 base when available, and rejects unsafe fallback. SDR stays
SDR, HLG stays HLG, 8-bit stays 8-bit and 10-bit stays 10-bit. Stream-copy behavior
is unchanged. No metadata extraction/scanning/reinsertion passes were restored.

Physical testing was on the user's AMD machine, using short samples. No new
full-length conversion, HDR-TV playback certification or physical NVIDIA/Intel
QSV testing was performed. Bounded application output validation is not a full
movie visual inspection. Lossy compression quality still depends on settings.

Small machine-readable evidence is saved beside the versioned AppImage. All
previous builds are preserved. Disposable fixtures, encoded samples, preview
frames and AppImage staging copies are removed after packaging/launch checks.

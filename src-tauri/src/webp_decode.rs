//! Animated WebP decoding via RIFF container parsing and ffmpeg frame decode.
//!
//! ffmpeg's built-in WebP decoder does not support animated WebP (ANIM/ANMF
//! chunks). This module works around the limitation by:
//!
//! 1. Parsing the RIFF container to extract canvas dimensions, frame count,
//!    total duration, and each frame's compressed payload.
//! 2. Feeding each frame's raw WebP bytes into a single ffmpeg process via
//!    `image2pipe` stdin, which decodes them using the static WebP decoder.
//! 3. Reading decoded RGBA pixels from ffmpeg's stdout.
//! 4. Compositing each frame onto a canvas according to the WebP animation
//!    spec (x/y offset, disposal method, alpha blending).
//! 5. Piping the composited canvas to a second ffmpeg process that encodes
//!    the final output video.
//!
//! The result is correct animated WebP to video conversion with no external
//! dependencies beyond ffmpeg itself, and per-frame progress reporting.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::events::{BatchControl, EventSink};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

// ── Public types ───────────────────────────────────────────────

/// Metadata extracted from an animated WebP file's container.
#[derive(Debug, Clone)]
pub struct WebpInfo {
    pub width: u32,
    pub height: u32,
    pub frame_count: u32,
    pub total_duration_ms: u32,
    pub loop_count: u16,
    pub has_alpha: bool,
}

/// A single animation frame's metadata and compressed data.
#[derive(Debug)]
struct AnimFrame {
    x_offset: u32,
    y_offset: u32,
    width: u32,
    height: u32,
    duration_ms: u32,
    dispose: bool, // true = dispose to background after rendering
    blend: bool,   // true = alpha-blend onto canvas; false = overwrite
    data: Vec<u8>, // raw WebP bitstream (VP8/VP8L, possibly with ALPH)
}

// ── Container parsing ──────────────────────────────────────────

/// Read a 4-byte ASCII chunk ID.
fn read_fourcc(r: &mut impl Read) -> Result<[u8; 4], String> {
    let mut buf = [0u8; 4];
    r.read_exact(&mut buf)
        .map_err(|e| format!("read fourcc: {e}"))?;
    Ok(buf)
}

/// Read a 32-bit little-endian unsigned integer.
fn read_u32_le(r: &mut impl Read) -> Result<u32, String> {
    let mut buf = [0u8; 4];
    r.read_exact(&mut buf)
        .map_err(|e| format!("read u32: {e}"))?;
    Ok(u32::from_le_bytes(buf))
}

/// Read a 24-bit little-endian unsigned integer (3 bytes).
fn read_u24_le(r: &mut impl Read) -> Result<u32, String> {
    let mut buf = [0u8; 3];
    r.read_exact(&mut buf)
        .map_err(|e| format!("read u24: {e}"))?;
    Ok(buf[0] as u32 | (buf[1] as u32) << 8 | (buf[2] as u32) << 16)
}

/// Read a 16-bit little-endian unsigned integer.
fn read_u16_le(r: &mut impl Read) -> Result<u16, String> {
    let mut buf = [0u8; 2];
    r.read_exact(&mut buf)
        .map_err(|e| format!("read u16: {e}"))?;
    Ok(u16::from_le_bytes(buf))
}

/// Probe an animated WebP for metadata only, without reading frame data.
/// Returns None if the file is not an animated WebP (no ANIM chunk found).
fn probe_metadata(path: &Path) -> Result<Option<WebpInfo>, String> {
    let mut file =
        std::fs::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;

    file.seek(SeekFrom::Start(12))
        .map_err(|e| format!("seek: {e}"))?;

    let mut width: u32 = 0;
    let mut height: u32 = 0;
    let mut has_alpha = false;
    let mut loop_count: u16 = 0;
    let mut total_duration_ms: u32 = 0;
    let mut frame_count: u32 = 0;
    let mut found_anim = false;

    while let Ok(chunk_id) = read_fourcc(&mut file) {
        let chunk_size = match read_u32_le(&mut file) {
            Ok(s) => s,
            Err(_) => break,
        };
        let chunk_start = file.stream_position().map_err(|e| format!("{e}"))?;

        match &chunk_id {
            b"VP8X" => {
                let mut flags = [0u8; 4];
                file.read_exact(&mut flags)
                    .map_err(|e| format!("VP8X: {e}"))?;
                has_alpha = (flags[0] & 0x10) != 0;
                width = read_u24_le(&mut file)? + 1;
                height = read_u24_le(&mut file)? + 1;
            }
            b"ANIM" => {
                let _bg = read_u32_le(&mut file)?;
                loop_count = read_u16_le(&mut file)?;
                found_anim = true;
            }
            b"ANMF" => {
                // Read only the duration (3 bytes at offset +12 into the chunk)
                // Skip: x(3) + y(3) + w(3) + h(3) = 12 bytes
                file.seek(SeekFrom::Start(chunk_start + 12))
                    .map_err(|e| format!("seek: {e}"))?;
                let dur = read_u24_le(&mut file)?;
                total_duration_ms = total_duration_ms.saturating_add(dur);
                frame_count += 1;
                // Skip the rest - no frame data read
            }
            _ => {}
        }

        let padded_size = chunk_size as u64 + (chunk_size as u64 & 1);
        let next = chunk_start + padded_size;
        file.seek(SeekFrom::Start(next))
            .map_err(|e| format!("seek: {e}"))?;
    }

    if !found_anim || frame_count == 0 {
        return Ok(None);
    }

    Ok(Some(WebpInfo {
        width,
        height,
        frame_count,
        total_duration_ms,
        loop_count,
        has_alpha,
    }))
}

/// Probe an animated WebP file for canvas dimensions, frame count, and
/// total duration. Returns `None` if the file is not an animated WebP.
pub fn probe_webp(path: &Path) -> Result<Option<WebpInfo>, String> {
    probe_metadata(path)
}

/// Extract all animation frames from an animated WebP file.
/// Each frame's compressed data is wrapped as a minimal standalone WebP
/// file so ffmpeg's static WebP decoder can handle it.
fn extract_frames(path: &Path) -> Result<(WebpInfo, Vec<AnimFrame>), String> {
    let mut file =
        std::fs::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;

    // Skip RIFF header (12 bytes: "RIFF" + size + "WEBP")
    file.seek(SeekFrom::Start(12))
        .map_err(|e| format!("seek: {e}"))?;

    let mut width: u32 = 0;
    let mut height: u32 = 0;
    let mut has_alpha = false;
    let mut loop_count: u16 = 0;
    let mut total_duration_ms: u32 = 0;
    let mut frames: Vec<AnimFrame> = Vec::new();

    while let Ok(chunk_id) = read_fourcc(&mut file) {
        let chunk_size = match read_u32_le(&mut file) {
            Ok(s) => s,
            Err(_) => break,
        };
        let chunk_start = file.stream_position().map_err(|e| format!("{e}"))?;

        match &chunk_id {
            b"VP8X" => {
                let mut flags = [0u8; 4];
                file.read_exact(&mut flags)
                    .map_err(|e| format!("VP8X: {e}"))?;
                has_alpha = (flags[0] & 0x10) != 0;
                width = read_u24_le(&mut file)? + 1;
                height = read_u24_le(&mut file)? + 1;
            }
            b"ANIM" => {
                let _bg = read_u32_le(&mut file)?;
                loop_count = read_u16_le(&mut file)?;
            }
            b"ANMF" => {
                let x = read_u24_le(&mut file)? * 2; // spec: in units of 2 pixels
                let y = read_u24_le(&mut file)? * 2;
                let w = read_u24_le(&mut file)? + 1;
                let h = read_u24_le(&mut file)? + 1;
                let dur = read_u24_le(&mut file)?;
                let mut flags_byte = [0u8; 1];
                file.read_exact(&mut flags_byte)
                    .map_err(|e| format!("ANMF flags: {e}"))?;
                let dispose = (flags_byte[0] & 0x01) != 0;
                let blend = (flags_byte[0] & 0x02) == 0; // 0 = blend, 1 = no blend

                // Read the compressed frame data (rest of the ANMF chunk)
                let header_bytes = 3 + 3 + 3 + 3 + 3 + 1; // 16 bytes of ANMF header
                if (chunk_size as usize) < header_bytes {
                    return Err(format!("ANMF chunk too small: {} bytes", chunk_size));
                }
                let data_size = chunk_size as usize - header_bytes;
                const MAX_CHUNK_SIZE: usize = 256 * 1024 * 1024;
                if data_size > MAX_CHUNK_SIZE {
                    return Err(format!(
                        "ANMF chunk too large: {} bytes (max {})",
                        data_size, MAX_CHUNK_SIZE,
                    ));
                }
                let mut data = vec![0u8; data_size];
                file.read_exact(&mut data)
                    .map_err(|e| format!("ANMF data: {e}"))?;

                total_duration_ms = total_duration_ms.saturating_add(dur);
                frames.push(AnimFrame {
                    x_offset: x,
                    y_offset: y,
                    width: w,
                    height: h,
                    duration_ms: dur,
                    dispose,
                    blend,
                    data,
                });
            }
            _ => {}
        }

        let padded_size = chunk_size as u64 + (chunk_size as u64 & 1);
        let next = chunk_start + padded_size;
        file.seek(SeekFrom::Start(next))
            .map_err(|e| format!("seek: {e}"))?;
    }

    let info = WebpInfo {
        width,
        height,
        frame_count: frames.len() as u32,
        total_duration_ms,
        loop_count,
        has_alpha,
    };

    Ok((info, frames))
}

/// Wrap a raw frame bitstream as a minimal standalone WebP file.
/// ffmpeg's static WebP decoder needs the RIFF/WEBP header to recognise
/// the input format.
fn wrap_as_standalone_webp(frame_data: &[u8]) -> Vec<u8> {
    // Detect the sub-format from the frame data's leading bytes
    let is_vp8l = frame_data.starts_with(b"VP8L");
    let is_vp8 = frame_data.starts_with(b"VP8 ");
    let is_alph = frame_data.starts_with(b"ALPH");

    if is_vp8 || is_vp8l || is_alph {
        // Frame data already has chunk headers - wrap with RIFF/WEBP
        let file_size = 4 + frame_data.len(); // "WEBP" + data
        let mut buf = Vec::with_capacity(12 + frame_data.len());
        buf.extend_from_slice(b"RIFF");
        buf.extend_from_slice(&(file_size as u32).to_le_bytes());
        buf.extend_from_slice(b"WEBP");
        buf.extend_from_slice(frame_data);
        buf
    } else {
        // Raw VP8 bitstream without chunk header - wrap as "VP8 " chunk
        let chunk_size = frame_data.len() as u32;
        let file_size = 4 + 8 + frame_data.len(); // "WEBP" + "VP8 " + size + data
        let mut buf = Vec::with_capacity(12 + 8 + frame_data.len());
        buf.extend_from_slice(b"RIFF");
        buf.extend_from_slice(&(file_size as u32).to_le_bytes());
        buf.extend_from_slice(b"WEBP");
        buf.extend_from_slice(b"VP8 ");
        buf.extend_from_slice(&chunk_size.to_le_bytes());
        buf.extend_from_slice(frame_data);
        buf
    }
}

// ── Canvas compositing ─────────────────────────────────────────

/// RGBA pixel canvas for frame compositing.
struct Canvas {
    width: u32,
    height: u32,
    /// Row-major RGBA pixels, length = width * height * 4.
    pixels: Vec<u8>,
}

impl Canvas {
    fn new(width: u32, height: u32) -> Result<Self, String> {
        let size = (width as usize)
            .checked_mul(height as usize)
            .and_then(|v| v.checked_mul(4))
            .ok_or_else(|| format!("Canvas dimensions too large: {}x{}", width, height))?;
        Ok(Self {
            width,
            height,
            pixels: vec![0u8; size],
        })
    }

    /// Clear the entire canvas to transparent black.
    fn clear(&mut self) {
        self.pixels.fill(0);
    }

    /// Clear a rectangular region to transparent black.
    fn clear_rect(&mut self, x: u32, y: u32, w: u32, h: u32) {
        if x >= self.width || y >= self.height || w == 0 || h == 0 {
            return;
        }
        let y_end = (y + h).min(self.height);
        let x_end = (x + w).min(self.width);
        for row in y..y_end {
            let start = ((row * self.width + x) * 4) as usize;
            let end = ((row * self.width + x_end) * 4) as usize;
            if start < self.pixels.len() && end <= self.pixels.len() {
                self.pixels[start..end].fill(0);
            }
        }
    }

    /// Composite decoded RGBA pixels onto the canvas at the given offset.
    /// If `blend` is true, alpha-blend; otherwise overwrite.
    fn composite(&mut self, frame_rgba: &[u8], x: u32, y: u32, w: u32, h: u32, blend: bool) {
        for row in 0..h {
            let canvas_y = y + row;
            if canvas_y >= self.height {
                break;
            }
            for col in 0..w {
                let canvas_x = x + col;
                if canvas_x >= self.width {
                    break;
                }
                let src_idx = ((row * w + col) * 4) as usize;
                let dst_idx = ((canvas_y * self.width + canvas_x) * 4) as usize;

                if src_idx + 3 >= frame_rgba.len() || dst_idx + 3 >= self.pixels.len() {
                    continue;
                }

                if blend {
                    // Alpha compositing: src over dst
                    let sa = frame_rgba[src_idx + 3] as u32;
                    if sa == 255 {
                        self.pixels[dst_idx..dst_idx + 4]
                            .copy_from_slice(&frame_rgba[src_idx..src_idx + 4]);
                    } else if sa > 0 {
                        let da = self.pixels[dst_idx + 3] as u32;
                        let inv_sa = 255 - sa;
                        for c in 0..3 {
                            let sc = frame_rgba[src_idx + c] as u32;
                            let dc = self.pixels[dst_idx + c] as u32;
                            self.pixels[dst_idx + c] = ((sc * sa + dc * da * inv_sa / 255)
                                / (sa + da * inv_sa / 255).max(1))
                                as u8;
                        }
                        self.pixels[dst_idx + 3] = (sa + da * inv_sa / 255) as u8;
                    }
                    // sa == 0: source pixel fully transparent, leave dst unchanged
                } else {
                    // Overwrite mode
                    self.pixels[dst_idx..dst_idx + 4]
                        .copy_from_slice(&frame_rgba[src_idx..src_idx + 4]);
                }
            }
        }
    }

    /// Return the canvas pixels as a contiguous RGBA byte slice.
    fn as_rgba(&self) -> &[u8] {
        &self.pixels
    }
}

// ── Decode + encode pipeline ───────────────────────────────────

/// Decode an animated WebP file and encode it to a video file.
///
/// Uses two ffmpeg processes:
/// 1. A decoder that receives individual WebP frames via image2pipe stdin
///    and outputs raw RGBA pixels to stdout.
/// 2. An encoder that receives composited raw RGBA frames via rawvideo
///    stdin and writes the output file.
///
/// Progress is reported per-frame via `sink.file_progress`.
pub async fn transcode_animated_webp(
    input_path: &str,
    output_path: &str,
    video_args: &[String],
    threads: u32,
    low_priority: bool,
    sink: &dyn EventSink,
    batch_control: &dyn BatchControl,
) -> Result<TranscodeResult, String> {
    transcode_animated_webp_with_device(
        input_path,
        output_path,
        video_args,
        None,
        threads,
        low_priority,
        sink,
        batch_control,
    )
    .await
}

async fn interruptible<T>(
    future: impl std::future::Future<Output = Result<T, String>>,
    control: &dyn BatchControl,
    pauses: &mut [crate::process_control::PauseGuard],
) -> Result<T, String> {
    tokio::pin!(future);
    loop {
        if control.should_cancel_current() || control.should_cancel_all() {
            return Err("Cancelled".into());
        }
        let paused = control.is_paused();
        for process in pauses.iter_mut() {
            process.set(paused)?;
        }
        tokio::select! {
            value=&mut future, if !paused => return value,
            _=tokio::time::sleep(std::time::Duration::from_millis(100))=>{}
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn transcode_animated_webp_with_device(
    input_path: &str,
    output_path: &str,
    video_args: &[String],
    device: Option<&str>,
    threads: u32,
    low_priority: bool,
    sink: &dyn EventSink,
    batch_control: &dyn BatchControl,
) -> Result<TranscodeResult, String> {
    sink.log("  Animated WebP: parsing RIFF container...");
    let (info, frames) = extract_frames(Path::new(input_path))?;
    if frames.is_empty() {
        return Err("No animation frames found in WebP file".into());
    }
    let padded_w = info.width.checked_add(1).ok_or("WebP width overflow")? & !1;
    let padded_h = info.height.checked_add(1).ok_or("WebP height overflow")? & !1;
    let mut canvas = Canvas::new(padded_w, padded_h)?;
    canvas.clear();
    let frame_byte_size = canvas.as_rgba().len();
    let frames = std::sync::Arc::new(frames);
    let enc_args = crate::encoder::raw_rgba_encoder_args(
        output_path,
        video_args,
        padded_w,
        padded_h,
        50.0,
        threads,
        device,
    );
    sink.processing_stage("encoding", "Encoding animated WebP...", None);
    sink.batch_command(&format!("ffmpeg {}", enc_args.join(" ")));
    let mut command = crate::ffmpeg::ffmpeg_command();
    command
        .kill_on_drop(true)
        .args(&enc_args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    #[cfg(windows)]
    if low_priority {
        command.creation_flags(0x00004000 | 0x08000000);
    }
    let mut encoder = command
        .spawn()
        .map_err(|e| format!("Cannot start WebP encoder: {e}"))?;
    let encoder_progress = crate::encoder::FfmpegProgress::new();
    let encoder_reader = crate::encoder::spawn_stderr_reader(
        encoder.stderr.take().unwrap(),
        &encoder_progress,
        None,
        "WebP encoder",
    );
    #[cfg(unix)]
    if low_priority {
        let _ = std::process::Command::new("renice")
            .args(["-n", "10", "-p", &encoder.id().unwrap().to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    let mut decoder = match crate::ffmpeg::ffmpeg_command()
        .kill_on_drop(true)
        .args([
            "-v",
            "error",
            "-f",
            "webp_pipe",
            "-i",
            "pipe:0",
            "-vf",
            &format!("format=rgba,pad={padded_w}:{padded_h}:0:0:color=black@0"),
            // FFmpeg 4.4 (Ubuntu 22.04 CI) lacks -fps_mode; -vsync 0 is equivalent.
            "-vsync",
            "0",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgba",
            "pipe:1",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            let _ = encoder.kill().await;
            let _ = encoder.wait().await;
            let _ = encoder_reader.join();
            return Err(format!("Cannot start WebP decoder: {error}"));
        }
    };
    let decoder_progress = crate::encoder::FfmpegProgress::new();
    let decoder_reader = crate::encoder::spawn_stderr_reader(
        decoder.stderr.take().unwrap(),
        &decoder_progress,
        None,
        "WebP decoder",
    );
    let mut pauses = [
        crate::process_control::PauseGuard::new(encoder.id().unwrap()),
        crate::process_control::PauseGuard::new(decoder.id().unwrap()),
    ];
    sink.log(&format!(
        "  WebP encoder PID: {}; decoder PID: {}",
        encoder.id().unwrap(),
        decoder.id().unwrap()
    ));
    let mut enc_stdin = encoder.stdin.take();
    let mut dec_stdout = decoder.stdout.take();
    let mut dec_stdin = decoder.stdin.take().unwrap();
    let payloads = frames.clone();
    // Share compressed frame data; wrap one frame at a time rather than
    // duplicating all compressed payloads in a second in-memory collection.
    let mut writer = Some(tokio::spawn(async move {
        for frame in payloads.iter() {
            dec_stdin
                .write_all(&wrap_as_standalone_webp(&frame.data))
                .await
                .map_err(|e| e.to_string())?;
        }
        Ok::<_, String>(())
    }));
    let mut extra_reader = None;
    let result: Result<(i32, u64), String> = async {
        let mut encoded_frames = 0u64;
        for (index, frame) in frames.iter().enumerate() {
            if frame.width > padded_w || frame.height > padded_h {
                return Err("WebP frame exceeds canvas dimensions".into());
            }
            let mut rgba = vec![0u8; frame_byte_size];
            interruptible(
                async {
                    dec_stdout
                        .as_mut()
                        .unwrap()
                        .read_exact(&mut rgba)
                        .await
                        .map_err(|e| format!("Incomplete WebP frame {}: {e}", index + 1))
                },
                batch_control,
                &mut pauses,
            )
            .await?;
            // Decode every fragment into a fixed-size padded buffer, then
            // take its actual rectangle for the existing WebP compositing.
            let mut fragment = Vec::with_capacity(frame.width as usize * frame.height as usize * 4);
            for row in 0..frame.height as usize {
                let start = row * padded_w as usize * 4;
                fragment.extend_from_slice(&rgba[start..start + frame.width as usize * 4]);
            }
            if !frame.blend {
                canvas.clear_rect(frame.x_offset, frame.y_offset, frame.width, frame.height);
            }
            canvas.composite(
                &fragment,
                frame.x_offset,
                frame.y_offset,
                frame.width,
                frame.height,
                frame.blend,
            );
            let repeats = ((frame.duration_ms as f64 / 20.0).round() as u32).max(1);
            for _ in 0..repeats {
                interruptible(
                    async {
                        enc_stdin
                            .as_mut()
                            .unwrap()
                            .write_all(canvas.as_rgba())
                            .await
                            .map_err(|e| format!("WebP encoder pipe failed: {e}"))
                    },
                    batch_control,
                    &mut pauses,
                )
                .await?;
                encoded_frames += 1;
            }
            if frame.dispose {
                canvas.clear_rect(frame.x_offset, frame.y_offset, frame.width, frame.height);
            }
            let fraction = (index + 1) as f64 / frames.len() as f64;
            let duration = info.total_duration_ms as f64 / 1000.0;
            sink.file_progress(
                (fraction * 100.0).min(99.9),
                fraction * duration,
                duration,
                None,
            );
        }
        drop(enc_stdin.take());
        let mut remaining = dec_stdout.take().unwrap();
        extra_reader = Some(tokio::spawn(async move {
            let mut count = 0;
            let mut bytes = [0u8; 8192];
            loop {
                let n = remaining
                    .read(&mut bytes)
                    .await
                    .map_err(|e| e.to_string())?;
                if n == 0 {
                    break;
                }
                count += n;
            }
            Ok::<_, String>(count)
        }));
        interruptible(
            async { writer.as_mut().unwrap().await.map_err(|e| e.to_string())? },
            batch_control,
            &mut pauses,
        )
        .await?;
        drop(writer.take());
        let decoded = interruptible(
            async { decoder.wait().await.map_err(|e| e.to_string()) },
            batch_control,
            &mut pauses,
        )
        .await?;
        let encoded = interruptible(
            async { encoder.wait().await.map_err(|e| e.to_string()) },
            batch_control,
            &mut pauses,
        )
        .await?;
        let extra = extra_reader
            .take()
            .unwrap()
            .await
            .map_err(|e| e.to_string())??;
        if !decoded.success() {
            return Err("WebP decoder failed".into());
        }
        if extra != 0 {
            return Err("WebP decoder produced unexpected extra frames".into());
        }
        Ok((encoded.code().unwrap_or(-1), encoded_frames))
    }
    .await;
    // Never await a blocked feeder before closing/killing its subprocess.
    for pause in &mut pauses {
        let _ = pause.set(false);
    }
    if result.is_err() {
        let _ = encoder.kill().await;
        let _ = decoder.kill().await;
    }
    drop(enc_stdin);
    drop(dec_stdout);
    if let Some(task) = writer.take() {
        task.abort();
        if !task.is_finished() {
            let _ = task.await;
        }
    }
    if let Some(task) = extra_reader.take() {
        task.abort();
        if !task.is_finished() {
            let _ = task.await;
        }
    }
    let _ = decoder.wait().await;
    let _ = encoder.wait().await;
    let _ = decoder_reader.join();
    let _ = encoder_reader.join();
    let cancelled = batch_control.should_cancel_current() || batch_control.should_cancel_all();
    if cancelled {
        return Ok(TranscodeResult {
            exit_code: -1,
            was_cancelled: true,
            frame_count: 0,
        });
    }
    match result {
        Ok((exit_code, frame_count)) => {
            if exit_code != 0 {
                sink.log(&format!(
                    "  WebP encoder failed: {}",
                    encoder_progress.stderr_tail()
                ));
            } else {
                let duration = info.total_duration_ms as f64 / 1000.0;
                sink.file_progress(100.0, duration, duration, None);
            }
            Ok(TranscodeResult {
                exit_code,
                was_cancelled: false,
                frame_count,
            })
        }
        Err(error) => Err(format!(
            "{error}\nDecoder: {}\nEncoder: {}",
            decoder_progress.stderr_tail(),
            encoder_progress.stderr_tail()
        )),
    }
}

/// Result from the animated WebP transcode pipeline.
pub struct TranscodeResult {
    pub exit_code: i32,
    pub was_cancelled: bool,
    pub frame_count: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    fn animation(path: &Path) {
        let result = std::process::Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=128x96:rate=5:duration=1",
                "-c:v",
                "libwebp_anim",
                "-lossless",
                "1",
                "-loop",
                "0",
            ])
            .arg(path)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    #[tokio::test(flavor = "multi_thread")]
    async fn animation_completes_and_encoder_failure_does_not_deadlock() {
        let root = tempfile::tempdir().unwrap();
        let input = root.path().join("animated.webp");
        animation(&input);
        for codec in ["libx264", "invalid_webp_test_encoder"] {
            let output = root.path().join(format!("{codec}.mkv"));
            let args = ["-c:v", codec, "-preset", "ultrafast"].map(String::from);
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                transcode_animated_webp(
                    input.to_str().unwrap(),
                    output.to_str().unwrap(),
                    &args,
                    1,
                    false,
                    &crate::test_helpers::NoopSink,
                    &crate::test_helpers::NoopBatchControl,
                ),
            )
            .await
            .expect("WebP pipeline deadlocked");
            if codec == "libx264" {
                let result = result.unwrap();
                assert_eq!(result.exit_code, 0);
                assert_eq!(result.frame_count, 50);
                let scan = std::process::Command::new("ffprobe")
                    .args([
                        "-v",
                        "error",
                        "-count_frames",
                        "-select_streams",
                        "v:0",
                        "-show_entries",
                        "stream=nb_read_frames,width,height,pix_fmt",
                        "-of",
                        "json",
                    ])
                    .arg(&output)
                    .output()
                    .unwrap();
                assert!(scan.status.success());
                let json: serde_json::Value = serde_json::from_slice(&scan.stdout).unwrap();
                assert_eq!(json["streams"][0]["nb_read_frames"], "50");
                assert_eq!(json["streams"][0]["width"], 128);
                assert_eq!(json["streams"][0]["pix_fmt"], "yuv420p");
            } else {
                assert!(result.is_err() || result.is_ok_and(|r| r.exit_code != 0));
            }
        }
    }
    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread")]
    async fn cancelling_paused_animation_reaps_both_children() {
        use std::sync::{atomic::Ordering, Arc};
        let root = tempfile::tempdir().unwrap();
        let input = root.path().join("animated.webp");
        animation(&input);
        let output = root.path().join("cancelled.mkv");
        let sink = Arc::new(crate::test_helpers::RecordingSink::new());
        let control = Arc::new(crate::test_helpers::CancellableBatchControl::new());
        control.paused.store(true, Ordering::SeqCst);
        let task = {
            let sink = sink.clone();
            let control = control.clone();
            tokio::spawn(async move {
                transcode_animated_webp(
                    input.to_str().unwrap(),
                    output.to_str().unwrap(),
                    &["-c:v".into(), "libx264".into()],
                    1,
                    false,
                    &sink,
                    &*control,
                )
                .await
            })
        };
        let pids = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if let Some(line) = sink
                    .take_logs()
                    .into_iter()
                    .find(|line| line.contains("WebP encoder PID:"))
                {
                    break line
                        .split(|c: char| !c.is_ascii_digit())
                        .filter_map(|v| v.parse::<u32>().ok())
                        .collect::<Vec<_>>();
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        control.cancel_current.store(true, Ordering::SeqCst);
        let result = tokio::time::timeout(std::time::Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(result.was_cancelled);
        assert_eq!(pids.len(), 2);
        for pid in pids {
            assert!(!Path::new(&format!("/proc/{pid}")).exists());
        }
    }
    #[test]
    fn raw_rgba_vaapi_uses_verified_device_and_nv12_upload() {
        let args = crate::encoder::raw_rgba_encoder_args(
            "out.mkv",
            &["-c:v".into(), "hevc_vaapi".into()],
            128,
            96,
            50.0,
            1,
            Some("/test/dri/renderD177"),
        );
        let device = args.iter().position(|v| v == "-vaapi_device").unwrap();
        assert_eq!(args[device + 1], "/test/dri/renderD177");
        assert!(device < args.iter().position(|v| v == "-i").unwrap());
        let filter = &args[args.iter().position(|v| v == "-vf").unwrap() + 1];
        assert!(filter.contains("format=nv12,hwupload"));
        assert!(args.windows(2).any(|p| p == ["-pixel_format", "rgba"]));
        assert!(!args.windows(2).any(|p| p == ["-pix_fmt", "yuv420p"]));
    }
    #[tokio::test(flavor = "multi_thread")]
    async fn corrupt_frame_fails_instead_of_padding_successful_black_frames() {
        let root = tempfile::tempdir().unwrap();
        let input = root.path().join("corrupt.webp");
        animation(&input);
        let mut bytes = std::fs::read(&input).unwrap();
        let payload = bytes.windows(4).position(|v| v == b"VP8L").unwrap() + 8;
        bytes[payload..payload + 5].fill(0);
        std::fs::write(&input, &bytes).unwrap();
        let output = root.path().join("out.mkv");
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            transcode_animated_webp(
                input.to_str().unwrap(),
                output.to_str().unwrap(),
                &["-c:v".into(), "libx264".into()],
                1,
                false,
                &crate::test_helpers::NoopSink,
                &crate::test_helpers::NoopBatchControl,
            ),
        )
        .await
        .unwrap();
        assert!(result.is_err());
        assert_eq!(std::fs::read(input).unwrap(), bytes);
    }

    #[test]
    fn test_canvas_new_overflow_returns_err() {
        // u32::MAX * u32::MAX * 4 overflows usize — must return Err.
        let result = Canvas::new(u32::MAX, u32::MAX);
        assert!(result.is_err());
    }

    #[test]
    fn test_canvas_new_valid_dimensions() {
        let result = Canvas::new(100, 100);
        assert!(result.is_ok());
        let canvas = result.unwrap();
        assert_eq!(canvas.width, 100);
        assert_eq!(canvas.height, 100);
        assert_eq!(canvas.pixels.len(), 100 * 100 * 4);
    }

    #[test]
    fn test_canvas_new_zero_dimension() {
        // Zero width or height produces a zero-length pixel buffer.
        let result = Canvas::new(0, 100);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().pixels.len(), 0);
    }

    #[test]
    fn test_canvas_clear() {
        let mut canvas = Canvas::new(4, 4).unwrap();
        canvas.pixels.fill(0xFF);
        canvas.clear();
        assert!(canvas.pixels.iter().all(|&b| b == 0));
    }

    #[test]
    fn test_canvas_clear_rect() {
        let mut canvas = Canvas::new(4, 4).unwrap();
        canvas.pixels.fill(0xFF);
        canvas.clear_rect(1, 1, 2, 2);

        for row in 0..4u32 {
            for col in 0..4u32 {
                let idx = ((row * 4 + col) * 4) as usize;
                let pixel = &canvas.pixels[idx..idx + 4];
                if (1..3).contains(&row) && (1..3).contains(&col) {
                    assert!(
                        pixel.iter().all(|&b| b == 0),
                        "pixel ({col},{row}) should be zeroed"
                    );
                } else {
                    assert!(
                        pixel.iter().all(|&b| b == 0xFF),
                        "pixel ({col},{row}) should still be 0xFF"
                    );
                }
            }
        }
    }

    #[test]
    fn test_canvas_composite_overwrite() {
        let mut canvas = Canvas::new(4, 4).unwrap();
        // Canvas starts all zeros.
        // Create a 2x2 frame of all 0xFF (including alpha=255).
        let frame = vec![0xFFu8; 2 * 2 * 4];
        canvas.composite(&frame, 1, 1, 2, 2, false);

        for row in 0..4u32 {
            for col in 0..4u32 {
                let idx = ((row * 4 + col) * 4) as usize;
                let pixel = &canvas.pixels[idx..idx + 4];
                if (1..3).contains(&row) && (1..3).contains(&col) {
                    assert!(
                        pixel.iter().all(|&b| b == 0xFF),
                        "pixel ({col},{row}) should be 0xFF"
                    );
                } else {
                    assert!(
                        pixel.iter().all(|&b| b == 0),
                        "pixel ({col},{row}) should be 0"
                    );
                }
            }
        }
    }

    #[test]
    fn test_canvas_composite_blend_opaque() {
        let mut canvas = Canvas::new(4, 4).unwrap();
        // Fill background with known color (50, 100, 150, 255).
        for pixel in canvas.pixels.chunks_exact_mut(4) {
            pixel.copy_from_slice(&[50, 100, 150, 255]);
        }
        // Composite a fully opaque 2x2 frame (200, 200, 200, 255).
        let frame: Vec<u8> = [200u8, 200, 200, 255].repeat(2 * 2);
        canvas.composite(&frame, 1, 1, 2, 2, true);

        for row in 0..4u32 {
            for col in 0..4u32 {
                let idx = ((row * 4 + col) * 4) as usize;
                let pixel = &canvas.pixels[idx..idx + 4];
                if (1..3).contains(&row) && (1..3).contains(&col) {
                    assert_eq!(
                        pixel,
                        &[200, 200, 200, 255],
                        "pixel ({col},{row}) should be overwritten by opaque source"
                    );
                } else {
                    assert_eq!(
                        pixel,
                        &[50, 100, 150, 255],
                        "pixel ({col},{row}) should be unchanged"
                    );
                }
            }
        }
    }

    #[test]
    fn test_canvas_composite_blend_transparent() {
        let mut canvas = Canvas::new(4, 4).unwrap();
        // Fill background with known color.
        for pixel in canvas.pixels.chunks_exact_mut(4) {
            pixel.copy_from_slice(&[80, 120, 160, 255]);
        }
        // Composite a fully transparent 2x2 frame (alpha=0).
        let frame: Vec<u8> = [200u8, 200, 200, 0].repeat(2 * 2);
        canvas.composite(&frame, 1, 1, 2, 2, true);

        // Destination should be completely unchanged.
        for pixel in canvas.pixels.chunks_exact(4) {
            assert_eq!(pixel, &[80, 120, 160, 255]);
        }
    }

    #[test]
    fn test_canvas_composite_boundary_clip() {
        let mut canvas = Canvas::new(4, 4).unwrap();
        // Composite a 3x3 frame at offset (3,3) — only pixel (3,3) overlaps.
        let frame: Vec<u8> = vec![0xFF; 3 * 3 * 4];
        canvas.composite(&frame, 3, 3, 3, 3, false);

        for row in 0..4u32 {
            for col in 0..4u32 {
                let idx = ((row * 4 + col) * 4) as usize;
                let pixel = &canvas.pixels[idx..idx + 4];
                if row == 3 && col == 3 {
                    assert!(
                        pixel.iter().all(|&b| b == 0xFF),
                        "pixel (3,3) should be written"
                    );
                } else {
                    assert!(
                        pixel.iter().all(|&b| b == 0),
                        "pixel ({col},{row}) should be unchanged"
                    );
                }
            }
        }
    }

    #[test]
    fn test_canvas_composite_partial_alpha() {
        let mut canvas = Canvas::new(4, 4).unwrap();
        // Fill background with (100, 100, 100, 255).
        for pixel in canvas.pixels.chunks_exact_mut(4) {
            pixel.copy_from_slice(&[100, 100, 100, 255]);
        }
        // Composite 1x1 pixel with (200, 200, 200, 128) at (0,0) with blend.
        let frame = vec![200u8, 200, 200, 128];
        canvas.composite(&frame, 0, 0, 1, 1, true);

        let pixel = &canvas.pixels[0..4];
        // The blending formula in composite:
        //   sa=128, da=255, inv_sa=127
        //   out_c = (sc*sa + dc*da*inv_sa/255) / max(sa + da*inv_sa/255, 1)
        //         = (200*128 + 100*255*127/255) / max(128 + 255*127/255, 1)
        //         = (25600 + 12700) / (128 + 127) = 38300/255 ≈ 150
        //   out_a = sa + da*inv_sa/255 = 128 + 127 = 255
        // Allow tolerance of ±2 for integer rounding.
        assert!(
            (pixel[0] as i32 - 150).abs() <= 2,
            "red channel: expected ~150, got {}",
            pixel[0]
        );
        assert!(
            (pixel[1] as i32 - 150).abs() <= 2,
            "green channel: expected ~150, got {}",
            pixel[1]
        );
        assert!(
            (pixel[2] as i32 - 150).abs() <= 2,
            "blue channel: expected ~150, got {}",
            pixel[2]
        );
        assert_eq!(pixel[3], 255, "alpha should be 255");
    }

    // ── Property-based tests ─────────────────────────────────────

    use proptest::prelude::*;

    proptest! {
        #[test]
        fn canvas_new_no_panic(w in 0u32..=8192, h in 0u32..=8192) {
            // Should never panic - only Ok or Err
            let _ = Canvas::new(w, h);
        }

        #[test]
        fn canvas_composite_no_panic(
            canvas_w in 1u32..=64,
            canvas_h in 1u32..=64,
            frame_w in 1u32..=64,
            frame_h in 1u32..=64,
            x in 0u32..=128,
            y in 0u32..=128,
            blend in any::<bool>(),
        ) {
            if let Ok(mut canvas) = Canvas::new(canvas_w, canvas_h) {
                let frame_size = (frame_w as usize) * (frame_h as usize) * 4;
                let frame_data = vec![128u8; frame_size];
                // Should never panic even with out-of-bounds coordinates
                canvas.composite(&frame_data, x, y, frame_w, frame_h, blend);
            }
        }

        #[test]
        fn canvas_clear_rect_no_panic(
            canvas_w in 1u32..=64,
            canvas_h in 1u32..=64,
            x in 0u32..=128,
            y in 0u32..=128,
            w in 0u32..=128,
            h in 0u32..=128,
        ) {
            if let Ok(mut canvas) = Canvas::new(canvas_w, canvas_h) {
                // Should never panic even with out-of-bounds rectangle
                canvas.clear_rect(x, y, w, h);
            }
        }
    }
}

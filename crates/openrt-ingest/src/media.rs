//! Media intake (M9.4): ffprobe probing, ffmpeg frame/audio decoding, and
//! honest document builders. Audio transcripts come only from a real ASR
//! engine (structured `MODEL_MISSING` otherwise); video text comes from real
//! ffmpeg-sampled frames OCR'd by tesseract, each block carrying its position
//! on the media timeline (`t_start`/`t_end`). Nothing here is inferred.

use std::process::Command;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::asr::WhisperCli;
use crate::capabilities::{media_decode_capability, media_probe_capability, ocr_capability};
use crate::normalize::{
    Block, BlockKind, BlockSource, ExtractMethod, MediaMeta, Modality, NormalizedDocumentV1,
};
use crate::ocr::{temp_path, OcrError, Psm, TempArtifact, Tesseract};
use crate::{Confidence, FormatKind};

/// Seconds between frame samples when a video is longer than one slot.
pub const FRAME_INTERVAL_SECS: f64 = 2.0;
/// Hard cap on sampled frames per video (bounding work per document).
pub const MAX_FRAME_SAMPLES: usize = 20;

/// Structured media failure: absent backend, failed probe/decode, or a
/// carried OCR/ASR refusal. Never content.
#[derive(Debug, Error)]
pub enum MediaError {
    #[error("no `{backend}` backend on PATH: {reason}")]
    NoBackend {
        backend: &'static str,
        reason: String,
    },
    #[error("media probe failed: {0}")]
    Probe(String),
    #[error("media decode failed: {0}")]
    Decode(String),
    #[error(transparent)]
    Ocr(#[from] OcrError),
    #[error(transparent)]
    Asr(#[from] crate::asr::AsrError),
}

/// What ffprobe genuinely measured for one input.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct MediaProbe {
    pub duration_secs: Option<f64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub frame_rate: Option<f32>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub channels: Option<u32>,
    pub sample_rate: Option<u32>,
    pub has_audio: bool,
    pub has_video: bool,
}

impl MediaProbe {
    /// Honest metadata projection for a document of `kind` — only fields the
    /// probe actually measured for that stream type are carried over.
    pub fn to_media_meta(&self, kind: FormatKind) -> MediaMeta {
        if kind == FormatKind::Audio {
            MediaMeta {
                duration_secs: self.duration_secs,
                channels: self.channels,
                sample_rate: self.sample_rate,
                codec: self.audio_codec.clone(),
                ..MediaMeta::default()
            }
        } else {
            MediaMeta {
                duration_secs: self.duration_secs,
                width: self.width,
                height: self.height,
                frame_rate: self.frame_rate,
                codec: self.video_codec.clone(),
                ..MediaMeta::default()
            }
        }
    }
}

/// Parse `ffprobe -print_format json -show_format -show_streams` output.
/// Pure and unit-tested with fixture payloads; a payload with no streams is
/// a structured failure (never an empty "success").
pub fn parse_ffprobe_json(json: &str) -> Result<MediaProbe, MediaError> {
    let v: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| MediaError::Probe(format!("output is not JSON: {e}")))?;
    let mut probe = MediaProbe::default();
    if let Some(duration) = v.get("format").and_then(|f| f.get("duration")) {
        probe.duration_secs = duration
            .as_str()
            .and_then(|s| s.parse::<f64>().ok())
            .or_else(|| duration.as_f64());
    }
    let streams = v
        .get("streams")
        .and_then(|s| s.as_array())
        .ok_or_else(|| MediaError::Probe("ffprobe output has no `streams` array".into()))?;
    for st in streams {
        match st.get("codec_type").and_then(|c| c.as_str()) {
            Some("video") if !probe.has_video => {
                probe.has_video = true;
                probe.video_codec = str_field(st, "codec_name");
                probe.width = uint_field(st, "width");
                probe.height = uint_field(st, "height");
                probe.frame_rate = st
                    .get("avg_frame_rate")
                    .and_then(|r| r.as_str())
                    .and_then(parse_rate);
            }
            Some("audio") if !probe.has_audio => {
                probe.has_audio = true;
                probe.audio_codec = str_field(st, "codec_name");
                probe.channels = uint_field(st, "channels");
                probe.sample_rate = st
                    .get("sample_rate")
                    .and_then(|r| r.as_str())
                    .and_then(|s| s.parse().ok())
                    .or_else(|| uint_field(st, "sample_rate"));
            }
            _ => {}
        }
    }
    if !probe.has_audio && !probe.has_video {
        return Err(MediaError::Probe(
            "ffprobe reported no audio/video streams".into(),
        ));
    }
    Ok(probe)
}

/// Run ffprobe over raw media bytes (temp input) and return the parsed probe.
pub fn probe_media_bytes(bytes: &[u8]) -> Result<MediaProbe, MediaError> {
    let cap = media_probe_capability().ok_or_else(|| MediaError::NoBackend {
        backend: "ffprobe",
        reason: "media probing unavailable".into(),
    })?;
    let input = TempArtifact::persist("media", "bin", bytes)?;
    let out = Command::new(&cap.binary)
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
        ])
        .arg(input.path())
        .output()
        .map_err(|e| MediaError::Probe(format!("failed to run ffprobe: {e}")))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(MediaError::Probe(format!(
            "ffprobe exited non-zero: {}",
            stderr.lines().take(4).collect::<Vec<_>>().join(" ")
        )));
    }
    parse_ffprobe_json(&String::from_utf8_lossy(&out.stdout))
}

/// One sampled video frame with its position on the media timeline.
#[derive(Debug, Clone, PartialEq)]
pub struct FrameSample {
    /// Sample slot start (seconds, as seeked by ffmpeg).
    pub t_start: f64,
    /// Sample slot end: `min(t_start + FRAME_INTERVAL_SECS, duration)` —
    /// the window this frame represents. Never past the real duration.
    pub t_end: f64,
    /// Raw PNG bytes ffmpeg produced for this frame.
    pub png: Vec<u8>,
}

/// Sample frames on a fixed interval: `n = min(MAX_FRAME_SAMPLES,
/// ceil(duration / interval))` slots at `duration * k / (n + 1)` (midpoints,
/// so edge-black frames are avoided). Fails closed when ffprobe reported no
/// duration — timestamps would otherwise be invented.
pub fn sample_video_frames(
    bytes: &[u8],
    probe: &MediaProbe,
) -> Result<Vec<FrameSample>, MediaError> {
    let duration = probe
        .duration_secs
        .filter(|d| d.is_finite() && *d > 0.0)
        .ok_or_else(|| {
            MediaError::Probe(
                "ffprobe reported no duration — frame sample timestamps would be invented".into(),
            )
        })?;
    let cap = media_decode_capability().ok_or_else(|| MediaError::NoBackend {
        backend: "ffmpeg",
        reason: "video frame sampling unavailable".into(),
    })?;
    let n = (duration / FRAME_INTERVAL_SECS).ceil().max(1.0) as usize;
    let n = n.min(MAX_FRAME_SAMPLES);
    let input = TempArtifact::persist("video", "bin", bytes)?;
    let mut frames = Vec::with_capacity(n);
    for k in 1..=n {
        let t = duration * k as f64 / (n + 1) as f64;
        let out_path = temp_path("frame", "png");
        let status = Command::new(&cap.binary)
            .arg("-v")
            .arg("error")
            .arg("-y")
            .arg("-ss")
            .arg(format!("{t:.3}"))
            .arg("-i")
            .arg(input.path())
            .arg("-frames:v")
            .arg("1")
            .arg("-f")
            .arg("image2")
            .arg(&out_path)
            .status()
            .map_err(|e| MediaError::Decode(format!("failed to run ffmpeg: {e}")))?;
        if !status.success() {
            let _ = std::fs::remove_file(&out_path);
            return Err(MediaError::Decode(format!(
                "ffmpeg frame extraction failed at t={t:.3}s"
            )));
        }
        let png = std::fs::read(&out_path)
            .map_err(|e| MediaError::Decode(format!("read sampled frame: {e}")))?;
        let _ = std::fs::remove_file(&out_path);
        frames.push(FrameSample {
            t_start: t,
            t_end: (t + FRAME_INTERVAL_SECS).min(duration),
            png,
        });
    }
    Ok(frames)
}

/// Decode any audio container to the 16 kHz mono WAV an ASR engine expects.
pub fn decode_to_wav(bytes: &[u8]) -> Result<TempArtifact, MediaError> {
    let cap = media_decode_capability().ok_or_else(|| MediaError::NoBackend {
        backend: "ffmpeg",
        reason: "audio decode unavailable".into(),
    })?;
    let input = TempArtifact::persist("media", "bin", bytes)?;
    let out_path = temp_path("audio", "wav");
    let status = Command::new(&cap.binary)
        .arg("-v")
        .arg("error")
        .arg("-y")
        .arg("-i")
        .arg(input.path())
        .arg("-vn")
        .arg("-ac")
        .arg("1")
        .arg("-ar")
        .arg("16000")
        .arg("-f")
        .arg("wav")
        .arg(&out_path)
        .status()
        .map_err(|e| MediaError::Decode(format!("failed to run ffmpeg: {e}")))?;
    if !status.success() {
        let _ = std::fs::remove_file(&out_path);
        return Err(MediaError::Decode(
            "ffmpeg audio decode exited non-zero".into(),
        ));
    }
    Ok(TempArtifact::taken(out_path))
}

/// Composite, fully honest extractor label: every backend that actually ran,
/// with the version it reported (`ffprobe/9.0.1; ffmpeg/9.0.1; tesseract|5.5.3`).
fn extractor_label(kind: FormatKind) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(p) = media_probe_capability() {
        parts.push(format!(
            "ffprobe/{}",
            p.version.as_deref().unwrap_or("unknown")
        ));
    }
    if let Some(d) = media_decode_capability() {
        parts.push(format!(
            "ffmpeg/{}",
            d.version.as_deref().unwrap_or("unknown")
        ));
    }
    if kind == FormatKind::Audio {
        if let Some(a) = crate::capabilities::audio_capability() {
            parts.push(format!(
                "{}|{}",
                a.name,
                a.version.as_deref().unwrap_or("unknown")
            ));
        }
    } else if let Some(o) = ocr_capability() {
        parts.push(format!(
            "{}|{}",
            o.name,
            o.version.as_deref().unwrap_or("unknown")
        ));
    }
    parts.join("; ")
}

/// Video intake: ffprobe metadata + ffmpeg frame sampling + per-frame OCR.
/// Each block carries its sampling window on the media timeline; text is
/// only ever what tesseract read from a real frame.
pub fn ingest_video(
    bytes: &[u8],
    confidence: Confidence,
) -> Result<NormalizedDocumentV1, MediaError> {
    let probe = probe_media_bytes(bytes)?;
    let samples = sample_video_frames(bytes, &probe)?;
    let t = Tesseract::detect().ok_or_else(|| OcrError::BackendMissing {
        backend: "tesseract".into(),
        capability: "ocr".into(),
        reason: "no `tesseract` binary on PATH — video frame text extraction unavailable".into(),
    })?;
    let mut blocks: Vec<Block> = Vec::new();
    for frame in &samples {
        let page = t.recognize_bytes(&frame.png, "png", Psm::Auto)?;
        for mut b in page.to_blocks(1) {
            if b.text.trim().is_empty() {
                continue;
            }
            b.page = None;
            b.source.page = None;
            b.source.method = ExtractMethod::Video;
            b.source.t_start = Some(frame.t_start);
            b.source.t_end = Some(frame.t_end);
            blocks.push(b);
        }
    }
    // Frame-local keys restart per frame; re-key globally in reading order.
    for (i, b) in blocks.iter_mut().enumerate() {
        b.key = crate::block_key(i);
    }
    let content_sha256 = crate::sha256_hex_for(bytes);
    Ok(NormalizedDocumentV1 {
        schema_version: 1,
        doc_id: format!("doc/{content_sha256}"),
        version: 1,
        content_sha256,
        format: FormatKind::Video,
        size_bytes: bytes.len() as u64,
        pages: 0,
        blocks,
        created_at: openrt_core::now().to_rfc3339(),
        extractor: extractor_label(FormatKind::Video),
        detection_confidence: confidence,
        modality: Modality::Video,
        media: Some(probe.to_media_meta(FormatKind::Video)),
    })
}

/// Audio intake: ffprobe metadata + ffmpeg decode + real ASR. Without an
/// engine (or without a model) this is a structured `MODEL_MISSING`
/// refusal — never a transcript-shaped guess.
pub fn ingest_audio(
    bytes: &[u8],
    confidence: Confidence,
) -> Result<NormalizedDocumentV1, MediaError> {
    let probe = probe_media_bytes(bytes)?;
    let wav = decode_to_wav(bytes)?;
    let engine = WhisperCli::from_env()?;
    let work = temp_path("asr-work", "dir").with_extension("");
    std::fs::create_dir_all(&work).map_err(|e| MediaError::Probe(format!("asr work dir: {e}")))?;
    let segments = engine.transcribe(wav.path(), &work);
    let _ = std::fs::remove_dir_all(&work);
    let segments = segments?;
    let mut blocks: Vec<Block> = Vec::new();
    for (i, s) in segments.iter().enumerate() {
        if s.text.trim().is_empty() {
            continue;
        }
        blocks.push(Block {
            key: crate::block_key(i),
            kind: BlockKind::Paragraph,
            page: None,
            text: s.text.clone(),
            table: None,
            source: BlockSource {
                method: ExtractMethod::Audio,
                page: None,
                bbox: None,
                confidence: None,
                t_start: Some(s.t_start),
                t_end: Some(s.t_end),
            },
        });
    }
    let content_sha256 = crate::sha256_hex_for(bytes);
    Ok(NormalizedDocumentV1 {
        schema_version: 1,
        doc_id: format!("doc/{content_sha256}"),
        version: 1,
        content_sha256,
        format: FormatKind::Audio,
        size_bytes: bytes.len() as u64,
        pages: 0,
        blocks,
        created_at: openrt_core::now().to_rfc3339(),
        extractor: extractor_label(FormatKind::Audio),
        detection_confidence: confidence,
        modality: Modality::Audio,
        media: Some(probe.to_media_meta(FormatKind::Audio)),
    })
}

fn str_field(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(|x| x.as_str())
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

fn uint_field(v: &serde_json::Value, key: &str) -> Option<u32> {
    v.get(key)
        .and_then(|x| x.as_u64())
        .map(|n| n as u32)
        .or_else(|| {
            v.get(key)
                .and_then(|x| x.as_str())
                .and_then(|s| s.parse().ok())
        })
}

/// Parse an ffprobe rational like `"30000/1001"`; `0/0` (unknown) is `None`.
fn parse_rate(s: &str) -> Option<f32> {
    let (num, den) = s.split_once('/')?;
    let num: f32 = num.parse().ok()?;
    let den: f32 = den.parse().ok()?;
    if den == 0.0 || num <= 0.0 {
        return None;
    }
    Some(num / den)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FFPROBE_JSON: &str = r#"{
      "streams": [
        {"codec_type": "video", "codec_name": "h264", "width": 1200, "height": 400,
         "avg_frame_rate": "30000/1001"},
        {"codec_type": "audio", "codec_name": "aac", "channels": 2, "sample_rate": "44100"}
      ],
      "format": {"duration": "6.016000"}
    }"#;

    #[test]
    fn ffprobe_json_parsed_honestly() {
        let p = parse_ffprobe_json(FFPROBE_JSON).unwrap();
        assert_eq!(p.duration_secs, Some(6.016));
        assert_eq!(p.width, Some(1200));
        assert_eq!(p.height, Some(400));
        assert!((p.frame_rate.unwrap() - 29.97).abs() < 0.01);
        assert_eq!(p.video_codec.as_deref(), Some("h264"));
        assert_eq!(p.audio_codec.as_deref(), Some("aac"));
        assert_eq!(p.channels, Some(2));
        assert_eq!(p.sample_rate, Some(44100));
        assert!(p.has_audio && p.has_video);
    }

    #[test]
    fn unknown_frame_rate_is_not_guessed() {
        let p = parse_ffprobe_json(
            r#"{"streams":[{"codec_type":"video","avg_frame_rate":"0/0"}],
                "format":{"duration":"1.0"}}"#,
        )
        .unwrap();
        assert_eq!(p.frame_rate, None);
        assert_eq!(p.width, None, "absent fields stay absent");
    }

    #[test]
    fn no_streams_is_structured_failure() {
        let err = parse_ffprobe_json(r#"{"streams":[]}"#).unwrap_err();
        assert!(matches!(err, MediaError::Probe(_)), "{err:?}");
        assert!(err.to_string().contains("no audio/video streams"));
    }

    #[test]
    fn malformed_json_is_structured_failure() {
        assert!(matches!(
            parse_ffprobe_json("garbage"),
            Err(MediaError::Probe(_))
        ));
    }

    #[test]
    fn audio_meta_projection_keeps_only_audio_fields() {
        let p = parse_ffprobe_json(FFPROBE_JSON).unwrap();
        let audio = p.to_media_meta(FormatKind::Audio);
        assert_eq!(audio.channels, Some(2));
        assert_eq!(audio.width, None, "video dims are not claimed for audio");
        let video = p.to_media_meta(FormatKind::Video);
        assert_eq!(video.width, Some(1200));
        assert_eq!(video.channels, None);
        assert_eq!(video.codec.as_deref(), Some("h264"));
    }
}

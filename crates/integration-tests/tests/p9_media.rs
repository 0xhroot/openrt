//! Phase 9 (M9.4) — audio/video intake honesty, end-to-end:
//! * detection status for media tracks the *actual* ffprobe/ffmpeg/whisper
//!   capability gates on this host (never a fixed guess);
//! * audio intake without an ASR engine refuses with a structured
//!   `MODEL_MISSING` reason — no transcript-shaped fiction;
//! * video intake samples real frames through ffmpeg, OCRs them, and every
//!   block/chunk carries its position on the media timeline (t_start/t_end)
//!   with probed duration/dimensions in `MediaMeta`.
//!
//! Tests are gated on the host's real backends (production capability
//! probes): without them they skip with a message instead of faking media.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use openrt_config::{Config, RuntimeConfig};
use openrt_ingest::capabilities::{
    audio_capability, media_decode_capability, media_probe_capability, ocr_capability,
};
use openrt_ingest::chunk::{chunk_normalized, ChunkConfig};
use openrt_ingest::jail::IngestJail;
use openrt_ingest::{detect, ExtractMethod, FormatKind, Modality};
use openrt_models::mock::MockProvider;

/// Committed fixture: 1200x400 grayscale JPEG with "OPENRT" (verified
/// readable by tesseract) — used as video source frames so frame OCR is
/// deterministic.
const FIXTURE_JPEG: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/fixtures/ocr_text.jpg"
));

fn tmp_dir(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "openrt-p9-media-{prefix}-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn build(config: Config) -> Arc<openrt_runtime::Runtime> {
    openrt_runtime::RuntimeBuilder::new()
        .config(config)
        .store(Arc::new(openrt_persistence::MemoryStore::new()))
        .with_provider(Arc::new(MockProvider::tool_sequence("mock")))
        .build()
        .await
        .unwrap()
}

fn runtime_config(ws: &Path) -> Config {
    Config {
        runtime: RuntimeConfig {
            workspace_dir: Some(ws.to_path_buf()),
            ..RuntimeConfig::default()
        },
        ..Config::default()
    }
}

/// True when this host can genuinely probe + sample + OCR video frames.
fn video_or_skip() -> bool {
    let have = media_probe_capability().is_some()
        && media_decode_capability().is_some()
        && ocr_capability().is_some();
    if !have {
        eprintln!(
            "SKIP: ffprobe/ffmpeg/tesseract gates not all met on this host — video extraction not faked"
        );
    }
    have
}

/// A minimal but structurally valid PCM WAV (0.5 s of silence): enough for
/// magic-byte detection without any backend.
fn synth_wav() -> Vec<u8> {
    let sample_rate: u32 = 8000;
    let data_len: u32 = sample_rate / 2 * 2; // 0.5 s, mono, 16-bit
    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&(sample_rate * 2).to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    wav.extend(std::iter::repeat(0u8).take(data_len as usize));
    wav
}

/// Render the fixture JPEG into a 6 s, 1 fps MP4 with ffmpeg (real frames,
/// no drawing fakery) — skipped when the decode gate is absent.
fn synth_video() -> Option<Vec<u8>> {
    let dir = tmp_dir("synth");
    let src = dir.join("frame.jpg");
    let out = dir.join("clip.mp4");
    std::fs::write(&src, FIXTURE_JPEG).unwrap();
    let status = std::process::Command::new(media_decode_capability()?.binary)
        .args(["-v", "error", "-y", "-loop", "1", "-i"])
        .arg(&src)
        .args(["-t", "6", "-r", "1", "-pix_fmt", "yuv420p"])
        .arg(&out)
        .status()
        .ok()?;
    let bytes = if status.success() {
        std::fs::read(&out).ok()
    } else {
        None
    };
    let _ = std::fs::remove_dir_all(&dir);
    bytes
}

/// M9.4 — detection status for media is exactly the capability gate: the
/// same host-evaluated condition as `supported_extraction`, with the
/// structured refusal reason naming the missing backend when unsupported.
#[test]
fn wav_detection_tracks_media_capability() {
    let wav = synth_wav();
    let r = detect(&wav, Some("clip.wav"));
    assert_eq!(r.kind, FormatKind::Audio);
    let gates_pass = media_probe_capability().is_some()
        && media_decode_capability().is_some()
        && audio_capability().is_some();
    assert_eq!(r.status.is_supported(), gates_pass);
    if !gates_pass {
        let reason = r.status.reason().expect("reason present").to_string();
        if audio_capability().is_none() {
            assert!(reason.contains("whisper"), "{reason}");
            assert!(reason.contains("MODEL_MISSING"), "{reason}");
        }
        assert!(!reason.is_empty());
    }
}

/// M9.4 — audio intake without an ASR engine refuses *structurally* through
/// the production corpus path: `UnsupportedFormat` carrying the capability
/// reason and an audit rejection. Nothing is stored, nothing invented.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audio_ingest_refuses_structured_without_asr() {
    if audio_capability().is_some() {
        eprintln!("SKIP: host has a whisper engine — structured-refusal branch not reachable");
        return;
    }
    let root = tmp_dir("audio");
    let rt = build(runtime_config(&root)).await;
    let jail = IngestJail::new(root.clone()).unwrap();
    std::fs::write(root.join("clip.wav"), synth_wav()).unwrap();

    let err = rt
        .corpus
        .ingest_path(&jail, Path::new("clip.wav"), "p9-test")
        .await
        .expect_err("audio without ASR must fail closed");
    match err {
        openrt_ingest::corpus::IngestError::UnsupportedFormat { format, reason } => {
            assert_eq!(format, "audio/*");
            assert!(reason.contains("whisper"), "{reason}");
            assert!(reason.contains("MODEL_MISSING"), "{reason}");
        }
        other => panic!("expected structured UnsupportedFormat, got {other:?}"),
    }
    // The refused file was not stored.
    let stored = rt.corpus.list().await.unwrap();
    assert!(
        stored.is_empty(),
        "refused audio must not be stored: {stored:?}"
    );
}

/// M9.4 — video intake: real ffmpeg frame sampling + real tesseract OCR.
/// Blocks and chunks carry honest timeline windows inside the probed
/// duration; metadata (duration/dimensions/codec) comes from ffprobe.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn video_frames_ocr_with_timeline_and_chunks() {
    if !video_or_skip() {
        return;
    }
    let Some(video) = synth_video() else {
        eprintln!("SKIP: ffmpeg could not synthesize the fixture video");
        return;
    };

    let r = detect(&video, Some("clip.mp4"));
    assert_eq!(r.kind, FormatKind::Video);
    assert!(r.status.is_supported(), "gates hold on this host: {r:?}");

    let root = tmp_dir("video");
    let rt = build(runtime_config(&root)).await;
    let jail = IngestJail::new(root.clone()).unwrap();
    std::fs::write(root.join("clip.mp4"), &video).unwrap();

    let out = rt
        .corpus
        .ingest_path(&jail, Path::new("clip.mp4"), "p9-test")
        .await
        .expect("video intake with all gates present must succeed");
    assert_eq!(out.state.label(), "stored");
    assert_eq!(out.doc.format.label(), "video/*");
    assert_eq!(out.doc.modality, Modality::Video);
    assert_eq!(out.doc.pages, 0, "media has no pages");

    // Probed metadata — ffprobe measurements, never guessed.
    let media = out.doc.media.as_ref().expect("ffprobe metadata stored");
    let duration = media.duration_secs.expect("duration probed");
    assert!((5.0..7.5).contains(&duration), "duration: {duration}");
    assert_eq!(media.width, Some(1200));
    assert_eq!(media.height, Some(400));
    assert!(media.codec.is_some(), "video codec recorded");
    assert_eq!(media.channels, None, "audio fields not claimed for video");

    // Every backend that actually ran is named in the extractor label.
    assert!(
        out.doc.extractor.contains("ffprobe/"),
        "{}",
        out.doc.extractor
    );
    assert!(
        out.doc.extractor.contains("ffmpeg/"),
        "{}",
        out.doc.extractor
    );
    assert!(
        out.doc.extractor.contains("tesseract|"),
        "{}",
        out.doc.extractor
    );

    // Real frame OCR text with full temporal + spatial provenance.
    assert!(!out.doc.blocks.is_empty(), "frame OCR produced no blocks");
    let mut keys = std::collections::HashSet::new();
    for b in &out.doc.blocks {
        assert!(keys.insert(b.key.clone()), "duplicate block key {}", b.key);
        assert_eq!(b.source.method, ExtractMethod::Video);
        assert!(b.page.is_none(), "media blocks are not page-bound");
        let t0 = b.source.t_start.expect("block t_start");
        let t1 = b.source.t_end.expect("block t_end");
        assert!(t0 <= t1);
        assert!((0.0..=duration).contains(&t0), "t_start {t0} outside media");
        assert!((0.0..=duration).contains(&t1), "t_end {t1} outside media");
        assert!(b.source.bbox.is_some(), "frame bbox provenance");
        let conf = b.source.confidence.expect("tesseract confidence");
        assert!((0.0..=1.0).contains(&conf));
        assert!(
            b.text.to_uppercase().contains("OPENRT"),
            "text: {:?}",
            b.text
        );
    }

    // Provenance survives storage.
    let shown = rt.corpus.inspect(&out.doc.doc_id).await.unwrap();
    assert_eq!(shown.blocks, out.doc.blocks);

    // Timestamped chunking: every chunk carries the span of its blocks.
    let chunks = chunk_normalized(&out.doc, &ChunkConfig::default()).expect("video doc chunks");
    assert!(!chunks.is_empty());
    for c in &chunks {
        let t0 = c.t_start.expect("chunk t_start propagated");
        let t1 = c.t_end.expect("chunk t_end propagated");
        assert!(t0 <= t1);
        assert!((0.0..=duration).contains(&t0));
        assert!((0.0..=duration).contains(&t1));
    }
}

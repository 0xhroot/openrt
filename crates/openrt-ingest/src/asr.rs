//! Speech-to-text (M9.4) behind a provider trait: a real whisper.cpp-family
//! engine when present, structured `MODEL_MISSING` / `BackendMissing`
//! otherwise. Nothing here ever invents a transcript — every segment carries
//! backend-reported timestamps and is only produced by an actual run.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Why a transcript could not be produced. Every variant is a structured,
/// machine-readable refusal (never invented content).
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AsrError {
    /// No ASR engine on `PATH` — transcript extraction unavailable.
    #[error("ASR backend missing: {reason}")]
    BackendMissing { reason: String },
    /// Engine present but no model file — still `MODEL_MISSING`, not a guess.
    #[error("ASR model missing: {reason}")]
    ModelMissing { reason: String },
    /// The engine ran and failed; stderr is carried for diagnosis.
    #[error("ASR backend failed: {0}")]
    Failed(String),
}

/// One transcribed span on the media timeline (seconds).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AsrSegment {
    pub t_start: f64,
    pub t_end: f64,
    pub text: String,
}

/// Parse whisper.cpp `--output-json` output into timeline segments.
/// Offsets are milliseconds (`{"from": 0, "to": 2500}`); whitespace-only
/// segments are dropped; a payload without a `transcription` array is a
/// structured failure, never an empty "success".
pub fn parse_whisper_json(json: &str) -> Result<Vec<AsrSegment>, AsrError> {
    let v: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| AsrError::Failed(format!("output is not JSON: {e}")))?;
    let arr = v
        .get("transcription")
        .and_then(|t| t.as_array())
        .ok_or_else(|| AsrError::Failed("whisper output has no `transcription` array".into()))?;
    let mut segments = Vec::new();
    for item in arr {
        let offsets = item
            .get("offsets")
            .ok_or_else(|| AsrError::Failed("whisper segment has no `offsets`".into()))?;
        let from_ms = offsets
            .get("from")
            .and_then(|x| x.as_i64())
            .ok_or_else(|| AsrError::Failed("segment offset `from` is not a number".into()))?;
        let to_ms = offsets
            .get("to")
            .and_then(|x| x.as_i64())
            .ok_or_else(|| AsrError::Failed("segment offset `to` is not a number".into()))?;
        let text = item.get("text").and_then(|t| t.as_str()).unwrap_or("");
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        segments.push(AsrSegment {
            t_start: from_ms.max(0) as f64 / 1000.0,
            t_end: to_ms.max(from_ms) as f64 / 1000.0,
            text: text.to_string(),
        });
    }
    Ok(segments)
}

/// The whisper.cpp CLI provider. Runs only against explicit paths (binary +
/// model) so tests can substitute a fake executable without touching `PATH`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhisperCli {
    pub binary: PathBuf,
    pub model: PathBuf,
}

impl WhisperCli {
    /// Discover a real engine: `whisper-cli`/`whisper-cpp`/`whisper` on
    /// `PATH` plus a model file. The model is taken from
    /// `$OPENRT_WHISPER_MODEL` or the common cache locations — absent ⇒
    /// structured `ModelMissing` (never a fabricated transcript).
    pub fn from_env() -> Result<Self, AsrError> {
        let cap = crate::capabilities::audio_capability().ok_or_else(|| AsrError::BackendMissing {
            reason: "no whisper binary on PATH — audio transcript extraction unavailable (MODEL_MISSING)"
                .into(),
        })?;
        let model = discover_model().ok_or_else(|| AsrError::ModelMissing {
            reason: "no whisper model found — set OPENRT_WHISPER_MODEL to a ggml `.bin` model file"
                .into(),
        })?;
        Ok(Self {
            binary: PathBuf::from(cap.binary),
            model,
        })
    }

    /// Transcribe a 16 kHz mono WAV through the engine
    /// (`whisper-cli -m model -f wav -oj -of base`) and parse the JSON it
    /// writes next to `work_dir/asr`. Any non-zero exit is a structured
    /// `Failed` carrying stderr — no partial output is reinterpreted.
    pub fn transcribe(&self, wav: &Path, work_dir: &Path) -> Result<Vec<AsrSegment>, AsrError> {
        let out_base = work_dir.join("asr");
        let out_json = work_dir.join("asr.json");
        let _ = std::fs::remove_file(&out_json);
        let out = Command::new(&self.binary)
            .arg("-m")
            .arg(&self.model)
            .arg("-f")
            .arg(wav)
            .arg("-oj")
            .arg("-of")
            .arg(&out_base)
            .arg("-np")
            .output()
            .map_err(|e| {
                AsrError::Failed(format!("failed to spawn {}: {e}", self.binary.display()))
            })?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(AsrError::Failed(format!(
                "{} exited non-zero: {}",
                self.binary.display(),
                stderr.lines().take(4).collect::<Vec<_>>().join(" ")
            )));
        }
        let json = std::fs::read_to_string(&out_json).map_err(|e| {
            AsrError::Failed(format!(
                "whisper wrote no output at {}: {e}",
                out_json.display()
            ))
        })?;
        parse_whisper_json(&json)
    }
}

/// `$OPENRT_WHISPER_MODEL` first, then the usual local model caches.
fn discover_model() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("OPENRT_WHISPER_MODEL") {
        let path = PathBuf::from(p);
        if path.is_file() {
            return Some(path);
        }
    }
    let home = std::env::var("HOME").ok()?;
    let candidates = [
        format!("{home}/.cache/whisper/ggml-base.en.bin"),
        format!("{home}/.cache/whisper/ggml-small.en.bin"),
        format!("{home}/.cache/openrt/models/ggml-base.en.bin"),
        format!("{home}/.models/ggml-base.en.bin"),
    ];
    candidates.iter().map(PathBuf::from).find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    const WHISPER_JSON: &str = r#"{
      "transcription": [
        {"timestamps": {"from": "00:00:00.000", "to": "00:00:02.500"},
         "offsets": {"from": 0, "to": 2500}, "text": " hello world"},
        {"offsets": {"from": 2500, "to": 4000}, "text": "   "},
        {"offsets": {"from": 4000, "to": 6100}, "text": "second span"}
      ]
    }"#;

    #[test]
    fn whisper_json_offsets_to_seconds() {
        let segs = parse_whisper_json(WHISPER_JSON).unwrap();
        assert_eq!(segs.len(), 2, "whitespace-only segments are dropped");
        assert_eq!(segs[0].t_start, 0.0);
        assert_eq!(segs[0].t_end, 2.5);
        assert_eq!(segs[0].text, "hello world");
        assert_eq!(segs[1].t_start, 4.0);
        assert_eq!(segs[1].t_end, 6.1);
    }

    #[test]
    fn missing_transcription_is_structured_failure() {
        let err = parse_whisper_json(r#"{"text": "no array here"}"#).unwrap_err();
        assert!(matches!(err, AsrError::Failed(_)), "{err:?}");
        assert!(err.to_string().contains("transcription"));
    }

    #[test]
    fn malformed_json_is_structured_failure() {
        assert!(matches!(
            parse_whisper_json("not json"),
            Err(AsrError::Failed(_))
        ));
    }

    /// Provider contract against a *fake* whisper binary (fixture output
    /// only): the runner must honour `-of`, read the JSON the engine wrote,
    /// and parse it. The engine itself is never faked in production paths —
    /// this test only pins the adapter.
    #[cfg(unix)]
    #[test]
    fn fake_backend_script_round_trip() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "openrt-asr-fake-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("fake-whisper.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\n\
             of=\"\"\n\
             prev=\"\"\n\
             for a in \"$@\"; do\n\
             \x20 if [ \"$prev\" = \"-of\" ]; then of=\"$a\"; fi\n\
             \x20 prev=\"$a\"\n\
             done\n\
             cat > \"$of.json\" <<'JSON'\n\
             {\"transcription\":[{\"offsets\":{\"from\":0,\"to\":1500},\"text\":\" fixture words\"}]}\n\
             JSON\n\
             exit 0\n",
        )
        .unwrap();
        let mut perm = std::fs::metadata(&script).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&script, perm).unwrap();

        let wav = dir.join("in.wav");
        std::fs::write(&wav, b"RIFF-fixture").unwrap();
        let provider = WhisperCli {
            binary: script,
            model: dir.join("model.bin"),
        };
        let segs = provider.transcribe(&wav, &dir).unwrap();
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].t_start, 0.0);
        assert_eq!(segs[0].t_end, 1.5);
        assert_eq!(segs[0].text, "fixture words");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn failing_backend_reports_stderr() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("openrt-asr-fail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("fail.sh");
        std::fs::write(&script, "#!/bin/sh\necho 'no model loaded' >&2\nexit 1\n").unwrap();
        let mut perm = std::fs::metadata(&script).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&script, perm).unwrap();

        let provider = WhisperCli {
            binary: script,
            model: dir.join("model.bin"),
        };
        let err = provider.transcribe(&dir.join("x.wav"), &dir).unwrap_err();
        assert!(matches!(err, AsrError::Failed(_)), "{err:?}");
        assert!(err.to_string().contains("no model loaded"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_env_without_engine_is_backend_missing() {
        // This host has no whisper binary (verified); on a host that does,
        // from_env legitimately proceeds to the model check instead.
        if crate::capabilities::audio_capability().is_none() {
            let err = WhisperCli::from_env().unwrap_err();
            assert!(matches!(err, AsrError::BackendMissing { .. }), "{err:?}");
            assert!(err.to_string().contains("MODEL_MISSING"));
        }
    }
}

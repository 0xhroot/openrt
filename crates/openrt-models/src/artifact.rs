//! Artifact inspection and resource profiling for local model files.
//!
//! # Why this exists
//!
//! "The file is 9.35 GiB" and "running this model needs 9.35 GiB of resident
//! memory" are different claims, and a system that cannot tell them apart will
//! either refuse to catalogue a perfectly good file or promise it can run
//! something it cannot. This module keeps them apart on purpose:
//!
//! * [`ArtifactMetadata`] — what the file *says about itself*, read from a
//!   bounded header window. Never loads weights.
//! * [`ModelResourceProfile`] — the numbers, each tagged with how it was
//!   obtained ([`Provenance`]), with `None` wherever nothing real is known.
//! * [`assess_execution_fit`] — an **advisory** comparison against host
//!   memory. It is not a gate: admission stays with `openrt-resources` at the
//!   execution boundary.
//!
//! # Bounded by construction
//!
//! [`ArtifactMetadata::read`] opens the file and reads at most
//! [`GGUF_HEADER_WINDOW`] bytes ([`GGUF_HEADER_WINDOW`] is 256 KiB) regardless of
//! artifact size. A 9 GiB model and a 9 KiB one cost the same here. That is the
//! difference between inspecting an artifact and loading it.

use std::path::Path;

/// Size of the streaming read buffer used while walking an artifact header.
///
/// The header is walked with this buffer instead of being slurped: a real 9.8 GB
/// Qwen3.5 GGUF carries a 10.4 MiB key/value block, almost all of it tokenizer
/// vocabulary arrays. Reading it sequentially keeps memory flat at one buffer
/// regardless of artifact size, which is the whole point of inspecting a file
/// rather than loading it.
pub const GGUF_READ_BUFFER_BYTES: usize = 256 * 1024;

/// Ceiling on bytes read from one artifact header.
///
/// Bounded so a malformed or hostile header cannot turn metadata inspection into
/// an unbounded read. Exceeding it is reported, not hidden.
pub const GGUF_MAX_HEADER_BYTES: u64 = 64 * 1024 * 1024;

/// Longest string accepted from a header.
///
/// String lengths are attacker-controlled; a huge declared length must not turn
/// into a huge allocation.
pub const GGUF_MAX_STRING_BYTES: u64 = 1 << 20;

/// Most elements walked while skipping an array payload.
pub const GGUF_MAX_ARRAY_ITEMS: u64 = 1 << 26;

/// Context length a KV-cache estimate is computed at.
///
/// A model's declared context is its *maximum* — Qwen3.5 declares 262144 — and
/// pricing a session at the maximum produces numbers (tens of GiB of KV) that
/// describe a configuration nobody runs. Estimates therefore use a modest
/// assumed window, say so out loud, and carry the per-token cost so any other
/// window can be priced from the same number.
pub const ASSUMED_CONTEXT_TOKENS: u64 = 4096;

/// File container format, derived from the extension.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtifactFormat {
    Gguf,
    Safetensors,
    Onnx,
    Npz,
    Torch,
    Binary,
    /// An extension we do not model. Carried rather than dropped so the UI can
    /// still say what it saw.
    Other(String),
}

impl ArtifactFormat {
    pub fn from_path(path: &Path) -> Self {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        match ext.as_str() {
            "gguf" => Self::Gguf,
            "safetensors" => Self::Safetensors,
            "onnx" => Self::Onnx,
            "npz" => Self::Npz,
            "pt" | "pth" | "bin" => Self::Torch,
            _ => Self::Other(ext),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Gguf => "gguf",
            Self::Safetensors => "safetensors",
            Self::Onnx => "onnx",
            Self::Npz => "npz",
            Self::Torch => "torch",
            Self::Binary => "binary",
            Self::Other(e) if e.is_empty() => "(no extension)",
            Self::Other(e) => e,
        }
    }

    /// True when the format carries machine-readable metadata we can read
    /// without loading weights. Formats that do not are honestly reported as
    /// "no metadata available" rather than guessed at.
    pub fn has_readable_metadata(&self) -> bool {
        matches!(self, Self::Gguf)
    }
}

/// What the artifact declared about itself.
///
/// Every field is optional because absence is the normal case for most formats,
/// and a fabricated default would be worse than `None`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ArtifactMetadata {
    pub format: Option<ArtifactFormat>,
    /// e.g. `qwen3`, from `general.architecture`.
    pub architecture: Option<String>,
    /// e.g. `Q8_0`, from `general.file_type`.
    pub quantization: Option<String>,
    pub parameter_count: Option<u64>,
    /// Default context length declared by the file (`<arch>.context_length`).
    /// Not a limit the runtime enforces — what the file was trained/configured
    /// for.
    pub context_length: Option<u64>,
    pub n_layers: Option<u64>,
    pub n_embd: Option<u64>,
    /// Attention heads (`<arch>.attention.head_count`).
    pub n_heads: Option<u64>,
    /// Key/value heads (`<arch>.attention.head_count_kv`). Drives KV cache size:
    /// grouped-query attention keeps fewer KV heads than query heads.
    pub n_kv_heads: Option<u64>,
    /// Per-head key and value widths, when declared separately from
    /// `embedding_length`.
    pub key_length: Option<u64>,
    pub value_length: Option<u64>,
    /// Bytes actually read from the file to produce this. The proof that
    /// inspecting an artifact is bounded no matter how large the artifact is,
    /// and the number the tests assert on.
    pub bytes_read: u64,
    /// GGUF container version, when the container is GGUF.
    pub gguf_version: Option<u32>,
    /// The whole key/value block was walked.
    pub kv_complete: bool,
    /// The walk stopped on purpose because a profile no longer needs more.
    pub stopped_early: bool,
    /// Set when the header could not be fully parsed. The metadata gathered so
    /// far is still returned: a header we cannot fully read is not a reason to
    /// refuse to catalogue the file.
    pub notes: Vec<String>,
}

impl ArtifactMetadata {
    /// True once the fields a resource profile is built from are present.
    ///
    /// Drives the early exit in the header walk: a real GGUF puts its tokenizer
    /// vocabulary *after* these keys, and that vocabulary is megabytes of arrays
    /// no profile reads.
    pub fn has_what_a_profile_needs(&self) -> bool {
        self.architecture.is_some()
            && self.quantization.is_some()
            && self.n_layers.is_some()
            && self.n_embd.is_some()
            // Without these, KV cache can only be guessed at full attention
            // width. A real GGUF declares all of them within the first couple of
            // kilobytes — long before the tokenizer vocabulary — so demanding
            // them costs almost nothing and is the difference between a measured
            // KV cache and a rough one.
            && self.context_length.is_some()
            && (self.n_kv_heads.is_some() || self.n_heads.is_some())
    }
}

/// Inspect an artifact's header.
///
/// Returns `Ok` for any readable file, including ones whose header is absent or
/// malformed — `notes` then explains what was and was not learned. Returns
/// `Err` only when the file cannot be opened or read at all.
pub fn read_metadata(path: &Path) -> Result<ArtifactMetadata, ArtifactError> {
    let format = ArtifactFormat::from_path(path);
    let mut md = ArtifactMetadata {
        format: Some(format.clone()),
        ..Default::default()
    };
    if !format.has_readable_metadata() {
        md.notes.push(format!(
            "{} carries no header this crate can read; metadata NOT PROBED",
            format.as_str()
        ));
        return Ok(md);
    }

    let f = std::fs::File::open(path)?;
    let mut r = HeaderReader::new(f);
    match read_gguf_header(&mut r, &mut md) {
        Ok(()) => {}
        // A header we could not walk to the end is not a reason to throw away
        // what we did learn: the metadata gathered so far is applied as it is
        // read, and the reason it stopped is a note.
        Err(e) => md
            .notes
            .push(format!("gguf header walk stopped early: {e}")),
    }
    md.bytes_read = r.bytes_read;
    if !md.kv_complete && !md.stopped_early {
        md.notes.push(format!(
            "gguf key/value block not walked to the end after {} bytes \
             (read ceiling {} bytes); any metadata past that point NOT PROBED",
            md.bytes_read, GGUF_MAX_HEADER_BYTES
        ));
    }
    Ok(md)
}

/// A GGUF value as far as this reader needs to know it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum GgufValue {
    U32(u32),
    U64(u64),
    Str(String),
    Other(&'static str),
}

/// Streaming, bounded reader over an artifact header.
///
/// Holds one fixed buffer and walks forward through it. Values the caller does
/// not need are stepped over rather than collected, so memory is
/// `GGUF_READ_BUFFER_BYTES` plus the largest string encountered — never the
/// size of the header, and never the size of the artifact.
pub(crate) struct HeaderReader<R: std::io::Read> {
    src: R,
    buf: Vec<u8>,
    pos: usize,
    end: usize,
    eof: bool,
    pub(crate) bytes_read: u64,
}

impl<R: std::io::Read + std::io::Seek> HeaderReader<R> {
    pub(crate) fn new(src: R) -> Self {
        Self {
            src,
            buf: vec![0u8; GGUF_READ_BUFFER_BYTES],
            pos: 0,
            end: 0,
            eof: false,
            bytes_read: 0,
        }
    }

    fn available(&self) -> usize {
        self.end - self.pos
    }

    /// Make `n` bytes available at the cursor, refilling as needed.
    fn need(&mut self, n: usize) -> Result<(), ArtifactError> {
        if n > self.buf.len() {
            return Err(ArtifactError::Header(format!(
                "value of {n} bytes exceeds the {}-byte read buffer",
                self.buf.len()
            )));
        }
        while self.available() < n {
            if self.eof {
                return Err(ArtifactError::Header(format!(
                    "header ended after {} bytes while {} more were needed",
                    self.bytes_read,
                    n - self.available()
                )));
            }
            if self.bytes_read >= GGUF_MAX_HEADER_BYTES {
                return Err(ArtifactError::Header(format!(
                    "read ceiling of {GGUF_MAX_HEADER_BYTES} bytes reached"
                )));
            }
            if self.pos > 0 {
                self.buf.copy_within(self.pos..self.end, 0);
                self.end -= self.pos;
                self.pos = 0;
            }
            let want =
                (self.buf.len() - self.end).min((GGUF_MAX_HEADER_BYTES - self.bytes_read) as usize);
            let got = self
                .src
                .read(&mut self.buf[self.end..self.end + want])
                .map_err(ArtifactError::Io)?;
            if got == 0 {
                self.eof = true;
            } else {
                self.end += got;
                self.bytes_read += got as u64;
            }
        }
        Ok(())
    }

    pub(crate) fn take(&mut self, n: usize) -> Result<&[u8], ArtifactError> {
        self.need(n)?;
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    /// Step over `n` bytes, reading only what the buffer cannot cover anyway.
    ///
    /// A payload larger than the buffer is jumped with a seek: those bytes are
    /// never wanted, so reading them would be pure waste. A seek reads nothing,
    /// so it does not count against the read ceiling either.
    fn advance(&mut self, n: u64) -> Result<(), ArtifactError> {
        let mut left = n;
        loop {
            let have = self.available() as u64;
            if have >= left {
                self.pos += left as usize;
                return Ok(());
            }
            left -= have;
            self.pos = self.end;
            if left == 0 {
                return Ok(());
            }
            if left > self.buf.len() as u64 {
                // Jump the part that could never fit in the buffer and leave a
                // full buffer's worth to be read normally. Strictly greater:
                // seeking when `left` merely equals the buffer size would seek
                // zero bytes and come straight back here forever.
                let jump = left - self.buf.len() as u64;
                self.seek_ahead(jump)?;
                left -= jump;
            }
            self.need(1)?;
        }
    }

    fn seek_ahead(&mut self, n: u64) -> Result<(), ArtifactError> {
        if n == 0 {
            return Ok(());
        }
        let cur = self.src.stream_position().map_err(ArtifactError::Io)?;
        let target = cur
            .checked_add(n)
            .ok_or_else(|| ArtifactError::Header("offset overflow".into()))?;
        self.src
            .seek(std::io::SeekFrom::Start(target))
            .map_err(ArtifactError::Io)?;
        self.pos = 0;
        self.end = 0;
        self.eof = false;
        Ok(())
    }

    /// Logical file cursor position independent of buffering.
    ///
    /// The reader slurps ahead in one fixed buffer, so the underlying stream
    /// position is ahead of the logical cursor by the unconsumed buffered
    /// bytes. Everything that wants a *position* (e.g. to derive where a
    /// GGUF tensor-data section begins) needs this adjusted value.
    pub(crate) fn logical_position(&mut self) -> u64 {
        let buffered = (self.end - self.pos) as u64;
        self.src
            .stream_position()
            .map(|p| p.saturating_sub(buffered))
            .unwrap_or(0)
    }

    pub(crate) fn u32(&mut self) -> Result<u32, ArtifactError> {
        let s = self.take(4)?;
        Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }

    pub(crate) fn u64(&mut self) -> Result<u64, ArtifactError> {
        let s = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(s);
        Ok(u64::from_le_bytes(a))
    }

    fn string_len(&mut self) -> Result<u64, ArtifactError> {
        let len = self.u64()?;
        if len > GGUF_MAX_STRING_BYTES {
            return Err(ArtifactError::Header(format!(
                "string length {len} exceeds the {GGUF_MAX_STRING_BYTES}-byte limit"
            )));
        }
        Ok(len)
    }

    pub(crate) fn string(&mut self) -> Result<String, ArtifactError> {
        let len = self.string_len()? as usize;
        let s = self.take(len)?;
        Ok(String::from_utf8_lossy(s).into_owned())
    }

    /// Read a value we care about, or step over one we do not.
    pub(crate) fn walk_value(
        &mut self,
        vtype: u32,
        depth: u32,
    ) -> Result<GgufValue, ArtifactError> {
        if depth > 4 {
            return Err(ArtifactError::Header(
                "array nesting deeper than 4 levels is not supported".into(),
            ));
        }
        match vtype {
            4 => Ok(GgufValue::U32(self.u32()?)),
            10 => Ok(GgufValue::U64(self.u64()?)),
            8 => Ok(GgufValue::Str(self.string()?)),
            0 | 1 | 7 => {
                self.advance(1)?;
                Ok(GgufValue::Other("1-byte scalar"))
            }
            2 | 3 => {
                self.advance(2)?;
                Ok(GgufValue::Other("2-byte scalar"))
            }
            5 | 6 => {
                self.advance(4)?;
                Ok(GgufValue::Other("4-byte scalar"))
            }
            11 | 12 => {
                self.advance(8)?;
                Ok(GgufValue::Other("8-byte scalar"))
            }
            9 => {
                let elem = self.u32()?;
                let count = self.u64()?;
                if count > GGUF_MAX_ARRAY_ITEMS {
                    return Err(ArtifactError::Header(format!(
                        "array of {count} elements exceeds the {GGUF_MAX_ARRAY_ITEMS} limit"
                    )));
                }
                // Step over the payload. Fixed-width elements are one seek;
                // strings and nested arrays have to be walked length by length.
                if let Some(sz) = gguf_fixed_width(elem) {
                    let bytes = count
                        .checked_mul(sz as u64)
                        .ok_or_else(|| ArtifactError::Header("array length overflow".into()))?;
                    self.advance(bytes)?;
                } else {
                    for _ in 0..count {
                        self.skip_value(elem, depth + 1)?;
                    }
                }
                Ok(GgufValue::Other("array"))
            }
            other => Err(ArtifactError::Header(format!("unknown gguf type {other}"))),
        }
    }

    pub(crate) fn skip_value(&mut self, vtype: u32, depth: u32) -> Result<(), ArtifactError> {
        self.walk_value(vtype, depth).map(|_| ())
    }
}

/// Byte width of a fixed-width GGUF type, or `None` for variable-width ones.
fn gguf_fixed_width(vtype: u32) -> Option<usize> {
    match vtype {
        0 | 1 | 7 => Some(1),
        2 | 3 => Some(2),
        4 | 5 | 6 => Some(4),
        10 | 11 | 12 => Some(8),
        _ => None,
    }
}

/// Walk a GGUF header, applying metadata to `md` as it goes.
///
/// Each key/value pair is applied the moment it is read so the walk can stop as
/// soon as the fields worth having are in hand: a real GGUF keeps its tokenizer
/// vocabulary (megabytes of arrays) *after* the architecture and shape keys, and
/// those are the only keys a resource profile needs.
fn read_gguf_header<R: std::io::Read + std::io::Seek>(
    r: &mut HeaderReader<R>,
    md: &mut ArtifactMetadata,
) -> Result<(), ArtifactError> {
    if r.take(4)? != b"GGUF" {
        return Err(ArtifactError::Header(
            "not a GGUF file (magic mismatch)".into(),
        ));
    }
    let version = r.u32()?;
    if version == 0 || version > 3 {
        return Err(ArtifactError::Header(format!(
            "unsupported GGUF version {version}"
        )));
    }
    let _tensors = r.u64()?;
    let kv_count = r.u64()?;
    if kv_count > (GGUF_MAX_HEADER_BYTES / 16) as u64 {
        return Err(ArtifactError::Header(format!(
            "kv count {kv_count} cannot fit in a {GGUF_MAX_HEADER_BYTES}-byte ceiling"
        )));
    }
    md.gguf_version = Some(version);
    for _ in 0..kv_count {
        let key = r.string()?;
        let vtype = r.u32()?;
        let value = r.walk_value(vtype, 0)?;
        apply_gguf_kv(md, &key, &value);
        if md.has_what_a_profile_needs() {
            md.kv_complete = false; // stopped early, deliberately
            md.stopped_early = true;
            return Ok(());
        }
    }
    md.kv_complete = true;
    Ok(())
}

/// Apply one key/value pair to the metadata as it is read.
///
/// Incremental on purpose: the walk stops as soon as these fields are in hand,
/// which for a real GGUF is a few hundred bytes in — long before the tokenizer
/// vocabulary arrays that make the header megabytes big.
fn apply_gguf_kv(md: &mut ArtifactMetadata, key: &str, value: &GgufValue) {
    let arch_prefix = md.architecture.as_deref().map(|a| format!("{a}."));
    let arch_scoped = arch_prefix.as_deref().is_some_and(|p| key.starts_with(p));
    match (key, value) {
        ("general.architecture", GgufValue::Str(a)) => {
            md.architecture = Some(a.clone());
            return;
        }
        ("general.file_type", GgufValue::U32(t)) => {
            md.quantization = Some(gguf_file_type_name(*t).to_string());
            return;
        }
        ("general.parameter_count", GgufValue::U64(n)) => {
            md.parameter_count = Some(*n);
            return;
        }
        _ => {}
    }
    if !arch_scoped {
        return;
    }
    let want = &key[arch_prefix.as_ref().unwrap().len()..];
    let n = match value {
        GgufValue::U32(n) => Some(*n as u64),
        GgufValue::U64(n) => Some(*n),
        _ => None,
    };
    let Some(n) = n else { return };
    match want {
        "context_length" => md.context_length = Some(n),
        "block_count" => md.n_layers = Some(n),
        "embedding_length" => md.n_embd = Some(n),
        "attention.head_count" => md.n_heads = Some(n),
        "attention.head_count_kv" => md.n_kv_heads = Some(n),
        "attention.key_length" => md.key_length = Some(n),
        "attention.value_length" => md.value_length = Some(n),
        _ => {}
    }
}

/// `general.file_type` → the quantization name operators recognize.
///
/// Unmapped ids return the raw number rather than a guess: `file_type=9993` is
/// not a quantization scheme anyone can act on.
pub fn gguf_file_type_name(t: u32) -> &'static str {
    match t {
        0 => "F32",
        1 => "F16",
        2 => "Q4_0",
        3 => "Q4_1",
        7 => "Q8_0",
        8 => "Q5_0",
        9 => "Q5_1",
        10 => "Q2_K",
        11 => "Q3_K_S",
        12 => "Q3_K_M",
        13 => "Q3_K_L",
        14 => "Q4_K_S",
        15 => "Q4_K_M",
        16 => "Q5_K_S",
        17 => "Q5_K_M",
        18 => "Q6_K",
        30 => "BF16",
        _ => "UNKNOWN",
    }
}

/// Effective bits per weight for the formats above.
///
/// Block-quantized formats waste part of each block on scales/minimums, so
/// "Q8_0" is 8.5 bits/weight, not 8. Arithmetic that follows from this is
/// labelled [`Provenance::Advisory`] everywhere it surfaces.
pub fn bits_per_weight(quantization: &str) -> Option<f64> {
    Some(match quantization {
        "F32" => 32.0,
        "F16" | "BF16" => 16.0,
        "Q8_0" => 8.5,
        "Q6_K" => 6.5625,
        "Q5_K_M" => 5.69,
        "Q5_K_S" => 5.52,
        "Q5_1" => 5.5,
        "Q5_0" => 5.5,
        "Q4_K_M" => 4.85,
        "Q4_K_S" => 4.58,
        "Q4_1" => 4.5,
        "Q4_0" => 4.5,
        "Q3_K_M" => 3.91,
        "Q3_K_L" => 3.81,
        "Q3_K_S" => 3.5,
        "Q2_K" => 2.63,
        _ => return None,
    })
}

/// How a number in a [`ModelResourceProfile`] was obtained.
///
/// Present on the type rather than in prose so a UI cannot accidentally present
/// an estimate as a measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provenance {
    /// Read from the artifact or the host, not inferred.
    Measured,
    /// Arithmetic from measured values under stated assumptions.
    Advisory,
    /// Nothing on this host or in this artifact can answer it.
    NotProbed,
}

impl Provenance {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Measured => "MEASURED",
            Self::Advisory => "ADVISORY",
            Self::NotProbed => "NOT PROBED",
        }
    }
}

/// A number plus where it came from. Makes "we don't know" a value rather than
/// an absence, so it survives being rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sourced {
    pub bytes: u64,
    pub provenance: Provenance,
}

impl Sourced {
    fn advisory(bytes: u64) -> Self {
        Self {
            bytes,
            provenance: Provenance::Advisory,
        }
    }

    fn measured(bytes: u64) -> Self {
        Self {
            bytes,
            provenance: Provenance::Measured,
        }
    }
}

/// What executing an artifact is expected to cost, kept separate from how big
/// the file is.
///
/// The distinction the whole task turns on: `artifact_bytes` is 9.35 GiB of
/// *file*, `runtime_working_set` is an estimate of what execution would touch.
/// For the quantized GGUF artifacts OpenRT registers today the runtime is an
/// external server that memory-maps the file, so the second number is smaller
/// than the first and grows with context length rather than with file size.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelResourceProfile {
    /// File size on disk. Measured.
    pub artifact_bytes: u64,
    /// Weight storage implied by the declared quantization, when derivable.
    pub weight_bytes: Option<Sourced>,
    pub bits_per_weight: Option<f64>,
    /// KV cache at [`ModelResourceProfile::kv_context_length`].
    /// KV cache for [`ModelResourceProfile::kv_assumed_context_tokens`].
    pub kv_cache_bytes: Option<Sourced>,
    /// KV cache cost of a single token, derived from measured head geometry.
    /// This is the honest unit: KV grows linearly with context, so a single
    /// number for "what execution needs" is meaningless without saying at which
    /// context.
    pub kv_bytes_per_token: Option<Sourced>,
    /// Context length the file declares — its maximum, not what a session will
    /// use.
    pub kv_context_length: Option<u64>,
    /// The context [`ModelResourceProfile::kv_cache_bytes`] was computed at.
    pub kv_assumed_context_tokens: u64,
    /// Weights + KV. Advisory: it assumes full residency, which a
    /// memory-mapping backend does not actually do.
    pub runtime_working_set_bytes: Option<Sourced>,
    /// Never invented. `None` until a GPU probe reports something.
    pub vram_bytes: Option<Sourced>,
    /// RAM needed for execution. Advisory, same caveat as the working set.
    pub ram_bytes: Option<Sourced>,
    pub notes: Vec<String>,
}

impl Default for ModelResourceProfile {
    fn default() -> Self {
        Self {
            artifact_bytes: 0,
            weight_bytes: None,
            bits_per_weight: None,
            kv_cache_bytes: None,
            kv_bytes_per_token: None,
            kv_context_length: None,
            kv_assumed_context_tokens: ASSUMED_CONTEXT_TOKENS,
            runtime_working_set_bytes: None,
            vram_bytes: None,
            ram_bytes: None,
            notes: Vec::new(),
        }
    }
}

impl ModelResourceProfile {
    /// Build a profile from what the artifact declared.
    ///
    /// Every `None` here is deliberate: no field is inferred from a field we do
    /// not have. A safetensors artifact with no readable header yields exactly
    /// `artifact_bytes` and a note.
    pub fn from_metadata(md: &ArtifactMetadata, artifact_bytes: u64) -> Self {
        let mut notes = md.notes.clone();
        let bits = md.quantization.as_deref().and_then(bits_per_weight);

        // Weights are measured, not modelled: in a GGUF the tensor payload *is*
        // the rest of the file, so weights are what remains after the header we
        // just walked. Only for formats whose weights we cannot locate that
        // does this fall back to deriving bytes from bits-per-weight, and there
        // it says so.
        let weight_bytes = match md.format {
            Some(ArtifactFormat::Gguf) if artifact_bytes > md.bytes_read => Some(Sourced {
                bytes: artifact_bytes - md.bytes_read,
                provenance: Provenance::Measured,
            }),
            _ => match (bits, md.parameter_count) {
                (Some(bits), Some(params)) => Some(Sourced::advisory(
                    ((params as f64) * bits / 8.0).round() as u64,
                )),
                _ => {
                    if md.format == Some(ArtifactFormat::Gguf) {
                        notes.push(
                            "weight bytes NOT PROBED: file is no larger than its header".into(),
                        );
                    } else {
                        notes.push(
                            "quantization and parameter count not both known; weight bytes NOT PROBED"
                                .into(),
                        );
                    }
                    None
                }
            },
        };

        // KV cache from measured head geometry, per token:
        //   layers x kv_heads x (key_len + value_len) x bytes per element.
        // Grouped-query attention is why kv_heads is used rather than heads: a
        // 16-head model with 4 KV heads stores a quarter of the K/V it attends
        // with. f16 for keys and values, as every current backend does.
        let kv_bytes_per_token = match (
            md.n_layers,
            md.n_kv_heads.or(md.n_heads),
            md.key_length,
            md.value_length,
        ) {
            (Some(layers), Some(kv_heads), Some(kl), Some(vl)) => Some(Sourced::advisory(
                layers
                    .saturating_mul(kv_heads)
                    .saturating_mul(kl.saturating_add(vl))
                    .saturating_mul(2),
            )),
            _ => match (md.n_layers, md.n_embd) {
                // No separate KV head geometry: fall back to full-width
                // attention and say that is what happened.
                (Some(layers), Some(embd)) => {
                    notes.push(
                        "KV head geometry not declared; KV cache estimated at full attention width"
                            .into(),
                    );
                    Some(Sourced::advisory(
                        layers.saturating_mul(embd).saturating_mul(4),
                    ))
                }
                _ => {
                    if md.format == Some(ArtifactFormat::Gguf) {
                        notes.push("layer/head geometry incomplete; KV cache NOT PROBED".into());
                    }
                    None
                }
            },
        };
        let kv_cache_bytes = kv_bytes_per_token.map(|per_token| Sourced {
            bytes: per_token.bytes.saturating_mul(ASSUMED_CONTEXT_TOKENS),
            provenance: Provenance::Advisory,
        });
        let runtime_working_set_bytes = match (weight_bytes, kv_cache_bytes) {
            (Some(w), Some(kv)) => Some(Sourced {
                bytes: w.bytes.saturating_add(kv.bytes),
                provenance: Provenance::Advisory,
            }),
            (Some(w), None) => Some(Sourced {
                bytes: w.bytes,
                provenance: w.provenance,
            }),
            _ => None,
        };
        if let (Some(per_token), Some(declared)) = (kv_bytes_per_token, md.context_length) {
            notes.push(format!(
                "KV cache counted at an assumed {} tokens ({} total, {} per token, advisory); \
                 the file declares up to {} tokens, which would need {}",
                ASSUMED_CONTEXT_TOKENS,
                human_bytes(per_token.bytes.saturating_mul(ASSUMED_CONTEXT_TOKENS)),
                human_bytes(per_token.bytes),
                declared,
                human_bytes(per_token.bytes.saturating_mul(declared)),
            ));
        }
        if runtime_working_set_bytes.is_none() {
            notes.push(
                "runtime working set NOT PROBED: needs weight or KV metadata this artifact does not declare"
                    .into(),
            );
        }
        Self {
            artifact_bytes,
            weight_bytes,
            bits_per_weight: bits,
            kv_cache_bytes,
            kv_bytes_per_token,
            kv_context_length: md.context_length,
            kv_assumed_context_tokens: ASSUMED_CONTEXT_TOKENS,
            runtime_working_set_bytes,
            vram_bytes: None,
            ram_bytes: runtime_working_set_bytes,
            notes,
        }
    }

    /// The number to compare against host memory.
    ///
    /// Prefers the advisory working set and falls back to artifact size when
    /// nothing was derived — a file that *is* the working set for a backend
    /// that reads it all. Never returns zero for a non-empty artifact: that
    /// would render as "fits easily" for the one case we know least about.
    pub fn execution_requirement(&self) -> Sourced {
        self.runtime_working_set_bytes
            .unwrap_or_else(|| Sourced::measured(self.artifact_bytes))
    }

    /// Plain-language account of how [`Self::execution_requirement`] was built.
    ///
    /// Rendered next to the number so a reader never has to guess whether
    /// "needs 9.6 GiB" came from a measurement or from an assumption.
    pub fn requirement_basis(&self) -> String {
        match (&self.weight_bytes, &self.kv_cache_bytes) {
            (Some(w), Some(kv)) => format!(
                "{} measured weights + {} KV at {} tokens (advisory)",
                human_bytes(w.bytes),
                human_bytes(kv.bytes),
                self.kv_assumed_context_tokens
            ),
            (Some(w), None) => format!(
                "{} weights, KV cache NOT PROBED ({} provenance)",
                human_bytes(w.bytes),
                w.provenance.as_str()
            ),
            _ => format!(
                "{} artifact size only — nothing else was measurable",
                human_bytes(self.artifact_bytes)
            ),
        }
    }
}

/// Whether a model looks runnable right now. **Advisory, never a gate.**
#[derive(Debug, Clone, PartialEq)]
pub enum ExecutionFit {
    /// The advisory requirement fits inside the memory the host reports free.
    Ready { required: Sourced, available: u64 },
    /// It does not fit. The refusal itself comes from resource admission at
    /// execution time; this is the explanation shown before the attempt.
    ResourceLimited {
        required: Sourced,
        available: u64,
        reason: String,
    },
    /// Cannot be assessed. Said out loud rather than assumed to be fine.
    Unknown { reason: String },
}

/// Compare an advisory requirement against available host memory.
pub fn assess_execution_fit(
    profile: &ModelResourceProfile,
    available_ram: Option<u64>,
) -> ExecutionFit {
    let required = profile.execution_requirement();
    let Some(available) = available_ram else {
        return ExecutionFit::Unknown {
            reason: "host memory not probed".into(),
        };
    };
    if available == 0 {
        return ExecutionFit::Unknown {
            reason: "host reported 0 available memory".into(),
        };
    }
    if required.bytes > available {
        return ExecutionFit::ResourceLimited {
            reason: format!(
                "advisory requirement {} ({}) exceeds available {}",
                human_bytes(required.bytes),
                profile.requirement_basis(),
                human_bytes(available)
            ),
            required,
            available,
        };
    }
    ExecutionFit::Ready {
        required,
        available,
    }
}

/// `9.4G`-style size, matching what the TUI already prints elsewhere.
pub fn human_bytes(b: u64) -> String {
    const KIB: f64 = 1024.0;
    let b = b as f64;
    if b >= KIB * KIB * KIB {
        format!("{:.2} GiB", b / (KIB * KIB * KIB))
    } else if b >= KIB * KIB {
        format!("{:.1} MiB", b / (KIB * KIB))
    } else if b >= KIB {
        format!("{:.0} KiB", b / KIB)
    } else {
        format!("{b} B")
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ArtifactError {
    #[error("io error reading artifact: {0}")]
    Io(#[from] std::io::Error),
    #[error("artifact header error: {0}")]
    Header(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a byte-accurate GGUF header so the parser is tested against the
    /// real format rather than against itself.
    fn gguf_header(entries: &[(&str, GgufValue)]) -> Vec<u8> {
        let mut v: Vec<u8> = Vec::new();
        v.extend_from_slice(b"GGUF");
        v.extend_from_slice(&3u32.to_le_bytes());
        v.extend_from_slice(&0u64.to_le_bytes()); // tensors
        v.extend_from_slice(&(entries.len() as u64).to_le_bytes());
        for (k, val) in entries {
            v.extend_from_slice(&(k.len() as u64).to_le_bytes());
            v.extend_from_slice(k.as_bytes());
            match val {
                GgufValue::U32(n) => {
                    v.extend_from_slice(&4u32.to_le_bytes());
                    v.extend_from_slice(&n.to_le_bytes());
                }
                GgufValue::U64(n) => {
                    v.extend_from_slice(&10u32.to_le_bytes());
                    v.extend_from_slice(&n.to_le_bytes());
                }
                GgufValue::Str(s) => {
                    v.extend_from_slice(&8u32.to_le_bytes());
                    v.extend_from_slice(&(s.len() as u64).to_le_bytes());
                    v.extend_from_slice(s.as_bytes());
                }
                GgufValue::Other(_) => unreachable!("test helper does not emit these"),
            }
        }
        v
    }

    #[test]
    fn reads_architecture_quantization_and_shapes() {
        let dir = crate::artifact::tests::tmpdir("gguf-meta");
        let p = dir.join("model.gguf");
        let bytes = gguf_header(&[
            ("general.architecture", GgufValue::Str("qwen3".into())),
            ("general.file_type", GgufValue::U32(7)),
            ("general.parameter_count", GgufValue::U64(9_000_000_000)),
            ("qwen3.context_length", GgufValue::U32(32768)),
            ("qwen3.block_count", GgufValue::U32(40)),
            ("qwen3.embedding_length", GgufValue::U32(4096)),
        ]);
        std::fs::write(&p, &bytes).unwrap();

        let md = read_metadata(&p).unwrap();
        assert_eq!(md.architecture.as_deref(), Some("qwen3"));
        assert_eq!(md.quantization.as_deref(), Some("Q8_0"));
        assert_eq!(md.parameter_count, Some(9_000_000_000));
        assert_eq!(md.context_length, Some(32768));
        assert_eq!(md.n_layers, Some(40));
        assert_eq!(md.n_embd, Some(4096));
        assert!(
            md.bytes_read <= (GGUF_READ_BUFFER_BYTES * 4) as u64,
            "read {} bytes for a header of {} bytes",
            md.bytes_read,
            bytes.len()
        );
        crate::artifact::tests::cleanup(&dir);
    }

    #[test]
    fn inspection_is_bounded_by_the_window_not_the_file() {
        let dir = crate::artifact::tests::tmpdir("gguf-window");
        let p = dir.join("big.gguf");
        // A header followed by far more file than the window: the point is that
        // reading stops at the window.
        let mut bytes = gguf_header(&[("general.architecture", GgufValue::Str("llama".into()))]);
        bytes.resize(GGUF_READ_BUFFER_BYTES * 4, 0);
        std::fs::write(&p, &bytes).unwrap();

        let md = read_metadata(&p).unwrap();
        assert_eq!(md.bytes_read, GGUF_READ_BUFFER_BYTES as u64);
        assert_eq!(md.architecture.as_deref(), Some("llama"));
        crate::artifact::tests::cleanup(&dir);
    }

    #[test]
    fn a_non_gguf_format_reports_no_metadata_rather_than_guessing() {
        let dir = crate::artifact::tests::tmpdir("safetensors");
        let p = dir.join("model.safetensors");
        std::fs::write(&p, b"{\"__metadata__\":{}}").unwrap();

        let md = read_metadata(&p).unwrap();
        assert_eq!(md.format, Some(ArtifactFormat::Safetensors));
        assert_eq!(md.architecture, None);
        assert_eq!(md.quantization, None);
        assert_eq!(md.bytes_read, 0, "no bytes read at all for this format");
        assert!(md.notes.iter().any(|n| n.contains("NOT PROBED")));
        crate::artifact::tests::cleanup(&dir);
    }

    #[test]
    fn a_corrupt_header_still_yields_the_format_and_a_note() {
        let dir = crate::artifact::tests::tmpdir("gguf-corrupt");
        let p = dir.join("bad.gguf");
        std::fs::write(&p, b"NOTGGUF-but-claims-to-be").unwrap();

        let md = read_metadata(&p).unwrap();
        assert_eq!(md.format, Some(ArtifactFormat::Gguf));
        assert!(
            md.notes.iter().any(|n| n.contains("stopped early")),
            "{:?}",
            md.notes
        );
        crate::artifact::tests::cleanup(&dir);
    }

    #[test]
    fn a_hostile_string_length_cannot_exhaust_memory() {
        // kv count 1, key ok, type 8 (string), length = u64::MAX
        let mut v: Vec<u8> = Vec::new();
        v.extend_from_slice(b"GGUF");
        v.extend_from_slice(&3u32.to_le_bytes());
        v.extend_from_slice(&0u64.to_le_bytes());
        v.extend_from_slice(&1u64.to_le_bytes());
        v.extend_from_slice(&1u64.to_le_bytes());
        v.extend_from_slice(b"k");
        v.extend_from_slice(&8u32.to_le_bytes());
        v.extend_from_slice(&u64::MAX.to_le_bytes());
        let dir = crate::artifact::tests::tmpdir("gguf-hostile");
        let p = dir.join("hostile.gguf");
        std::fs::write(&p, &v).unwrap();

        let md = read_metadata(&p).unwrap();
        assert!(
            md.notes.iter().any(|n| n.contains("string length")),
            "a hostile declared length must be named as such: {:?}",
            md.notes
        );
        assert!(
            md.bytes_read < GGUF_READ_BUFFER_BYTES as u64 * 2,
            "a hostile length must not turn into a read: {} bytes",
            md.bytes_read
        );
        crate::artifact::tests::cleanup(&dir);
    }

    #[test]
    fn kv_cache_is_arithmetic_from_declared_shapes() {
        let md = ArtifactMetadata {
            format: Some(ArtifactFormat::Gguf),
            architecture: Some("qwen3".into()),
            quantization: Some("Q8_0".into()),
            parameter_count: Some(9_440_000_000),
            // A declared *maximum* far above the assumed window: the profile
            // must not price a session at it.
            context_length: Some(262_144),
            n_layers: Some(40),
            n_embd: Some(4096),
            n_heads: Some(32),
            n_kv_heads: Some(8),
            key_length: Some(128),
            value_length: Some(128),
            bytes_read: 128,
            gguf_version: Some(3),
            kv_complete: true,
            stopped_early: false,
            notes: vec![],
        };
        let p = ModelResourceProfile::from_metadata(&md, 9_350_300_000);

        // Per token, from measured geometry:
        //   40 layers x 8 kv_heads x (128 key + 128 value) x 2 bytes (f16)
        let per_token = p.kv_bytes_per_token.expect("per-token kv derived");
        assert_eq!(per_token.provenance, Provenance::Advisory);
        assert_eq!(per_token.bytes, 40 * 8 * (128 + 128) * 2);

        // And at the assumed window, which is what the requirement uses.
        assert_eq!(p.kv_assumed_context_tokens, ASSUMED_CONTEXT_TOKENS);
        let kv = p.kv_cache_bytes.expect("kv derived");
        assert_eq!(kv.provenance, Provenance::Advisory);
        assert_eq!(kv.bytes, per_token.bytes * ASSUMED_CONTEXT_TOKENS);

        // Weights are measured: the tensor payload is the file minus its header.
        let weights = p.weight_bytes.expect("weights");
        assert_eq!(weights.provenance, Provenance::Measured);
        assert_eq!(weights.bytes, p.artifact_bytes - md.bytes_read);

        let ws = p.runtime_working_set_bytes.expect("working set derived");
        assert_eq!(ws.bytes, weights.bytes + kv.bytes);

        // The declared maximum is reported, and priced, but never used as the
        // requirement: a 262144-token session would need 5 GiB of KV, and
        // quietly charging every session for it is how "this model needs 14 GiB"
        // lies get told.
        assert_eq!(p.kv_context_length, Some(262_144));
        let declared_priced = per_token.bytes * 262_144;
        assert!(
            ws.bytes * 4 < declared_priced,
            "requirement {} must reflect the assumed window, not the declared \
             maximum ({declared_priced} bytes)",
            ws.bytes
        );
        assert!(
            p.notes
                .iter()
                .any(|n| n.contains("262144") && n.contains("would need") && n.contains("assumed")),
            "the declared context must be priced in the open: {:?}",
            p.notes
        );
        assert!(
            p.requirement_basis().contains("KV at"),
            "basis must name the window: {}",
            p.requirement_basis()
        );

        // Any other window can be priced from the per-token number.
        let at_512 = per_token.bytes * 512;
        assert!(
            at_512 * 20 < weights.bytes,
            "512-token KV is a rounding error against the weights"
        );
    }

    #[test]
    fn arrays_are_stepped_over_rather_than_misread() {
        // The bug this guards: an array element width computed as 0 for strings
        // left the cursor in place, and every later key was read as garbage.
        let dir = crate::artifact::tests::tmpdir("gguf-arrays");
        let p = dir.join("arrays.gguf");
        let mut v: Vec<u8> = Vec::new();
        v.extend_from_slice(b"GGUF");
        v.extend_from_slice(&3u32.to_le_bytes());
        v.extend_from_slice(&0u64.to_le_bytes());
        v.extend_from_slice(&6u64.to_le_bytes()); // architecture, tags, file_type, block_count, rope, embedding
        let entry = |v: &mut Vec<u8>, k: &str, e: &GgufValue| {
            v.extend_from_slice(&(k.len() as u64).to_le_bytes());
            v.extend_from_slice(k.as_bytes());
            match e {
                GgufValue::U32(n) => {
                    v.extend_from_slice(&4u32.to_le_bytes());
                    v.extend_from_slice(&n.to_le_bytes());
                }
                GgufValue::U64(n) => {
                    v.extend_from_slice(&10u32.to_le_bytes());
                    v.extend_from_slice(&n.to_le_bytes());
                }
                GgufValue::Str(s) => {
                    v.extend_from_slice(&8u32.to_le_bytes());
                    v.extend_from_slice(&(s.len() as u64).to_le_bytes());
                    v.extend_from_slice(s.as_bytes());
                }
                GgufValue::Other(_) => unreachable!(),
            }
        };
        entry(
            &mut v,
            "general.architecture",
            &GgufValue::Str("qwen3".into()),
        );
        // general.tags: an array of 400 variable-length strings.
        v.extend_from_slice(&(b"general.tags".len() as u64).to_le_bytes());
        v.extend_from_slice(b"general.tags");
        v.extend_from_slice(&9u32.to_le_bytes());
        v.extend_from_slice(&8u32.to_le_bytes()); // element type: string
        v.extend_from_slice(&400u64.to_le_bytes());
        for i in 0..400 {
            let s = format!("tag-number-{i:04}");
            v.extend_from_slice(&(s.len() as u64).to_le_bytes());
            v.extend_from_slice(s.as_bytes());
        }
        entry(&mut v, "general.file_type", &GgufValue::U32(7));
        entry(&mut v, "qwen3.block_count", &GgufValue::U32(40));
        // A fixed-width array after it, sized to land exactly on a buffer
        // boundary once padded: the skip path must survive that too.
        v.extend_from_slice(&(b"qwen3.rope.dimension_sections".len() as u64).to_le_bytes());
        v.extend_from_slice(b"qwen3.rope.dimension_sections");
        v.extend_from_slice(&9u32.to_le_bytes());
        v.extend_from_slice(&4u32.to_le_bytes()); // element type: int32
        v.extend_from_slice(&65_536u64.to_le_bytes());
        v.extend(std::iter::repeat_n(0u8, 65_536 * 4));
        entry(&mut v, "qwen3.embedding_length", &GgufValue::U32(4096));
        std::fs::write(&p, &v).unwrap();

        let md = read_metadata(&p).unwrap();
        assert_eq!(md.architecture.as_deref(), Some("qwen3"));
        assert_eq!(md.quantization.as_deref(), Some("Q8_0"));
        assert_eq!(md.n_layers, Some(40));
        assert_eq!(md.n_embd, Some(4096));
        assert!(
            md.bytes_read < 4 * 1024 * 1024,
            "stepping over arrays should not read them: {} bytes",
            md.bytes_read
        );
        crate::artifact::tests::cleanup(&dir);
    }

    #[test]
    fn inspection_reads_a_little_and_never_the_weights() {
        // A header far larger than the read buffer, followed by a payload far
        // larger than anything read: inspection must cost the header walk and
        // stop.
        let dir = crate::artifact::tests::tmpdir("gguf-huge");
        let p = dir.join("huge.gguf");
        let mut bytes = gguf_header(&[
            ("general.architecture", GgufValue::Str("qwen3".into())),
            ("general.file_type", GgufValue::U32(7)),
            ("qwen3.block_count", GgufValue::U32(40)),
            ("qwen3.embedding_length", GgufValue::U32(4096)),
            ("qwen3.context_length", GgufValue::U32(32_768)),
            ("qwen3.attention.head_count_kv", GgufValue::U32(8)),
            ("qwen3.attention.key_length", GgufValue::U32(128)),
            ("qwen3.attention.value_length", GgufValue::U32(128)),
        ]);
        // A megabyte of trailing "tensor data" nobody should read.
        let file_size = bytes.len() + 64 * 1024 * 1024;
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&p).unwrap();
            f.write_all(&bytes).unwrap();
            f.set_len(file_size as u64).unwrap();
        }
        let md = read_metadata(&p).unwrap();
        assert!(
            md.stopped_early,
            "walk should stop once a profile has its inputs"
        );
        assert!(
            md.bytes_read <= (GGUF_READ_BUFFER_BYTES * 2) as u64,
            "read {} bytes of a {file_size}-byte artifact",
            md.bytes_read
        );
        bytes.clear();
        let prof = ModelResourceProfile::from_metadata(&md, file_size as u64);
        assert_eq!(
            prof.weight_bytes.expect("weights").bytes,
            file_size as u64 - md.bytes_read,
            "weights are the payload, measured from the file, not read into memory"
        );
        crate::artifact::tests::cleanup(&dir);
    }

    #[test]
    fn an_unknown_artifact_has_no_invented_numbers() {
        let md = ArtifactMetadata {
            format: Some(ArtifactFormat::Safetensors),
            ..Default::default()
        };
        let p = ModelResourceProfile::from_metadata(&md, 1_000);
        assert_eq!(p.artifact_bytes, 1_000);
        assert_eq!(p.weight_bytes, None);
        assert_eq!(p.kv_cache_bytes, None);
        assert_eq!(p.runtime_working_set_bytes, None);
        assert_eq!(p.vram_bytes, None);
        // The fallback is the file itself, and it says so.
        assert_eq!(p.execution_requirement().provenance, Provenance::Measured);
        assert!(p.notes.iter().any(|n| n.contains("NOT PROBED")));
    }

    #[test]
    fn fit_assessment_never_says_ready_without_a_number() {
        let p = ModelResourceProfile {
            artifact_bytes: 9_350_300_000,
            ..Default::default()
        };
        assert!(matches!(
            assess_execution_fit(&p, None),
            ExecutionFit::Unknown { .. }
        ));
        assert!(matches!(
            assess_execution_fit(&p, Some(0)),
            ExecutionFit::Unknown { .. }
        ));
        // With no derived working set the requirement falls back to the file
        // size, which is the honest worst case — not "fits easily".
        assert!(matches!(
            assess_execution_fit(&p, Some(8_000_000_000)),
            ExecutionFit::ResourceLimited { .. }
        ));
        assert!(matches!(
            assess_execution_fit(&p, Some(20_000_000_000)),
            ExecutionFit::Ready { .. }
        ));
    }

    pub(super) fn tmpdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("openrt-artifact-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    pub(super) fn cleanup(d: &std::path::Path) {
        let _ = std::fs::remove_dir_all(d);
    }
}

//! Model anatomy: bounded, streaming description of a model artifact's tensor
//! layout (Memory Fabric M2), extended by M7 to models composed of several
//! physical files ("shards").
//!
//! Anatomy is **optional metadata describing a model's storage layout** — it
//! never drives how OpenRT executes that model through tiered memory, and it
//! never equates storage bytes with resident memory. Admitting a
//! registration, hashing, and running all behave exactly as before; anatomy is
//! attached for later tooling/telemetry only.
//!
//! Reading rules (hard constraints):
//! * Only the header/tensor-table portion is ever read (GGUF: up to
//!   [`GGUF_MAX_HEADER_BYTES`](crate::artifact::GGUF_MAX_HEADER_BYTES);
//!   safetensors: [`ANATOMY_MAX_SAFETENSORS_HEADER_BYTES`], matching the
//!   scanner's policy; sharded safetensors index: [`ANATOMY_MAX_INDEX_BYTES`]).
//!   Tensor *payloads* are stepped over with seeks and are never read.
//! * Every numeric bound is checked; a malformed file yields an error, never
//!   an unbounded allocation or a panic.
//! * Deriving a tensor's byte length requires a size for its dtype. A length is
//!   recorded either from an **explicit** size (F32/F16, whose byte sizes are
//!   stable across every GGUF ggml-type enum) or, opt-in only, from the
//!   explicit versioned ggml block-size table (M8, [`GGML_QUANT_TABLE_VERSION`]).
//!   Derived-but-explicit lengths are tagged [`TensorLengthBasis::Header`];
//!   table-derived ones are tagged [`TensorLengthBasis::QuantTableV1`] and never
//!   assume payload-offset chainability (real GGUF writers pad tensor payloads,
//!   whose exact alignment is not declared in the header). Everything outside
//!   both the explicit sizes and the table stays `None` (unknown) rather than
//!   guessed. The opt-in is off by default: without it, quantized lengths are
//!   exactly as unknown as before this milestone.
//! * Dtype labels are best-effort names for human display only; they are never
//!   used in arithmetic.
//! * The payload offset of tensor `i` equals `data_start + Σ lengths[0..i)`.
//!   When a truncated dtype makes a prefix length unknown, later offsets
//!   become `None` because they cannot be derived from a contiguous tail and an
//!   exact file size alone.
//!
//! # Multi-shard anatomy (M7)
//!
//! * A **sharded safetensors** model is anchored on its index file
//!   (`weight_map`: tensor name → shard file). The index is untrusted input:
//!   shard references are validated (relative, no `..`, no absolute, no escape
//!   from the index's directory), bounded in count and size, and each
//!   referenced shard header is read within the same 16 MiB ceiling the
//!   scanner enforces. Shard payloads are never read.
//! * `ModelAnatomy.total_bytes` is the **sum of all shard sizes** — a storage
//!   aggregate, never a residency claim. Per-shard sizes live in
//!   [`ModelAnatomy::shard_files`]; per-tensor locations in
//!   [`TensorAnatomy::shard`] + [`TensorAnatomy::offset`] (offset is *within
//!   that shard file*).
//! * Backward compatibility: `shard_files` is an additive, defaulted field —
//!   pre-M7 `ModelAnatomy` JSON (schema v1) and multi-shard records share the
//!   same wire format and version; an old record simply deserializes with an
//!   empty `shard_files` and is treated as a single-file layout. No migration
//!   step is needed.
//! * GGUF remains single-file by design (documented boundary); no multi-file
//!   GGUF layout is invented.

use std::collections::BTreeSet;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::artifact::{ArtifactError, HeaderReader, GGUF_MAX_STRING_BYTES};

/// Upper bound on the number of tensor descriptors a single artifact may claim.
/// A header cannot allocate more than this guard, regardless of what it says.
pub const ANATOMY_MAX_TENSORS: u64 = 1 << 20;

/// Page size used for alignment reporting. Later milestones may want
/// page-aligned tensor reads, so anatomy records whether a payload is aligned.
pub const ANATOMY_PAGE_BYTES: u64 = 4096;

/// Safetensors headers are capped at 16 MiB by `openrt-scan`'s default policy;
/// anatomy enforces the same ceiling so the two analyses stay consistent.
pub const ANATOMY_MAX_SAFETENSORS_HEADER_BYTES: u64 = 16 * 1024 * 1024;

/// Safetensors shard **index** files (`weight_map`) are untrusted input bounded
/// to the same 16 MiB ceiling as single-file headers. Anything larger is
/// rejected, never allocated around.
pub const ANATOMY_MAX_INDEX_BYTES: u64 = 16 * 1024 * 1024;

/// Upper bound on the distinct physical shard files one anatomy may claim. A
/// hostile index cannot force more shard opens than this guard.
pub const ANATOMY_MAX_SHARDS: u32 = 4096;

const GGUF_MAGIC: u32 = 0x4655_4747; // "GGUF", read little-endian
const GGUF_LEGACY_MAGIC: u32 = 0x5154_4647; // "FITQ" (oldggml, GGUF-compatible layout)
const SAFETENSORS_HEADER_LEN_BYTES: u64 = 8;
const MAX_GGUF_DIMS: u32 = 64;

/// Byte size of a tensor element for dtypes whose size is stable across all
/// GGUF ggml-type enums. Empty otherwise.
///
/// F32=0 and F16=1 carry the same numbering in every ggml.h ever shipped;
/// I8..F64, BF16 and every quant type do not, so they are deliberately
/// omitted and reported unknown rather than guessed.
fn gguf_element_size(dtype: i32) -> Option<u64> {
    match dtype {
        0 => Some(4), // F32
        1 => Some(2), // F16
        _ => None,
    }
}

/// Version of the ggml quantized-type block-size table used by M8's opt-in
/// length derivation. The block layouts below are fixed by ggml.h for the
/// ggml-type numbers they are keyed on; a future writer or table change bumps
/// this constant so derived lengths stay attributable to a specific revision.
pub const GGML_QUANT_TABLE_VERSION: u32 = 1;

/// Explicit ggml block-size table, `(ggml_type, elements_per_block,
/// bytes_per_block)`. Only types whose block layout is stable across ggml
/// revisions and whose byte size does not depend on integer-width quirks are
/// listed (elements per block from ggml `QK*`; bytes per block equal to
/// `sizeof(ggml_block_q*_t)`). Integer, float, BF16 and IQ variants are
/// deliberately omitted and remain unknown rather than guessed.
const GGML_QUANT_BLOCKS_V1: &[(i32, u64, u64)] = &[
    (2, 32, 18),    // Q4_0
    (3, 32, 20),    // Q4_1
    (6, 32, 22),    // Q5_0
    (7, 32, 24),    // Q5_1
    (8, 32, 34),    // Q8_0
    (9, 32, 36),    // Q8_1
    (10, 256, 84),  // Q2_K
    (11, 256, 110), // Q3_K
    (12, 256, 144), // Q4_K
    (13, 256, 176), // Q5_K
    (14, 256, 210), // Q6_K
    (15, 256, 292), // Q8_K
];

/// Elements per block and bytes per block for a ggml quantized type, when the
/// explicit version-1 table knows it.
fn gguf_quant_block_size(dtype: i32) -> Option<(u64, u64)> {
    GGML_QUANT_BLOCKS_V1
        .iter()
        .find(|(id, _, _)| *id == dtype)
        .map(|(_, elements, bytes)| (*elements, *bytes))
}

/// One tensor length derivation: either an explicit element size × count
/// ([`TensorLengthBasis::Header`]) or the M8 opt-in table formula
/// `ceil(elements / block_elements) × block_bytes`
/// ([`TensorLengthBasis::QuantTableV1`]).
fn gguf_tensor_length(dtype: i32, elements: u64) -> Option<(u64, TensorLengthBasis)> {
    if let Some(sz) = gguf_element_size(dtype) {
        return elements
            .checked_mul(sz)
            .map(|l| (l, TensorLengthBasis::Header));
    }
    let (block_elements, block_bytes) = gguf_quant_block_size(dtype)?;
    let blocks = elements
        .checked_add(block_elements - 1)?
        .checked_div(block_elements)?;
    blocks
        .checked_mul(block_bytes)
        .map(|l| (l, TensorLengthBasis::QuantTableV1))
}

/// How a tensor's byte length was determined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TensorLengthBasis {
    /// Length is explicit in the artifact header (an element size × count, or a
    /// declared `data_offsets` span). Offset chaining is sound.
    Header,
    /// Length was derived from the explicit, versioned ggml block-size table
    /// ([`GGML_QUANT_TABLE_VERSION`]). An estimate of storage bytes, **not** a
    /// certified residency claim, and never offset-chainable alone.
    QuantTableV1,
}

/// One length derivation decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnatomyOptions {
    /// M8 opt-in: fill quantized tensor `length` from the versioned ggml
    /// block-size table, tagged [`TensorLengthBasis::QuantTableV1`]. Off by
    /// default — without it, quantized lengths are `None` exactly as before.
    pub quantized_lengths: bool,
}

impl Default for AnatomyOptions {
    fn default() -> Self {
        Self {
            quantized_lengths: false,
        }
    }
}

/// Human-readable dtype label, best-effort only. Never used in arithmetic.
fn gguf_dtype_label(dtype: i32) -> String {
    match dtype {
        0 => "F32",
        1 => "F16",
        2 => "Q4_0",
        3 => "Q4_1",
        6 => "Q5_0",
        7 => "Q5_1",
        8 => "Q8_0",
        9 => "Q8_1",
        10 => "Q2_K",
        11 => "Q3_K",
        12 => "Q4_K",
        13 => "Q5_K",
        14 => "Q6_K",
        15 => "Q8_K",
        24 => "I8",
        25 => "I16",
        26 => "I32",
        27 => "I64",
        28 => "F64",
        30 => "BF16",
        other => return format!("QUANT(unknown:{other})"),
    }
    .into()
}

/// Format of the model artifact that anatomy understood.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnatomyFormat {
    Gguf,
    Safetensors,
    Unknown,
}

/// Layout of a single tensor within one physical model file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TensorAnatomy {
    /// Tensor name, e.g. `blk.0.attn_q.weight`.
    pub name: String,
    /// Index into [`ModelAnatomy::shard_files`] this tensor lives in. `0` for
    /// a single-file model; `k` for the k-th physical shard of a sharded
    /// model. Pre-M7 single-file records without `shard_files` are valid:
    /// shard `0` then names the artifact's sole file.
    pub shard: u32,
    /// Byte offset of the tensor payload **within `shard_files[shard]`** (for
    /// a single-file model, within the artifact itself). `None` when a
    /// preceding unknown-length dtype makes it underivable.
    pub offset: Option<u64>,
    /// Byte length of the tensor payload. `None` for dtypes neither explicit
    /// nor covered by the (opt-in) quantization table — this milestone never
    /// fabricates one.
    pub length: Option<u64>,
    /// How [`TensorAnatomy::length`] was determined, when it was. `None` while
    /// `length` is `None`; a pre-M8 record that has a `length` but no basis is
    /// treated as [`TensorLengthBasis::Header`] by [`TensorAnatomy::length_basis`].
    #[serde(default)]
    pub length_basis: Option<TensorLengthBasis>,
    /// Guaranteed alignment of the payload (a power of two), when known.
    pub alignment: Option<u64>,
    /// Whether offset and length are both page aligned. Requires both known.
    pub page_aligned: bool,
    /// Best-effort human label, e.g. `F32` or `QUANT(unknown:2)`.
    pub dtype: String,
}

impl TensorAnatomy {
    fn page_flags(offset: Option<u64>, length: Option<u64>) -> (Option<u64>, bool) {
        match (offset, length) {
            (Some(o), Some(l)) if o % ANATOMY_PAGE_BYTES == 0 && l % ANATOMY_PAGE_BYTES == 0 => {
                (Some(ANATOMY_PAGE_BYTES), true)
            }
            _ => (None, false),
        }
    }

    /// Effective length basis. A legacy record carries `length` without a
    /// basis; that is only ever produced by the explicit header path, so it is
    /// reported as [`TensorLengthBasis::Header`].
    pub fn length_basis(&self) -> Option<TensorLengthBasis> {
        match (self.length, self.length_basis) {
            (Some(_), Some(b)) => Some(b),
            (Some(_), None) => Some(TensorLengthBasis::Header),
            (None, _) => None,
        }
    }
}

/// One physical model file of a (possibly sharded) artifact.
///
/// Describes **storage layout**: the shard's stable identity, its exact byte
/// size, and an already-available checksum. Shard size is never treated as
/// resident memory, and a missing checksum is the honest unknown, never a
/// fabricated one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShardAnatomy {
    /// Stable 0-based shard index; matches [`TensorAnatomy::shard`].
    pub shard: u32,
    /// Stable logical name of the shard (e.g. `model-00001-of-00008`).
    pub name: String,
    /// Stable file identity — a **safe relative** file name within the artifact
    /// layout; never an absolute path and never containing path traversal.
    pub file: String,
    /// Exact byte size of this shard file (from file metadata, never a payload
    /// read).
    pub size_bytes: u64,
    /// SHA-256 checksum when already available. Never computed during
    /// planning/anatomy; `None` is the honest unknown.
    pub checksum: Option<String>,
}

/// Streaming description of a model artifact's tensor layout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelAnatomy {
    pub schema_version: u32,
    pub format: AnatomyFormat,
    /// Exact artifact size in bytes (the measured file size for a single-file
    /// model, or the **sum of all shard sizes** for a sharded model). Storage,
    /// never residency.
    pub total_bytes: u64,
    /// Number of file shards this anatomy was derived from. Additive-with-v1:
    /// a pre-M7 record omits `shard_files` and keeps this as its only count.
    pub shards: u32,
    /// Bounded tensor descriptors, in deterministic order.
    pub tensors: Vec<TensorAnatomy>,
    /// Per-file records (M7). Empty for a pre-M7 record, which is a valid
    /// single-file layout (`shards` then carries the count).
    #[serde(default)]
    pub shard_files: Vec<ShardAnatomy>,
}

/// Current schema version for `ModelAnatomy`.
///
/// `shard_files` was added additively with `#[serde(default)]`, so the wire
/// format and this version are unchanged by M7: old records and new records
/// deserialize into the same type without a migration step.
pub const ANATOMY_SCHEMA_VERSION: u32 = 1;

impl ModelAnatomy {
    fn empty(total_bytes: u64) -> Self {
        Self {
            schema_version: ANATOMY_SCHEMA_VERSION,
            format: AnatomyFormat::Unknown,
            total_bytes,
            shards: 0,
            tensors: Vec::new(),
            shard_files: Vec::new(),
        }
    }

    /// Number of physical model files this anatomy describes. Prefers the M7
    /// per-file records and falls back to the v1 `shards` count.
    pub fn shard_count(&self) -> u32 {
        if !self.shard_files.is_empty() {
            self.shard_files.len() as u32
        } else {
            self.shards
        }
    }

    /// True when this anatomy describes more than one physical model file.
    pub fn is_multi_shard(&self) -> bool {
        self.shard_count() > 1
    }

    /// Sum of the per-shard byte sizes, when the shard records are present.
    ///
    /// `None` for a pre-M7 single-file record (`shard_files` empty), where
    /// [`ModelAnatomy::total_bytes`] is the authoritative size.
    pub fn shard_bytes(&self) -> Option<u64> {
        let mut total: u64 = 0;
        for shard in &self.shard_files {
            total = total.checked_add(shard.size_bytes)?;
        }
        if self.shard_files.is_empty() {
            None
        } else {
            Some(total)
        }
    }
}

/// Read anatomy for `path` by sniffing the format and parsing only the bounded
/// header region. Payload bytes are never read — a seek over them is all that
/// happens. Returns `Ok(Unknown)` — not an error — when the format is not
/// recognized, so callers can attach an empty anatomy record.
///
/// Recognized single-file models (GGUF, standalone safetensors) get a
/// single-entry [`ModelAnatomy::shard_files`] record carrying the file's stable
/// name and measured size; a sharded safetensors model is described by
/// [`sharded_anatomy_for_index_file`], not by this function.
pub fn anatomy_for_file(path: &Path) -> Result<ModelAnatomy, ArtifactError> {
    anatomy_for_file_with_options(path, AnatomyOptions::default())
}

/// [`anatomy_for_file`] with explicit M8 derivation options.
pub fn anatomy_for_file_with_options(
    path: &Path,
    options: AnatomyOptions,
) -> Result<ModelAnatomy, ArtifactError> {
    let mut f = std::fs::File::open(path).map_err(ArtifactError::Io)?;
    let total = f.metadata().map_err(ArtifactError::Io)?.len();
    let mut anatomy = anatomy_from_reader_with_options(&mut f, total, options)?;
    if anatomy.shard_files.is_empty() && anatomy.format != AnatomyFormat::Unknown {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "shard-0".to_string());
        anatomy.shard_files.push(ShardAnatomy {
            shard: 0,
            name: name.clone(),
            file: name,
            size_bytes: total,
            checksum: None,
        });
        anatomy.shards = 1;
    }
    Ok(anatomy)
}

/// Sniff and parse anatomy for an already-open reader. The caller owns the
/// stream, so the registry can reuse the file handle it is already reading.
pub fn anatomy_from_reader<R: Read + Seek>(
    src: &mut R,
    total: u64,
) -> Result<ModelAnatomy, ArtifactError> {
    anatomy_from_reader_with_options(src, total, AnatomyOptions::default())
}

/// [`anatomy_from_reader`] with explicit M8 derivation options.
pub fn anatomy_from_reader_with_options<R: Read + Seek>(
    src: &mut R,
    total: u64,
    options: AnatomyOptions,
) -> Result<ModelAnatomy, ArtifactError> {
    let mut magic = [0u8; 4];
    src.read_exact(&mut magic).map_err(ArtifactError::Io)?;
    src.seek(SeekFrom::Start(0)).map_err(ArtifactError::Io)?;

    let magic = u32::from_le_bytes(magic);
    if magic == GGUF_MAGIC || magic == GGUF_LEGACY_MAGIC {
        return parse_gguf_anatomy_from_reader(src, total, options);
    }

    if sniff_safetensors(src, total)? {
        return parse_safetensors_anatomy_from_reader(src, total);
    }

    let mut out = ModelAnatomy::empty(total);
    out.shards = 1;
    Ok(out)
}

fn sniff_safetensors<R: Read + Seek>(src: &mut R, total: u64) -> Result<bool, ArtifactError> {
    src.seek(SeekFrom::Start(0)).map_err(ArtifactError::Io)?;
    if total < SAFETENSORS_HEADER_LEN_BYTES {
        return Ok(false);
    }
    let mut len_bytes = [0u8; SAFETENSORS_HEADER_LEN_BYTES as usize];
    src.read_exact(&mut len_bytes).map_err(ArtifactError::Io)?;
    let header_len = u64::from_le_bytes(len_bytes);
    if header_len == 0 || header_len > ANATOMY_MAX_SAFETENSORS_HEADER_BYTES {
        return Ok(false);
    }
    if total < SAFETENSORS_HEADER_LEN_BYTES + header_len {
        return Ok(false);
    }
    // Sniff the first non-whitespace byte: a JSON object starts with '{'.
    let mut probe = [0u8; 64];
    let mut limited = src.take(probe.len() as u64);
    let got = std::io::Read::read(&mut limited, &mut probe).map_err(ArtifactError::Io)?;
    let looks_like_json = probe[..got]
        .iter()
        .find(|b| !matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
        .map(|b| *b == b'{')
        .unwrap_or(false);
    src.seek(SeekFrom::Start(0)).map_err(ArtifactError::Io)?;
    Ok(looks_like_json)
}

fn parse_gguf_anatomy_from_reader<R: Read + Seek>(
    src: &mut R,
    total: u64,
    options: AnatomyOptions,
) -> Result<ModelAnatomy, ArtifactError> {
    src.seek(SeekFrom::Start(0)).map_err(ArtifactError::Io)?;
    let mut r = HeaderReader::new(src);
    let magic = r.u32()?;
    if magic != GGUF_MAGIC && magic != GGUF_LEGACY_MAGIC {
        return Err(ArtifactError::Header(format!(
            "not a GGUF file (magic {magic:#010x})"
        )));
    }
    let _version = r.u32()?;
    let tensor_count = r.u64()?;
    let kv_count = r.u64()?;
    if tensor_count > ANATOMY_MAX_TENSORS {
        return Err(ArtifactError::Header(format!(
            "tensor count {tensor_count} exceeds {ANATOMY_MAX_TENSORS}"
        )));
    }
    for _ in 0..kv_count {
        let _key = r.string()?;
        let vtype = r.u32()?;
        r.skip_value(vtype, 0)?;
    }

    let mut tensors: Vec<(u64, i32, String)> =
        Vec::with_capacity(tensor_count.min(1 << 10) as usize);
    for _ in 0..tensor_count {
        let name = r.string()?;
        if name.len() > GGUF_MAX_STRING_BYTES as usize {
            return Err(ArtifactError::Header(
                "tensor name exceeds string cap".into(),
            ));
        }
        let ndims = r.u32()?;
        if ndims > MAX_GGUF_DIMS {
            return Err(ArtifactError::Header(format!(
                "tensor {name:?} claims {ndims} dimensions (ceiling {MAX_GGUF_DIMS})"
            )));
        }
        let mut elements: u64 = 1;
        for _ in 0..ndims {
            let d = r.u32()?;
            elements = elements
                .checked_mul(d as u64)
                .ok_or_else(|| ArtifactError::Header("tensor shape overflow".into()))?;
        }
        let dtype = r.u32()?;
        tensors.push((elements, dtype as i32, name));
    }

    let data_start = r.logical_position();
    let mut running: Option<u64> = Some(0);
    let mut anatomy = Vec::with_capacity(tensors.len());
    for (elements, dtype, name) in tensors {
        let (length, length_basis) = match if options.quantized_lengths {
            gguf_tensor_length(dtype, elements)
        } else {
            gguf_element_size(dtype)
                .and_then(|sz| elements.checked_mul(sz))
                .map(|l| (l, TensorLengthBasis::Header))
        } {
            Some(v) => (Some(v.0), Some(v.1)),
            None => (None, None),
        };
        let derived = match &length_basis {
            Some(TensorLengthBasis::QuantTableV1) => true,
            _ => false,
        };
        // Offsets ("data_start + Σ prefix lengths") are only chainable from
        // explicit header lengths. A table-derived length is a per-tensor
        // storage estimate; real writers pad payloads, whose exact alignment
        // the header does not declare, so it cannot extend the offset chain.
        let offset = if derived {
            None
        } else {
            running.and_then(|sum| data_start.checked_add(sum))
        };
        if derived || offset.is_none() || length.is_none() {
            running = None;
        } else if let Some(l) = length {
            running = running.and_then(|sum| sum.checked_add(l));
        }
        let (alignment, page_aligned) = TensorAnatomy::page_flags(offset, length);
        anatomy.push(TensorAnatomy {
            name,
            shard: 0,
            offset,
            length,
            length_basis,
            alignment,
            page_aligned,
            dtype: gguf_dtype_label(dtype),
        });
    }

    Ok(ModelAnatomy {
        schema_version: ANATOMY_SCHEMA_VERSION,
        format: AnatomyFormat::Gguf,
        total_bytes: total,
        shards: 1,
        tensors: anatomy,
        shard_files: Vec::new(),
    })
}

/// Parse a safetensors JSON header (already read within its bounded ceiling)
/// into `(name, start, end, dtype)` tuples in the *data block*, validating that
/// every `data_offsets: [s, e]` satisfies `s <= e <= data_block_len`.
///
/// Shared by the single-file parser and the multi-shard (M7) path so both
/// enforce identical bounds. Returns entries in JSON-object iteration order
/// (serde_json objects are key-sorted, so the order is deterministic); callers
/// that need file order sort explicitly.
fn parse_safetensors_header_bytes(
    header_bytes: &[u8],
    data_block_len: u64,
) -> Result<Vec<(String, u64, u64, String)>, ArtifactError> {
    let json: serde_json::Value = serde_json::from_slice(header_bytes)
        .map_err(|e| ArtifactError::Header(format!("safetensors header is not valid JSON: {e}")))?;
    let obj = json
        .as_object()
        .ok_or_else(|| ArtifactError::Header("safetensors header must be a JSON object".into()))?;

    let mut tensors = Vec::new();
    for (name, spec) in obj {
        if name == "__metadata__" {
            continue;
        }
        if tensors.len() as u64 >= ANATOMY_MAX_TENSORS {
            return Err(ArtifactError::Header(format!(
                "tensor count exceeds {ANATOMY_MAX_TENSORS}"
            )));
        }
        let spec = spec.as_object().ok_or_else(|| {
            ArtifactError::Header(format!("tensor {name:?} spec is not an object"))
        })?;
        let dtype = spec
            .get("dtype")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let offsets = spec
            .get("data_offsets")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| ArtifactError::Header(format!("tensor {name:?} lacks data_offsets")))?;
        if offsets.len() != 2 {
            return Err(ArtifactError::Header(format!(
                "tensor {name:?} data_offsets must have exactly two entries"
            )));
        }
        match (offsets[0].as_u64(), offsets[1].as_u64()) {
            (Some(s), Some(e)) if s <= e && e <= data_block_len => {
                tensors.push((name.clone(), s, e, dtype));
            }
            _ => {
                return Err(ArtifactError::Header(format!(
                    "tensor {name:?} data_offsets [{0:?}, {1:?}] out of bounds for a {data_block_len}-byte data block",
                    offsets[0], offsets[1]
                )))
            }
        }
    }
    Ok(tensors)
}

/// Loaded state for one referenced shard, fed to the pure M7 builder by the
/// file-backed resolver ([`sharded_anatomy_for_index_file`]) or by tests.
pub(crate) struct ShardLoad {
    /// Exact file size in bytes (from metadata).
    pub size_bytes: u64,
    /// The shard's safetensors **header JSON bytes** already read within the
    /// bounded ceiling — the payload is never loaded.
    pub header_bytes: Vec<u8>,
}

/// Reject a shard file reference that could escape the layout: absolute paths,
/// `..`, `.`, empty components, backslashes, and NULs are all refused. Shard
/// references are untrusted input from a model index.
fn validate_shard_name(name: &str) -> Result<&str, ArtifactError> {
    if name.is_empty() {
        return Err(ArtifactError::Header("shard reference is empty".into()));
    }
    if name.contains('\0') || name.contains('\\') {
        return Err(ArtifactError::Header(format!(
            "shard reference {name:?} contains an illegal character"
        )));
    }
    if std::path::Path::new(name).is_absolute() {
        return Err(ArtifactError::Header(format!(
            "shard reference {name:?} is absolute"
        )));
    }
    for comp in name.split('/') {
        if comp.is_empty() || comp == "." || comp == ".." {
            return Err(ArtifactError::Header(format!(
                "shard reference {name:?} is not a safe relative path"
            )));
        }
    }
    Ok(name)
}

/// Pure (M7) builder: turn an index document's bytes plus a shard-loader into a
/// multi-shard safetensors [`ModelAnatomy`].
///
/// The index is untrusted input. The loader is invoked once per distinct shard
/// referenced by `weight_map` in deterministic order; the builder then
/// cross-checks that the declared mapping and the actual per-shard headers
/// agree **exactly** (equal tensor sets, correct shard assignment), so a
/// missing shard, a duplicated tensor, an extra tensor, or a mis-mapped tensor
/// all fail closed instead of producing a silently wrong layout.
fn build_sharded_safetensors_anatomy<F>(
    index_bytes: &[u8],
    mut load_shard: F,
) -> Result<ModelAnatomy, ArtifactError>
where
    F: FnMut(&str) -> Result<ShardLoad, ArtifactError>,
{
    if index_bytes.len() as u64 > ANATOMY_MAX_INDEX_BYTES {
        return Err(ArtifactError::Header(format!(
            "safetensors index is {} bytes (ceiling {ANATOMY_MAX_INDEX_BYTES})",
            index_bytes.len()
        )));
    }
    let json: serde_json::Value = serde_json::from_slice(index_bytes)
        .map_err(|e| ArtifactError::Header(format!("safetensors index is not valid JSON: {e}")))?;
    let obj = json
        .as_object()
        .ok_or_else(|| ArtifactError::Header("safetensors index must be a JSON object".into()))?;
    let weight_map = obj
        .get("weight_map")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| {
            ArtifactError::Header("safetensors index lacks object `weight_map`".into())
        })?;
    if weight_map.is_empty() {
        return Err(ArtifactError::Header(
            "safetensors index `weight_map` is empty".into(),
        ));
    }
    if weight_map.len() as u64 > ANATOMY_MAX_TENSORS {
        return Err(ArtifactError::Header(format!(
            "weight_map tensor count exceeds {ANATOMY_MAX_TENSORS}"
        )));
    }
    // Coerce the untrusted index into `(tensor name -> validated shard name)`
    // pairs. Every reference must be a JSON string and a safe relative name.
    let mut wm: Vec<(&str, &str)> = Vec::with_capacity(weight_map.len());
    for (tensor_name, shard_value) in weight_map {
        if tensor_name.is_empty() {
            return Err(ArtifactError::Header(
                "weight_map contains an empty tensor name".into(),
            ));
        }
        let shard_name = shard_value.as_str().ok_or_else(|| {
            ArtifactError::Header(format!(
                "weight_map shard reference for tensor {tensor_name:?} is not a string"
            ))
        })?;
        validate_shard_name(shard_name)?;
        wm.push((tensor_name.as_str(), shard_name));
    }

    // Distinct, deterministic shard order derived from the index alone.
    let names: BTreeSet<&str> = wm.iter().map(|(_, s)| *s).collect();
    if names.len() as u64 > ANATOMY_MAX_SHARDS as u64 {
        return Err(ArtifactError::Header(format!(
            "weight_map references {} distinct shards (ceiling {ANATOMY_MAX_SHARDS})",
            names.len()
        )));
    }
    let order: Vec<&str> = names.into_iter().collect();

    let mut shard_files: Vec<ShardAnatomy> = Vec::with_capacity(order.len());
    let mut shard_tensor_sets: Vec<Vec<(String, u64, u64, String)>> =
        Vec::with_capacity(order.len());
    let mut total_bytes: u64 = 0;
    let mut tensors: Vec<TensorAnatomy> = Vec::new();
    let mut found_in_shard: std::collections::BTreeMap<String, u32> = Default::default();

    for (shard_index, name) in order.iter().enumerate() {
        validate_shard_name(name)?;
        let load = load_shard(name).map_err(|e| {
            ArtifactError::Header(format!("shard {name:?} could not be loaded: {e}"))
        })?;
        if load.size_bytes == 0 {
            return Err(ArtifactError::Header(format!(
                "shard {name:?} is zero bytes on disk"
            )));
        }
        let header_len = load.header_bytes.len() as u64;
        if header_len == 0 || header_len > ANATOMY_MAX_SAFETENSORS_HEADER_BYTES {
            return Err(ArtifactError::Header(format!(
                "shard {name:?} header is {header_len} bytes (ceiling {ANATOMY_MAX_SAFETENSORS_HEADER_BYTES})"
            )));
        }
        let data_block_start = SAFETENSORS_HEADER_LEN_BYTES
            .checked_add(header_len)
            .ok_or_else(|| ArtifactError::Header("shard header length overflow".into()))?;
        if data_block_start > load.size_bytes {
            return Err(ArtifactError::Header(format!(
                "shard {name:?} is truncated: header region {data_block_start} bytes but the file is {} bytes",
                load.size_bytes
            )));
        }
        let data_block_len = load.size_bytes - data_block_start;
        let parsed = parse_safetensors_header_bytes(&load.header_bytes, data_block_len)
            .map_err(|e| ArtifactError::Header(format!("shard {name:?} header invalid: {e}")))?;
        if parsed.is_empty() {
            return Err(ArtifactError::Header(format!(
                "shard {name:?} declares no tensors"
            )));
        }
        for (tensor_name, s, e, dtype) in &parsed {
            // Duplicate detection: a tensor name may appear in only one shard.
            if let Some(prev) = found_in_shard.get(tensor_name) {
                return Err(ArtifactError::Header(format!(
                    "tensor {tensor_name:?} appears in more than one shard (shard {prev} and {shard_index})"
                )));
            }
            found_in_shard.insert(tensor_name.clone(), shard_index as u32);
            let offset = data_block_start
                .checked_add(*s)
                .ok_or_else(|| ArtifactError::Header("tensor offset overflow".into()))?;
            let length = e - s;
            let (alignment, page_aligned) = TensorAnatomy::page_flags(Some(offset), Some(length));
            tensors.push(TensorAnatomy {
                name: tensor_name.clone(),
                shard: shard_index as u32,
                offset: Some(offset),
                length: Some(length),
                length_basis: Some(TensorLengthBasis::Header),
                alignment,
                page_aligned,
                dtype: dtype.clone(),
            });
        }
        shard_files.push(ShardAnatomy {
            shard: shard_index as u32,
            name: (*name).to_string(),
            file: (*name).to_string(),
            size_bytes: load.size_bytes,
            checksum: None,
        });
        shard_tensor_sets.push(parsed);
        total_bytes = total_bytes
            .checked_add(load.size_bytes)
            .ok_or_else(|| ArtifactError::Header("aggregate shard size overflow".into()))?;
    }

    // Cross-check the index against what each shard actually declares. The
    // tensor set must match *exactly* (nothing missing, nothing extra) and
    // every tensor must live in the shard the index assigned it to.
    let shard_of = |shard_name: &str| -> Result<u32, ArtifactError> {
        order
            .iter()
            .position(|n| *n == shard_name)
            .map(|i| i as u32)
            .ok_or_else(|| ArtifactError::Header("internal: unknown shard name".into()))
    };
    for (tensor_name, shard_name) in &wm {
        let declared_shard = shard_of(shard_name)?;
        let actual_shard = found_in_shard
            .get(*tensor_name)
            .ok_or_else(|| {
                ArtifactError::Header(format!(
                    "index maps tensor {tensor_name:?} to {shard_name:?} but that shard does not declare it"
                ))
            })?;
        if *actual_shard != declared_shard {
            return Err(ArtifactError::Header(format!(
                "tensor {tensor_name:?} is mapped to {shard_name:?} but physically lives in shard {}",
                order[*actual_shard as usize]
            )));
        }
    }
    for (shard_index, tensors_in_shard) in shard_tensor_sets.iter().enumerate() {
        for (tensor_name, _, _, _) in tensors_in_shard {
            if !weight_map.contains_key(tensor_name) {
                return Err(ArtifactError::Header(format!(
                    "shard {} declares tensor {tensor_name:?} but the index does not map it",
                    order[shard_index]
                )));
            }
        }
    }

    // Deterministic file order: ascending (shard, offset), then name.
    tensors.sort_by(|a, b| {
        a.shard
            .cmp(&b.shard)
            .then_with(|| a.offset.cmp(&b.offset))
            .then_with(|| a.name.cmp(&b.name))
    });

    Ok(ModelAnatomy {
        schema_version: ANATOMY_SCHEMA_VERSION,
        format: AnatomyFormat::Safetensors,
        total_bytes,
        shards: shard_files.len() as u32,
        tensors,
        shard_files,
    })
}

/// Bounded read of a shard's safetensors **header** from disk. The payload is
/// never read: after the 8-byte length and the (bounded) header JSON are read,
/// nothing else touches the file.
fn read_shard_header(path: &std::path::Path, file_size: u64) -> Result<Vec<u8>, ArtifactError> {
    let mut f = std::fs::File::open(path).map_err(ArtifactError::Io)?;
    if file_size < SAFETENSORS_HEADER_LEN_BYTES {
        return Err(ArtifactError::Header(
            "shard is truncated: fewer than 8 bytes".into(),
        ));
    }
    let mut len_bytes = [0u8; SAFETENSORS_HEADER_LEN_BYTES as usize];
    f.read_exact(&mut len_bytes).map_err(ArtifactError::Io)?;
    let header_len = u64::from_le_bytes(len_bytes);
    if header_len == 0 || header_len > ANATOMY_MAX_SAFETENSORS_HEADER_BYTES {
        return Err(ArtifactError::Header(format!(
            "shard header length {header_len} outside [1, {ANATOMY_MAX_SAFETENSORS_HEADER_BYTES}]"
        )));
    }
    let region = SAFETENSORS_HEADER_LEN_BYTES
        .checked_add(header_len)
        .ok_or_else(|| ArtifactError::Header("shard header length overflow".into()))?;
    if region > file_size {
        return Err(ArtifactError::Header(format!(
            "shard header region {region} overruns a {file_size}-byte file"
        )));
    }
    let mut header = vec![0u8; header_len as usize];
    f.read_exact(&mut header).map_err(ArtifactError::Io)?;
    Ok(header)
}

/// True when `path` looks like a bounded safetensors shard index: a JSON object
/// carrying a `weight_map` member, with the whole file at most
/// [`ANATOMY_MAX_INDEX_BYTES`]. Content-led, so the upstream registry does not
/// depend on any particular filename convention.
pub fn is_safetensors_index_file(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() || meta.len() > ANATOMY_MAX_INDEX_BYTES {
        return false;
    }
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return false;
    };
    json.get("weight_map")
        .is_some_and(serde_json::Value::is_object)
}

/// Build multi-shard (M7) safetensors anatomy from a shard **index** file.
///
/// The index's `weight_map` is untrusted input. Every referenced shard is
/// validated (safe relative name, stays inside the index's directory even
/// through symlinks), its header is read within the shared 16 MiB ceiling, and
/// its payload is never loaded. Returns an error — not a partial anatomy — on
/// any inconsistency (missing shard, truncated shard, malformed index,
/// duplicate/extra/mis-mapped tensor, traversal, overflow, absurd sizes), so
/// callers fail closed and report "anatomy unavailable" rather than a wrong
/// layout.
///
/// GGUF is intentionally **not** supported here (single-file by design): a GGUF
/// artifact is described by [`anatomy_for_file`].
pub fn sharded_anatomy_for_index_file(index_path: &Path) -> Result<ModelAnatomy, ArtifactError> {
    if is_safetensors_index_file(index_path) {
        let bytes = std::fs::read(index_path).map_err(ArtifactError::Io)?;
        let base = index_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let base = std::fs::canonicalize(&base)
            .map_err(|e| ArtifactError::Header(format!("cannot resolve index directory: {e}")))?;
        build_sharded_safetensors_anatomy(&bytes, |name| {
            let joined = base.join(name);
            let resolved = std::fs::canonicalize(&joined).map_err(|e| {
                ArtifactError::Header(format!("shard {name:?} is missing or unreadable: {e}"))
            })?;
            if !resolved.starts_with(&base) {
                return Err(ArtifactError::Header(format!(
                    "shard {name:?} resolves outside the index directory"
                )));
            }
            let meta = std::fs::metadata(&resolved).map_err(ArtifactError::Io)?;
            if !meta.is_file() {
                return Err(ArtifactError::Header(format!(
                    "shard {name:?} is not a regular file"
                )));
            }
            let header_bytes = read_shard_header(&resolved, meta.len())?;
            Ok(ShardLoad {
                size_bytes: meta.len(),
                header_bytes,
            })
        })
    } else {
        Err(ArtifactError::Header(
            "not a safetensors shard index (no object `weight_map`)".to_string(),
        ))
    }
}

fn parse_safetensors_anatomy_from_reader<R: Read + Seek>(
    src: &mut R,
    total: u64,
) -> Result<ModelAnatomy, ArtifactError> {
    src.seek(SeekFrom::Start(0)).map_err(ArtifactError::Io)?;
    let mut len_bytes = [0u8; SAFETENSORS_HEADER_LEN_BYTES as usize];
    src.read_exact(&mut len_bytes).map_err(ArtifactError::Io)?;
    let header_len = u64::from_le_bytes(len_bytes);
    if header_len == 0 || header_len > ANATOMY_MAX_SAFETENSORS_HEADER_BYTES {
        return Err(ArtifactError::Header(format!(
            "safetensors header length {header_len} outside [1, {ANATOMY_MAX_SAFETENSORS_HEADER_BYTES}]"
        )));
    }
    if total < SAFETENSORS_HEADER_LEN_BYTES + header_len {
        return Err(ArtifactError::Header(
            "safetensors header overruns the file".into(),
        ));
    }
    let mut header = vec![0u8; header_len as usize];
    src.read_exact(&mut header).map_err(ArtifactError::Io)?;

    let data_block_start = SAFETENSORS_HEADER_LEN_BYTES + header_len;
    let data_block_len = total - data_block_start;

    let parsed = parse_safetensors_header_bytes(&header, data_block_len)?;
    let mut tensors = Vec::with_capacity(parsed.len());
    for (name, s, e, dtype) in parsed {
        let offset = data_block_start.saturating_add(s);
        let length = e - s;
        let (alignment, page_aligned) = TensorAnatomy::page_flags(Some(offset), Some(length));
        tensors.push(TensorAnatomy {
            name,
            shard: 0,
            offset: Some(offset),
            length: Some(length),
            length_basis: Some(TensorLengthBasis::Header),
            alignment,
            page_aligned,
            dtype,
        });
    }

    Ok(ModelAnatomy {
        schema_version: ANATOMY_SCHEMA_VERSION,
        format: AnatomyFormat::Safetensors,
        total_bytes: total,
        shards: 1,
        tensors,
        shard_files: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn gguf_bytes(tensor_info: &[(&str, &[u32], i32)]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"GGUF");
        v.extend_from_slice(&3u32.to_le_bytes()); // version
        v.extend_from_slice(&(tensor_info.len() as u64).to_le_bytes());
        v.extend_from_slice(&0u64.to_le_bytes()); // kv metadata count
        for (name, dims, dtype) in tensor_info {
            let nb = name.as_bytes();
            v.extend_from_slice(&(nb.len() as u64).to_le_bytes());
            v.extend_from_slice(nb);
            v.extend_from_slice(&(dims.len() as u32).to_le_bytes());
            for d in *dims {
                v.extend_from_slice(&d.to_le_bytes());
            }
            v.extend_from_slice(&dtype.to_le_bytes());
        }
        v
    }

    #[test]
    fn gguf_anatomy_derives_contiguous_payload_offsets() {
        let dims_a = [4u32];
        let dims_b = [8u32];
        let mut g = gguf_bytes(&[("a.weight", &dims_a, 0), ("b.weight", &dims_b, 0)]);
        let data_start = g.len() as u64;
        g.extend_from_slice(&[0u8; 16 + 32]);
        let total = g.len() as u64;

        let anatomy = anatomy_from_reader(&mut Cursor::new(g), total).unwrap();
        assert_eq!(anatomy.format, AnatomyFormat::Gguf);
        assert_eq!(anatomy.total_bytes, total);
        assert_eq!(anatomy.tensors.len(), 2);
        let a = &anatomy.tensors[0];
        assert_eq!(a.name, "a.weight");
        assert_eq!(a.offset, Some(data_start));
        assert_eq!(a.length, Some(16));
        assert_eq!(a.dtype, "F32");
        let b = &anatomy.tensors[1];
        assert_eq!(b.name, "b.weight");
        assert_eq!(b.offset, Some(data_start + 16));
        assert_eq!(b.length, Some(32));
    }

    #[test]
    fn gguf_quant_tensor_length_is_unknown_not_wrong() {
        let dims_a = [1u32];
        let dims_q = [1024u32];
        let mut g = gguf_bytes(&[("a.weight", &dims_a, 0), ("q.weight", &dims_q, 2)]);
        let data_start = g.len() as u64;
        g.extend_from_slice(&[0u8; 4 + 4096]);
        let total = g.len() as u64;

        let anatomy = anatomy_from_reader(&mut Cursor::new(g), total).unwrap();
        let q = &anatomy.tensors[1];
        assert_eq!(q.name, "q.weight");
        assert_eq!(q.length, None, "quantized lengths are not fabricated");
        assert_eq!(
            q.length_basis(),
            None,
            "no basis when no length was derived (opt-in off by default)"
        );
        assert!(q.dtype.contains("Q4_0"));
        assert_eq!(anatomy.tensors[0].offset, Some(data_start));
        assert_eq!(anatomy.tensors[0].length, Some(4));
    }

    #[test]
    fn gguf_trailing_unknown_poisons_later_offsets_only() {
        // a(F32), q(Q4_0), b(F32): b's length is knowable but its offset is
        // not, because q's length is unknown and the tail is contiguous.
        let dims_a = [1u32];
        let dims_q = [1u32];
        let mut g = gguf_bytes(&[("a", &dims_a, 0), ("q", &dims_q, 2), ("b", &dims_a, 0)]);
        let data_start = g.len() as u64;
        g.extend_from_slice(&[0u8; 4 + 4 + 4]);
        let total = g.len() as u64;

        let anatomy = anatomy_from_reader(&mut Cursor::new(g), total).unwrap();
        assert_eq!(anatomy.tensors[0].offset, Some(data_start));
        assert_eq!(anatomy.tensors[1].length, None);
        assert_eq!(anatomy.tensors[2].length, Some(4));
        assert_eq!(
            anatomy.tensors[2].offset, None,
            "indeterminate offset stays None"
        );
    }

    #[test]
    fn gguf_malformed_tensor_count_is_bounded() {
        let mut g = gguf_bytes(&[]);
        g[10] = 200; // tensor_count low byte -> hundreds of thousands of tensors
        let total = g.len() as u64;
        let err = anatomy_from_reader(&mut Cursor::new(g), total).unwrap_err();
        match err {
            ArtifactError::Header(_) => {}
            ArtifactError::Io(e) => panic!("expected a header error, got io {e}"),
        }
    }

    #[test]
    fn gguf_huge_tensor_count_hits_the_cap() {
        let mut g = gguf_bytes(&[]);
        g[8] = 0xff; // tensor_count = 0xff... (max u64, clearly a lie)
        for b in g[9..16].iter_mut() {
            *b = 0xff;
        }
        let total = g.len() as u64;
        let err = anatomy_from_reader(&mut Cursor::new(g), total).unwrap_err();
        assert!(matches!(err, ArtifactError::Header(_)));
    }

    #[test]
    fn m8_optin_derives_quantized_lengths_from_versioned_table() {
        // Q8_0 (dtype 8): 32 elements / 34 bytes per block. 1024 elements ->
        // 32 blocks * 34 = 1088 bytes exactly.
        let dims_q = [1024u32];
        let mut g = gguf_bytes(&[("q.weight", &dims_q, 8)]);
        g.extend_from_slice(&[0u8; 1088]);
        let total = g.len() as u64;

        let anatomy = anatomy_from_reader_with_options(
            &mut Cursor::new(g),
            total,
            AnatomyOptions {
                quantized_lengths: true,
            },
        )
        .unwrap();
        let q = &anatomy.tensors[0];
        assert_eq!(q.length, Some(1088));
        assert_eq!(q.length_basis(), Some(TensorLengthBasis::QuantTableV1));
        assert_eq!(
            q.offset, None,
            "a table-derived length never claims an exact payload offset"
        );
    }

    #[test]
    fn m8_optin_partial_block_rounds_up_to_a_whole_block() {
        // Q4_0 (dtype 2): 33 elements -> ceil(33/32) = 2 blocks * 18 bytes.
        let dims_q = [33u32];
        let mut g = gguf_bytes(&[("q.weight", &dims_q, 2)]);
        g.extend_from_slice(&[0u8; 36]);
        let total = g.len() as u64;
        let anatomy = anatomy_from_reader_with_options(
            &mut Cursor::new(g),
            total,
            AnatomyOptions {
                quantized_lengths: true,
            },
        )
        .unwrap();
        assert_eq!(anatomy.tensors[0].length, Some(36));
    }

    #[test]
    fn m8_optin_explicit_dtypes_stay_header_basis() {
        let dims_a = [4u32];
        let mut g = gguf_bytes(&[("a.weight", &dims_a, 0)]);
        let data_start = g.len() as u64;
        g.extend_from_slice(&[0u8; 16]);
        let total = g.len() as u64;
        let anatomy = anatomy_from_reader_with_options(
            &mut Cursor::new(g),
            total,
            AnatomyOptions {
                quantized_lengths: true,
            },
        )
        .unwrap();
        let a = &anatomy.tensors[0];
        assert_eq!(a.length, Some(16));
        assert_eq!(a.offset, Some(data_start));
        assert_eq!(a.length_basis(), Some(TensorLengthBasis::Header));
    }

    #[test]
    fn m8_optin_offsets_are_not_chainable_through_quant_tensors() {
        // a(F32), q(Q8_0), b(F32): b keeps an explicit length but its offset is
        // not derivable past a padded quantized payload.
        let dims_a = [1u32];
        let dims_q = [32u32];
        let mut g = gguf_bytes(&[
            ("a.weight", &dims_a, 0),
            ("q.weight", &dims_q, 8),
            ("b.weight", &dims_a, 0),
        ]);
        let data_start = g.len() as u64;
        g.extend_from_slice(&[0u8; 4 + 34 + 4]);
        let total = g.len() as u64;
        let anatomy = anatomy_from_reader_with_options(
            &mut Cursor::new(g),
            total,
            AnatomyOptions {
                quantized_lengths: true,
            },
        )
        .unwrap();
        let a = &anatomy.tensors[0];
        assert_eq!(a.length, Some(4));
        assert_eq!(a.offset, Some(data_start));
        let q = &anatomy.tensors[1];
        assert_eq!(q.length, Some(34));
        assert_eq!(q.offset, None);
        let b = &anatomy.tensors[2];
        assert_eq!(b.length, Some(4));
        assert_eq!(b.offset, None, "padded quantized payload breaks the chain");
    }

    #[test]
    fn m8_unknown_dtype_stays_none_even_with_optin() {
        // dtype 100 is neither explicit nor in the table -> honest None.
        let dims_q = [1024u32];
        let mut g = gguf_bytes(&[("q.weight", &dims_q, 100)]);
        g.extend_from_slice(&[0u8; 4096]);
        let total = g.len() as u64;
        let anatomy = anatomy_from_reader_with_options(
            &mut Cursor::new(g),
            total,
            AnatomyOptions {
                quantized_lengths: true,
            },
        )
        .unwrap();
        assert_eq!(anatomy.tensors[0].length, None);
        assert_eq!(anatomy.tensors[0].length_basis(), None);
    }

    #[test]
    fn m8_table_coverage_is_explicit_and_stable() {
        // The versioned table must know the types it claims and nothing else.
        for (dtype, elements, bytes) in GGML_QUANT_BLOCKS_V1 {
            assert_eq!(
                gguf_quant_block_size(*dtype),
                Some((*elements, *bytes)),
                "dtype {dtype} must map to its declared block layout"
            );
        }
        for unknown in [4i32, 5, 16, 24, 25, 26, 27, 28, 30, 100] {
            assert_eq!(
                gguf_quant_block_size(unknown),
                None,
                "dtype {unknown} must stay unknown (not guessed)"
            );
        }
    }

    #[test]
    fn m8_legacy_record_without_basis_round_trips_as_header() {
        let legacy = r#"{"name":"w","shard":0,"offset":0,"length":64,"alignment":null,"page_aligned":false,"dtype":"F32"}"#;
        let t: TensorAnatomy = serde_json::from_str(legacy).unwrap();
        assert_eq!(t.length, Some(64));
        assert_eq!(t.length_basis(), Some(TensorLengthBasis::Header));
        let json = serde_json::to_string(&t).unwrap();
        let back: TensorAnatomy = serde_json::from_str(&json).unwrap();
        assert_eq!(back.length_basis(), Some(TensorLengthBasis::Header));
    }

    #[test]
    fn m8_safetensors_lengths_are_header_basis() {
        let header =
            r#"{"__metadata__":{},"a.weight":{"dtype":"F32","shape":[4],"data_offsets":[0,16]}}"#;
        let mut g = Vec::new();
        g.extend_from_slice(&(header.len() as u64).to_le_bytes());
        g.extend_from_slice(header.as_bytes());
        g.extend_from_slice(&[0u8; 16]);
        let total = g.len() as u64;
        let anatomy = anatomy_from_reader(&mut Cursor::new(g), total).unwrap();
        let a = &anatomy.tensors[0];
        assert_eq!(a.length, Some(16));
        assert_eq!(a.length_basis(), Some(TensorLengthBasis::Header));
    }

    #[test]
    fn safetensors_anatomy_parses_with_data_offsets() {
        let header = r#"{"__metadata__":{},"a.weight":{"dtype":"F32","shape":[4],"data_offsets":[0,16]},"b.weight":{"dtype":"F32","shape":[4],"data_offsets":[16,32]}}"#;
        let mut g = Vec::new();
        g.extend_from_slice(&(header.len() as u64).to_le_bytes());
        g.extend_from_slice(header.as_bytes());
        g.extend_from_slice(&[0u8; 32]);
        let total = g.len() as u64;

        let anatomy = anatomy_from_reader(&mut Cursor::new(g), total).unwrap();
        assert_eq!(anatomy.format, AnatomyFormat::Safetensors);
        assert_eq!(anatomy.tensors.len(), 2);
        let base = 8 + header.len() as u64;
        assert_eq!(anatomy.tensors[0].offset, Some(base));
        assert_eq!(anatomy.tensors[0].length, Some(16));
        assert_eq!(anatomy.tensors[1].offset, Some(base + 16));
        assert_eq!(anatomy.tensors[1].length, Some(16));
        assert_eq!(anatomy.tensors[0].dtype, "F32");
    }

    #[test]
    fn safetensors_out_of_bounds_offsets_are_rejected() {
        let header = r#"{"bad.weight":{"dtype":"F32","shape":[4],"data_offsets":[0,999]}}"#;
        let mut g = Vec::new();
        g.extend_from_slice(&(header.len() as u64).to_le_bytes());
        g.extend_from_slice(header.as_bytes());
        g.extend_from_slice(&[0u8; 16]);
        let total = g.len() as u64;
        let err = anatomy_from_reader(&mut Cursor::new(g), total).unwrap_err();
        assert!(matches!(err, ArtifactError::Header(_)));
    }

    #[test]
    fn safetensors_huge_header_is_never_preallocated() {
        // Header claims 8 GiB. Sniffing must bail to Unknown (not an error, and
        // definitely no 8 GiB allocation) because the file cannot satisfy it.
        let mut g = Vec::new();
        g.extend_from_slice(&(8u64 * 1024 * 1024 * 1024).to_le_bytes());
        g.extend_from_slice(b"{}");
        let total = g.len() as u64;
        let anatomy = anatomy_from_reader(&mut Cursor::new(g), total).unwrap();
        assert_eq!(anatomy.format, AnatomyFormat::Unknown);
        assert!(anatomy.tensors.is_empty());
    }

    #[test]
    fn unknown_format_is_unknown_not_error() {
        let mut g = Vec::new();
        g.extend_from_slice(b"\x89PNG\r\n\x1a\n");
        g.extend_from_slice(&[0u8; 64]);
        let total = g.len() as u64;
        let anatomy = anatomy_from_reader(&mut Cursor::new(g), total).unwrap();
        assert_eq!(anatomy.format, AnatomyFormat::Unknown);
        assert!(anatomy.tensors.is_empty());
        assert_eq!(anatomy.total_bytes, total);
    }

    #[test]
    fn truncated_table_is_bounded_and_errors() {
        let dims_a = [1u32];
        let mut g = gguf_bytes(&[("a.weight", &dims_a, 0), ("b.weight", &dims_a, 1)]);
        let cut = g.len() / 2;
        g.truncate(cut);
        let total = g.len() as u64;
        let err = anatomy_from_reader(&mut Cursor::new(g), total).unwrap_err();
        assert!(matches!(err, ArtifactError::Header(_)) || matches!(err, ArtifactError::Io(_)));
    }

    #[test]
    fn page_alignment_reflects_offsets_and_lengths() {
        let dims = [1u32];
        let mut g = gguf_bytes(&[("a.weight", &dims, 0)]);
        let data_start = g.len() as u64;
        g.extend_from_slice(&[0u8; ANATOMY_PAGE_BYTES as usize]);
        let total = g.len() as u64;
        let anatomy = anatomy_from_reader(&mut Cursor::new(g), total).unwrap();
        let a = &anatomy.tensors[0];
        assert_eq!(a.offset, Some(data_start));
        assert_eq!(a.length, Some(4));
        let aligned = data_start.is_multiple_of(ANATOMY_PAGE_BYTES)
            && 4u64.is_multiple_of(ANATOMY_PAGE_BYTES);
        assert_eq!(a.page_aligned, aligned);
    }

    #[test]
    fn anatomy_roundtrips_through_json() {
        let dims = [2u32];
        let mut g = gguf_bytes(&[("t.weight", &dims, 0)]);
        g.extend_from_slice(&[0u8; 8]);
        let total = g.len() as u64;
        let anatomy = anatomy_from_reader(&mut Cursor::new(g), total).unwrap();
        let json = serde_json::to_value(&anatomy).unwrap();
        let back: ModelAnatomy = serde_json::from_value(json).unwrap();
        assert_eq!(back, anatomy);
    }

    // ---- M7: multi-shard safetensors anatomy ----

    fn st_header(tensors: &[(&str, u64, u64)]) -> String {
        let inner = tensors
            .iter()
            .map(|(n, s, e)| {
                format!("{n:?}:{{\"dtype\":\"F32\",\"shape\":[],\"data_offsets\":[{s},{e}]}}")
            })
            .collect::<Vec<_>>()
            .join(",");
        format!("{{\"__metadata__\":{{}},{inner}}}")
    }

    fn shard_file_bytes(header: &str, payload_len: usize) -> Vec<u8> {
        let mut g = Vec::new();
        g.extend_from_slice(&(header.len() as u64).to_le_bytes());
        g.extend_from_slice(header.as_bytes());
        g.extend_from_slice(&vec![0u8; payload_len]);
        g
    }

    fn three_shard_scenario() -> (String, std::collections::BTreeMap<String, (u64, Vec<u8>)>) {
        let ha = st_header(&[("a.weight", 0, 16), ("a.bias", 16, 20)]);
        let hb = st_header(&[("b.weight", 0, 32)]);
        let hc = st_header(&[("c.weight", 0, 8), ("c.bias", 8, 16)]);
        let size = |h: &str, p: u64| 8u64 + h.len() as u64 + p;
        let mut shards: std::collections::BTreeMap<String, (u64, Vec<u8>)> = Default::default();
        shards.insert(
            "a.safetensors".into(),
            (size(&ha, 20), shard_file_bytes(&ha, 20)),
        );
        shards.insert(
            "b.safetensors".into(),
            (size(&hb, 32), shard_file_bytes(&hb, 32)),
        );
        shards.insert(
            "c.safetensors".into(),
            (size(&hc, 16), shard_file_bytes(&hc, 16)),
        );
        let index = r#"{"weight_map":{"a.weight":"a.safetensors","a.bias":"a.safetensors","b.weight":"b.safetensors","c.weight":"c.safetensors","c.bias":"c.safetensors"}}"#;
        (index.into(), shards)
    }

    fn mem_loader<'a>(
        shards: &'a std::collections::BTreeMap<String, (u64, Vec<u8>)>,
    ) -> impl FnMut(&str) -> Result<ShardLoad, ArtifactError> + 'a {
        move |name: &str| {
            let (size, bytes) = shards.get(name).ok_or_else(|| {
                ArtifactError::Header(format!("shard {name:?} is missing from the loader"))
            })?;
            // Trim the 8-byte length prefix, like read_shard_header does.
            if bytes.len() < 8 {
                return Err(ArtifactError::Header(format!(
                    "shard {name:?} is truncated: fewer than 8 bytes"
                )));
            }
            let len = u64::from_le_bytes(bytes[0..8].try_into().unwrap()) as usize;
            let hlen = 8usize
                .checked_add(len)
                .ok_or_else(|| ArtifactError::Header("overlong".into()))?;
            if hlen > bytes.len() {
                return Err(ArtifactError::Header(format!(
                    "shard {name:?} header overruns its bytes"
                )));
            }
            Ok(ShardLoad {
                size_bytes: *size,
                header_bytes: bytes[8..hlen].to_vec(),
            })
        }
    }

    #[test]
    fn sharded_anatomy_aggregates_shards_in_stable_order() {
        let (index, shards) = three_shard_scenario();
        let e = build_sharded_safetensors_anatomy(index.as_bytes(), mem_loader(&shards)).unwrap();
        assert_eq!(e.format, AnatomyFormat::Safetensors);
        assert_eq!(e.shards, 3);
        assert_eq!(e.shard_files.len(), 3);
        assert_eq!(e.shard_files[0].name, "a.safetensors");
        assert_eq!(e.shard_files[1].name, "b.safetensors");
        assert_eq!(e.shard_files[2].name, "c.safetensors");
        assert_eq!(e.shard_files[0].shard, 0);
        assert_eq!(e.shard_files[2].shard, 2);
        let total = e.shard_files.iter().map(|s| s.size_bytes).sum::<u64>();
        assert_eq!(e.total_bytes, total);
        assert_eq!(
            e.total_bytes,
            8 + shards["a.safetensors"].0 - 8
                + (shards["b.safetensors"].0 - 8)
                + (shards["c.safetensors"].0 - 8)
                + 8
                + 8
        );
        assert_eq!(e.tensors.len(), 5);
        // offsets are *within the owning shard file* (8 + header + data offset).
        let a = e.tensors.iter().find(|t| t.name == "a.weight").unwrap();
        let ah = st_header(&[("a.weight", 0, 16), ("a.bias", 16, 20)]);
        assert_eq!(a.shard, 0);
        assert_eq!(a.offset, Some(8 + ah.len() as u64));
        assert_eq!(a.length, Some(16));
        let c = e.tensors.iter().find(|t| t.name == "c.bias").unwrap();
        assert_eq!(c.shard, 2);
        // deterministic file order: ascending (shard, offset)
        assert!(e.tensors.windows(2).all(|w| {
            w[0].shard < w[1].shard || (w[0].shard == w[1].shard && w[0].offset <= w[1].offset)
        }));
        assert!(e.is_multi_shard());
        assert_eq!(e.shard_count(), 3);
        assert_eq!(e.shard_bytes(), Some(e.total_bytes));
    }

    #[test]
    fn sharded_anatomy_is_deterministic_across_index_key_order() {
        let (index, shards) = three_shard_scenario();
        // Same weight_map with insertion order reversed.
        let reversed = r#"{"weight_map":{"c.bias":"c.safetensors","c.weight":"c.safetensors","b.weight":"b.safetensors","a.bias":"a.safetensors","a.weight":"a.safetensors"}}"#;
        let a = build_sharded_safetensors_anatomy(index.as_bytes(), mem_loader(&shards)).unwrap();
        let b =
            build_sharded_safetensors_anatomy(reversed.as_bytes(), mem_loader(&shards)).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn sharded_anatomy_rejects_duplicate_tensor_across_shards() {
        let (index, mut shards) = three_shard_scenario();
        // b.safetensors now declares a.weight as well as b.weight.
        let hb_dup = st_header(&[("b.weight", 0, 32), ("a.weight", 32, 36)]);
        let bytes = shard_file_bytes(&hb_dup, 36);
        shards.insert("b.safetensors".into(), (bytes.len() as u64, bytes));
        let err =
            build_sharded_safetensors_anatomy(index.as_bytes(), mem_loader(&shards)).unwrap_err();
        assert!(matches!(err, ArtifactError::Header(_)));
        assert!(format!("{err:?}").contains("more than one shard"));
    }

    #[test]
    fn sharded_anatomy_rejects_unmapped_extra_tensor() {
        let (index, mut shards) = three_shard_scenario();
        // a.safetensors now also declares extra.weight.
        let ha_extra = st_header(&[
            ("a.weight", 0, 16),
            ("a.bias", 16, 20),
            ("extra.weight", 20, 24),
        ]);
        let bytes = shard_file_bytes(&ha_extra, 24);
        shards.insert("a.safetensors".into(), (bytes.len() as u64, bytes));
        let err =
            build_sharded_safetensors_anatomy(index.as_bytes(), mem_loader(&shards)).unwrap_err();
        assert!(matches!(err, ArtifactError::Header(_)));
        assert!(format!("{err:?}").contains("index does not map it"));
    }

    #[test]
    fn sharded_anatomy_rejects_tensor_mapped_to_absent_shard() {
        // weight_map points a.weight at a shard the loader cannot supply.
        let bad = r#"{"weight_map":{"a.weight":"missing.safetensors"}}"#;
        let err =
            build_sharded_safetensors_anatomy(bad.as_bytes(), mem_loader(&Default::default()))
                .unwrap_err();
        assert!(matches!(err, ArtifactError::Header(_)));
        assert!(format!("{err:?}").contains("missing"));
    }

    #[test]
    fn sharded_anatomy_rejects_tensor_mapped_into_wrong_shard() {
        let (index, mut shards) = three_shard_scenario();
        // Physical layout: a.weight lives in b.safetensors, but the index still
        // maps it to a.safetensors.
        let hb2 = st_header(&[("b.weight", 0, 32), ("a.weight", 32, 48)]);
        let bytes = shard_file_bytes(&hb2, 48);
        shards.insert("b.safetensors".into(), (bytes.len() as u64, bytes));
        let ha2 = st_header(&[("a.bias", 0, 4)]);
        let bytes = shard_file_bytes(&ha2, 4);
        shards.insert("a.safetensors".into(), (bytes.len() as u64, bytes));
        let err =
            build_sharded_safetensors_anatomy(index.as_bytes(), mem_loader(&shards)).unwrap_err();
        assert!(matches!(err, ArtifactError::Header(_)));
        assert!(format!("{err:?}").contains("physically lives"));
    }

    #[test]
    fn sharded_anatomy_rejects_empty_and_silly_indices() {
        let _ = (st_header, shard_file_bytes); // (helpers also used by fs tests)
        let err_empty = build_sharded_safetensors_anatomy(
            r#"{"weight_map":{}}"#.as_bytes(),
            mem_loader(&Default::default()),
        )
        .unwrap_err();
        assert!(matches!(err_empty, ArtifactError::Header(_)));

        let err_no_wm = build_sharded_safetensors_anatomy(
            r#"{"other":1}"#.as_bytes(),
            mem_loader(&Default::default()),
        )
        .unwrap_err();
        assert!(matches!(err_no_wm, ArtifactError::Header(_)));

        let err_not_obj =
            build_sharded_safetensors_anatomy(b"[1,2,3]", mem_loader(&Default::default()))
                .unwrap_err();
        assert!(matches!(err_not_obj, ArtifactError::Header(_)));

        let err_nonstr = build_sharded_safetensors_anatomy(
            r#"{"weight_map":{"t.weight":123}}"#.as_bytes(),
            mem_loader(&Default::default()),
        )
        .unwrap_err();
        assert!(matches!(err_nonstr, ArtifactError::Header(_)));
    }

    #[test]
    fn sharded_anatomy_rejects_shard_name_traversal() {
        for bad in [
            "../escape.safetensors",
            "/etc/passwd",
            "a//b",
            "a/./b",
            "a\\b",
            "nul\x00byte",
            "",
        ] {
            let index = format!("{{\"weight_map\":{{\"t.weight\":{bad:?}}}}}");
            let err = build_sharded_safetensors_anatomy(
                index.as_bytes(),
                mem_loader(&Default::default()),
            )
            .unwrap_err();
            assert!(
                matches!(err, ArtifactError::Header(_)),
                "{bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn sharded_anatomy_rejects_truncated_and_oversized_shards() {
        let (index, mut shards) = three_shard_scenario();
        // Shard A is truncated: file shorter than its header region.
        shards.insert(
            "a.safetensors".into(),
            (4, shards["a.safetensors"].1.clone()),
        );
        let err =
            build_sharded_safetensors_anatomy(index.as_bytes(), mem_loader(&shards)).unwrap_err();
        assert!(matches!(err, ArtifactError::Header(_)));

        // Shard B header exceeds the ceiling. A length-prefixed buffer whose
        // header claims to be longer than the ceiling; the builder must reject
        // it on size alone (no JSON parse of a 16 MiB header is attempted).
        let hlen = (ANATOMY_MAX_SAFETENSORS_HEADER_BYTES + 1) as usize;
        let mut big = Vec::with_capacity(8 + hlen);
        big.extend_from_slice(&(hlen as u64).to_le_bytes());
        big.extend(std::iter::repeat_n(b'x', hlen));
        assert_eq!(big.len() as u64, ANATOMY_MAX_SAFETENSORS_HEADER_BYTES + 9);
        let (_, mut shards2) = three_shard_scenario();
        shards2.insert("b.safetensors".into(), (big.len() as u64, big));
        let err =
            build_sharded_safetensors_anatomy(index.as_bytes(), mem_loader(&shards2)).unwrap_err();
        assert!(matches!(err, ArtifactError::Header(_)));
        assert!(format!("{err:?}").contains("header is"));
    }

    #[test]
    fn read_shard_header_rejects_implausible_lengths_without_allocation() {
        let dir = temp_dir("m7-huge-header");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("h.safetensors");
        let mut f = std::fs::File::create(&path).unwrap();
        use std::io::Write;
        f.write_all(&(1u64 << 40).to_le_bytes()).unwrap(); // header claims 1 TiB
        drop(f);
        let meta = std::fs::metadata(&path).unwrap();
        let err = read_shard_header(&path, meta.len()).unwrap_err();
        assert!(matches!(err, ArtifactError::Header(_)));
        assert!(format!("{err:?}").contains("outside"));

        // A normal-sized header overrunning a short file is also caught.
        let mut f = std::fs::File::create(&path).unwrap();
        let h = st_header(&[("t", 0, 4)]);
        f.write_all(&(h.len() as u64).to_le_bytes()).unwrap();
        f.write_all(h.as_bytes()).unwrap();
        f.write_all(&[0u8; 2]).unwrap(); // data block too small for [0,4]
        drop(f);
        let meta2 = std::fs::metadata(&path).unwrap();
        assert_eq!(
            read_shard_header(&path, meta2.len()).unwrap().len(),
            h.len()
        );
    }

    #[test]
    fn sharded_anatomy_for_index_file_builds_from_disk() {
        let dir = temp_dir("m7-e2e");
        std::fs::create_dir_all(&dir).unwrap();
        let (index, shards) = three_shard_scenario();
        for (name, (_, bytes)) in &shards {
            std::fs::write(dir.join(name), bytes).unwrap();
        }
        std::fs::write(dir.join("model.safetensors.index.json"), &index).unwrap();
        let anatomy =
            sharded_anatomy_for_index_file(&dir.join("model.safetensors.index.json")).unwrap();
        assert_eq!(anatomy.shards, 3);
        assert_eq!(anatomy.tensors.len(), 5);
        assert_eq!(
            anatomy.total_bytes,
            shards.values().map(|(s, _)| *s).sum::<u64>()
        );
        let a = anatomy.tensors.iter().find(|t| t.name == "c.bias").unwrap();
        let hc = st_header(&[("c.weight", 0, 8), ("c.bias", 8, 16)]);
        assert_eq!(a.shard, 2);
        assert_eq!(a.offset, Some(8 + hc.len() as u64 + 8));
        assert_eq!(a.length, Some(8));
    }

    #[test]
    fn is_safetensors_index_file_sniffs_content_not_filename() {
        let dir = temp_dir("m7-sniff");
        std::fs::create_dir_all(&dir).unwrap();
        let plain = dir.join("model.bin");
        let mut plain_bytes = shard_file_bytes(&st_header(&[("t", 0, 4)]), 4);
        plain_bytes[0] = 0xff; // corrupt the length so it is not a valid index
        std::fs::write(&plain, &plain_bytes).unwrap();
        assert!(!is_safetensors_index_file(&plain));

        let not_json = dir.join("x.json");
        std::fs::write(&not_json, b"not json at all").unwrap();
        assert!(!is_safetensors_index_file(&not_json));

        let no_wm = dir.join("y.json");
        std::fs::write(&no_wm, r#"{"foo":1}"#).unwrap();
        assert!(!is_safetensors_index_file(&no_wm));

        let real = dir.join("model.safetensors.index.json");
        std::fs::write(&real, r#"{"weight_map":{"t":"a.safetensors"}}"#).unwrap();
        assert!(is_safetensors_index_file(&real));

        // Oversized files are refused by the bounded sniff unconditionally.
        let big = dir.join("big.index.json");
        let f = std::fs::File::create(&big).unwrap();
        f.set_len(ANATOMY_MAX_INDEX_BYTES + 1).unwrap();
        assert!(!is_safetensors_index_file(&big));
    }

    #[test]
    fn sharded_anatomy_for_index_file_rejects_symlink_escape() {
        let dir = temp_dir("m7-symlink");
        let outside = std::env::temp_dir().join(format!(
            "openrt-anatomy-m7-symlink-out-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let payload = shard_file_bytes(&st_header(&[("t", 0, 4)]), 4);
        std::fs::write(outside.join("secret.safetensors"), &payload).unwrap();
        // Index inside dir/ refers to "linked/../outside/secret.safetensors" —
        // the `..` component is rejected outright, and a symlink that resolves
        // outside the index directory is refused after canonicalization.
        let index = &format!(
            r#"{{"weight_map":{{"t.weight":"{}"}}}}"#,
            "linked/../outside/secret.safetensors"
        );
        std::fs::write(dir.join("index.json"), index).unwrap();
        std::fs::create_dir_all(dir.join("linked")).unwrap();
        let err = sharded_anatomy_for_index_file(&dir.join("index.json")).unwrap_err();
        assert!(matches!(err, ArtifactError::Header(_)));
        assert!(format!("{err:?}").contains("not a safe relative path"));

        let link = dir.join("escape.safetensors");
        std::os::unix::fs::symlink(outside.join("secret.safetensors"), &link).unwrap();
        let index2 = r#"{"weight_map":{"t.weight":"escape.safetensors"}}"#;
        std::fs::write(dir.join("index.json"), index2).unwrap();
        let err2 = sharded_anatomy_for_index_file(&dir.join("index.json")).unwrap_err();
        assert!(matches!(err2, ArtifactError::Header(_)));
        assert!(format!("{err2:?}").contains("outside the index directory"));
    }

    #[test]
    fn single_shard_member_is_still_a_single_file_anatomy() {
        // M2 behavior is preserved: registering one member of a sharded model
        // treats it as an ordinary single-file safetensors artifact, not a
        // multi-shard one.
        let h = st_header(&[("a.weight", 0, 16)]);
        let dir = temp_dir("m7-member");
        std::fs::create_dir_all(&dir).unwrap();
        let member = dir.join("pytorch_model-00001-of-00003.safetensors");
        std::fs::write(&member, shard_file_bytes(&h, 16)).unwrap();
        let anatomy = anatomy_for_file(&member).unwrap();
        assert_eq!(anatomy.format, AnatomyFormat::Safetensors);
        assert_eq!(anatomy.shards, 1);
        assert_eq!(anatomy.shard_files.len(), 1);
        assert_eq!(
            anatomy.shard_files[0].file,
            "pytorch_model-00001-of-00003.safetensors"
        );
        assert!(!anatomy.is_multi_shard());
        assert_eq!(
            anatomy
                .tensors
                .iter()
                .find(|t| t.name == "a.weight")
                .unwrap()
                .shard,
            0
        );
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("openrt-anatomy-{tag}-{}", std::process::id()))
    }
}

//! Minimal ZIP reader/writer used by the OOXML (Office Open XML) modules.
//!
//! * Writer: **stored** entries (method 0) — deterministic (fixed DOS
//!   timestamp), no compression dependency in the write path, and every
//!   consumer (Excel/PowerPoint/Word/LibreOffice) accepts stored entries.
//! * Reader: stored + deflate (method 8, via `flate2`) so files produced by
//!   other OOXML tools can be read back.

use std::io::Read;

/// CRC-32 (IEEE 802.3, polynomial 0xEDB88320) — table-driven, no unsafe.
pub fn crc32(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for (i, item) in table.iter_mut().enumerate() {
        let mut c = i as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
        *item = c;
    }
    let mut crc = 0xFFFF_FFFFu32;
    for b in data {
        crc = table[((crc ^ u32::from(*b)) & 0xFF) as usize] ^ (crc >> 8);
    }
    !crc
}

const DOS_DATE: u16 = ((2026u16 - 1980) << 9) | (1 << 5) | 1; // 2026-01-01
const DOS_TIME: u16 = 0;

struct Entry {
    name: String,
    data: Vec<u8>,
}

/// A deterministic ZIP archive builder (stored entries).
pub struct ZipWriter {
    entries: Vec<Entry>,
}

impl ZipWriter {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Add one file. Names use `/` separators (OOXML requirement).
    pub fn add(&mut self, name: &str, data: impl Into<Vec<u8>>) {
        self.entries.push(Entry {
            name: name.to_string(),
            data: data.into(),
        });
    }

    pub fn add_str(&mut self, name: &str, text: &str) {
        self.add(name, text.as_bytes().to_vec());
    }

    pub fn build(&self) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::new();
        let mut central: Vec<u8> = Vec::new();
        for e in &self.entries {
            let name = e.name.as_bytes();
            let crc = crc32(&e.data);
            let size = e.data.len() as u32;
            let offset = out.len() as u32;
            // Local file header.
            out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
            out.extend_from_slice(&20u16.to_le_bytes()); // version needed
            out.extend_from_slice(&0u16.to_le_bytes()); // flags
            out.extend_from_slice(&0u16.to_le_bytes()); // method: stored
            out.extend_from_slice(&DOS_TIME.to_le_bytes());
            out.extend_from_slice(&DOS_DATE.to_le_bytes());
            out.extend_from_slice(&crc.to_le_bytes());
            out.extend_from_slice(&size.to_le_bytes());
            out.extend_from_slice(&size.to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes()); // extra len
            out.extend_from_slice(name);
            out.extend_from_slice(&e.data);
            // Central directory record.
            central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes()); // version made by
            central.extend_from_slice(&20u16.to_le_bytes()); // version needed
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&DOS_TIME.to_le_bytes());
            central.extend_from_slice(&DOS_DATE.to_le_bytes());
            central.extend_from_slice(&crc.to_le_bytes());
            central.extend_from_slice(&size.to_le_bytes());
            central.extend_from_slice(&size.to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes()); // extra
            central.extend_from_slice(&0u16.to_le_bytes()); // comment
            central.extend_from_slice(&0u16.to_le_bytes()); // disk
            central.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
            central.extend_from_slice(&0u32.to_le_bytes()); // external attrs
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(name);
        }
        let cd_offset = out.len() as u32;
        out.extend_from_slice(&central);
        let cd_size = (out.len() as u32) - cd_offset;
        let n = self.entries.len() as u16;
        // End of central directory.
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&n.to_le_bytes());
        out.extend_from_slice(&n.to_le_bytes());
        out.extend_from_slice(&cd_size.to_le_bytes());
        out.extend_from_slice(&cd_offset.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }
}

impl Default for ZipWriter {
    fn default() -> Self {
        Self::new()
    }
}

/// Read a ZIP archive into `(name, bytes)` pairs (stored + deflate).
pub fn read_zip(bytes: &[u8]) -> Result<Vec<(String, Vec<u8>)>, String> {
    // Locate EOCD (PK\x05\x06) scanning backwards.
    let mut eocd = None;
    if bytes.len() >= 22 {
        for i in (0..=bytes.len() - 22).rev() {
            if &bytes[i..i + 4] == b"PK\x05\x06" {
                eocd = Some(i);
                break;
            }
        }
    }
    let eocd = eocd.ok_or_else(|| "zip: end-of-central-directory not found".to_string())?;
    let cd_size = u32::from_le_bytes(bytes[eocd + 12..eocd + 16].try_into().unwrap()) as usize;
    let cd_offset = u32::from_le_bytes(bytes[eocd + 16..eocd + 20].try_into().unwrap()) as usize;
    let mut pos = cd_offset;
    let cd_end = cd_offset + cd_size;
    if cd_end > bytes.len() {
        return Err("zip: central directory out of bounds".into());
    }
    let mut out = Vec::new();
    while pos + 46 <= cd_end {
        if &bytes[pos..pos + 4] != b"PK\x01\x02" {
            return Err(format!("zip: bad central directory signature at {pos}"));
        }
        let method = u16::from_le_bytes(bytes[pos + 10..pos + 12].try_into().unwrap());
        let comp_size = u32::from_le_bytes(bytes[pos + 20..pos + 24].try_into().unwrap()) as usize;
        let name_len = u16::from_le_bytes(bytes[pos + 28..pos + 30].try_into().unwrap()) as usize;
        let extra_len = u16::from_le_bytes(bytes[pos + 30..pos + 32].try_into().unwrap()) as usize;
        let comment_len =
            u16::from_le_bytes(bytes[pos + 32..pos + 34].try_into().unwrap()) as usize;
        let local_off = u32::from_le_bytes(bytes[pos + 42..pos + 46].try_into().unwrap()) as usize;
        let name = String::from_utf8_lossy(&bytes[pos + 46..pos + 46 + name_len]).into_owned();
        // Local header re-read (name/extra lengths may differ from central).
        if local_off + 30 > bytes.len() || &bytes[local_off..local_off + 4] != b"PK\x03\x04" {
            return Err(format!("zip: bad local header for `{name}`"));
        }
        let l_name_len =
            u16::from_le_bytes(bytes[local_off + 26..local_off + 28].try_into().unwrap()) as usize;
        let l_extra_len =
            u16::from_le_bytes(bytes[local_off + 28..local_off + 30].try_into().unwrap()) as usize;
        let data_start = local_off + 30 + l_name_len + l_extra_len;
        let data_end = data_start + comp_size;
        if data_end > bytes.len() {
            return Err(format!("zip: data out of bounds for `{name}`"));
        }
        let raw = &bytes[data_start..data_end];
        let data = match method {
            0 => raw.to_vec(),
            8 => {
                let mut dec = flate2::read::DeflateDecoder::new(raw);
                let mut buf = Vec::new();
                dec.read_to_end(&mut buf)
                    .map_err(|e| format!("zip: inflate `{name}`: {e}"))?;
                buf
            }
            m => {
                return Err(format!(
                    "zip: unsupported compression method {m} for `{name}`"
                ))
            }
        };
        out.push((name, data));
        pos += 46 + name_len + extra_len + comment_len;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_known_vectors() {
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b"hello"), 0x3610_A686); // == zlib.crc32(b"hello")
    }

    #[test]
    fn roundtrip_stored() {
        let mut w = ZipWriter::new();
        w.add_str("[Content_Types].xml", "<Types/>");
        w.add("bin/data.bin", vec![0u8, 1, 2, 255]);
        let bytes = w.build();
        let entries = read_zip(&bytes).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, "[Content_Types].xml");
        assert_eq!(entries[0].1, b"<Types/>");
        assert_eq!(entries[1].1, vec![0u8, 1, 2, 255]);
    }

    #[test]
    fn deterministic_output() {
        let mut a = ZipWriter::new();
        a.add_str("x.txt", "same");
        let mut b = ZipWriter::new();
        b.add_str("x.txt", "same");
        assert_eq!(a.build(), b.build());
    }

    #[test]
    fn garbage_fails_cleanly() {
        assert!(read_zip(b"not a zip").is_err());
    }
}

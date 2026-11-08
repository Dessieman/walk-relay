//! A journey travels as one zip: its `.sav`, its `.sav.bak` (if any) and a manifest.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{Cursor, Read, Write};

pub const MANIFEST: &str = "walkrelay-manifest.json";

/// File name -> contents, for the files of one journey.
pub type Files = BTreeMap<String, Vec<u8>>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    pub format: u32,
    pub journey: String,
    pub uploaded_by: String,
    /// When this copy was uploaded (ms since epoch).
    pub uploaded_at: u64,
    /// When the game last wrote the save (newest file mtime, ms since epoch).
    pub saved_at: u64,
    pub content_hash: String,
}

/// Hash of a journey's files, independent of who uploaded them or when.
pub fn content_hash(files: &Files) -> String {
    let mut h = Sha256::new();
    for (name, data) in files {
        h.update((name.len() as u64).to_le_bytes());
        h.update(name.as_bytes());
        h.update((data.len() as u64).to_le_bytes());
        h.update(data);
    }
    hex::encode(h.finalize())
}

pub fn pack(manifest: &Manifest, files: &Files) -> Result<Vec<u8>> {
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    zip.start_file(MANIFEST, opts)?;
    zip.write_all(&serde_json::to_vec_pretty(manifest)?)?;
    for (name, data) in files {
        zip.start_file(name.as_str(), opts)?;
        zip.write_all(data)?;
    }
    Ok(zip.finish()?.into_inner())
}

pub fn unpack(bytes: &[u8]) -> Result<(Manifest, Files)> {
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).context("journey bundle is not a valid zip")?;
    let mut manifest: Option<Manifest> = None;
    let mut files = Files::new();
    for i in 0..zip.len() {
        let mut f = zip.by_index(i)?;
        let name = f.name().to_string();
        let mut data = Vec::with_capacity(f.size() as usize);
        f.read_to_end(&mut data)?;
        if name == MANIFEST {
            manifest = Some(serde_json::from_slice(&data).context("bad manifest")?);
        } else {
            // Only ever plain journey file names; never paths.
            if name.contains('/') || name.contains('\\') || name.contains("..") || crate::paths::journey_stem(&name).is_none() {
                bail!("unexpected file {name:?} in journey bundle");
            }
            files.insert(name, data);
        }
    }
    let manifest = manifest.context("journey bundle has no manifest")?;
    if content_hash(&files) != manifest.content_hash {
        bail!("journey bundle is corrupted (hash mismatch)");
    }
    for name in files.keys() {
        if crate::paths::journey_stem(name) != Some(manifest.journey.as_str()) {
            bail!("file {name:?} does not belong to journey {:?}", manifest.journey);
        }
    }
    Ok((manifest, files))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let mut files = Files::new();
        files.insert("save_x_0123456789abcdef.sav".into(), b"hello".to_vec());
        files.insert("save_x_0123456789abcdef.sav.bak".into(), b"hell".to_vec());
        let m = Manifest {
            format: 1,
            journey: "save_x_0123456789abcdef".into(),
            uploaded_by: "chris".into(),
            uploaded_at: 1,
            saved_at: 2,
            content_hash: content_hash(&files),
        };
        let bytes = pack(&m, &files).unwrap();
        let (m2, f2) = unpack(&bytes).unwrap();
        assert_eq!(m, m2);
        assert_eq!(files, f2);
    }

    #[test]
    fn rejects_tampered_hash() {
        let mut files = Files::new();
        files.insert("save_x_0123456789abcdef.sav".into(), b"hello".to_vec());
        let m = Manifest {
            format: 1,
            journey: "save_x_0123456789abcdef".into(),
            uploaded_by: "chris".into(),
            uploaded_at: 1,
            saved_at: 2,
            content_hash: "nope".into(),
        };
        assert!(unpack(&pack(&m, &files).unwrap()).is_err());
    }
}

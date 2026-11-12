//! First-use downloads for Kokoro TTS: the ONNX Runtime shared library for
//! this OS/arch and the Kokoro model + voices. Every file lands as
//! `<name>.part`, is verified (size, and sha256 where pinned), then renamed,
//! so an interrupted download never looks complete.

use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow, bail};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

pub struct Asset {
    pub url: &'static str,
    pub size: u64,
    pub sha256: Option<&'static str>,
}

pub const ORT_VERSION: &str = "1.28.2";

pub const MODEL: Asset = Asset {
    url: "https://github.com/thewh1teagle/kokoro-onnx/releases/download/model-files-v1.0/kokoro-v1.0.onnx",
    size: 325_532_387,
    sha256: Some("7d5df8ecf7d4b1878015a32686053fd0eebe2bc377234608764cc0ef3636a6c5"),
};
pub const VOICES: Asset = Asset {
    url: "https://github.com/thewh1teagle/kokoro-onnx/releases/download/model-files-v1.0/voices-v1.0.bin",
    size: 28_214_398,
    sha256: Some("bca610b8308e8d99f32e6fe4197e7ec01679264efed0cac9140fe9c29f1fbf7d"),
};

/// The misaki-rs crate archive from crates.io (immutable; the sha256 is its
/// `Cargo.lock` checksum). Its lexicons and POS-tagger data, ~37 MB of JSON,
/// are what the vendored G2P (`vendor/misaki-rs`) reads at runtime; they
/// used to be compiled into the binary.
pub const MISAKI_VERSION: &str = "0.6.0";
pub const MISAKI: Asset = Asset {
    url: "https://static.crates.io/crates/misaki-rs/misaki-rs-0.6.0.crate",
    size: 8_966_728,
    sha256: Some("e868d9d070425960dc6c40c84caeaa4e6ebfabb1af454c8f4b9fbe6dbc01ff04"),
};

/// (path inside [`MISAKI`], file name the G2P reads it under).
pub const MISAKI_FILES: [(&str, &str); 7] = {
    use misaki_rs::data::*;
    [
        ("misaki-rs-0.6.0/data/us_gold.json", US_GOLD),
        ("misaki-rs-0.6.0/data/us_silver.json", US_SILVER),
        ("misaki-rs-0.6.0/data/gb_gold.json", GB_GOLD),
        ("misaki-rs-0.6.0/data/gb_silver.json", GB_SILVER),
        (
            "misaki-rs-0.6.0/src/resources/tagger/weights.json",
            TAGGER_WEIGHTS,
        ),
        (
            "misaki-rs-0.6.0/src/resources/tagger/classes.txt",
            TAGGER_CLASSES,
        ),
        (
            "misaki-rs-0.6.0/src/resources/tagger/tags.json",
            TAGGER_TAGS,
        ),
    ]
};

/// The official ONNX Runtime CPU release for this host: archive, the
/// library's file name inside it, and the name it is stored under.
pub struct OrtAsset {
    pub archive: Asset,
    pub entry: &'static str,
    pub lib_name: &'static str,
}

macro_rules! ort_url {
    ($f:literal) => {
        concat!(
            "https://github.com/microsoft/onnxruntime/releases/download/v1.28.2/",
            $f
        )
    };
}

pub fn ort_asset() -> Option<OrtAsset> {
    let (url, size, sha, entry, lib_name) = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => (
            ort_url!("onnxruntime-linux-x64-1.28.2.tgz"),
            9_128_991,
            "d7209b8751b27b862b0c76332c2e20e203396edb5dab700ecf4bb485cf147415",
            "libonnxruntime.so.1.28.2",
            "libonnxruntime.so",
        ),
        ("linux", "aarch64") => (
            ort_url!("onnxruntime-linux-aarch64-1.28.2.tgz"),
            8_119_456,
            "f020b3d31106cc7db03889b4a5c21e7c38ce4a09ad26119c11d1ad6d3fa0ec04",
            "libonnxruntime.so.1.28.2",
            "libonnxruntime.so",
        ),
        ("macos", "aarch64") => (
            ort_url!("onnxruntime-osx-arm64-1.28.2.tgz"),
            32_093_967,
            "c4fceacfc53765d0869dc9180c31ec91054d149017a99d1e80ffe28dc79596de",
            "libonnxruntime.1.28.2.dylib",
            "libonnxruntime.dylib",
        ),
        ("windows", "x86_64") => (
            ort_url!("onnxruntime-win-x64-1.28.2.zip"),
            78_620_837,
            "c4eedd29489d5feca21866d054638416f3655bf6b18851b3b6b85c8313e95c35",
            "onnxruntime.dll",
            "onnxruntime.dll",
        ),
        ("windows", "aarch64") => (
            ort_url!("onnxruntime-win-arm64-1.28.2.zip"),
            79_713_523,
            "a3ab2265e52d157ef1c4f4f82f66fc582ce12a780510aac240690ce194b58510",
            "onnxruntime.dll",
            "onnxruntime.dll",
        ),
        // ONNX Runtime no longer publishes macOS x86_64 builds.
        _ => return None,
    };
    Some(OrtAsset {
        archive: Asset {
            url,
            size,
            sha256: Some(sha),
        },
        entry,
        lib_name,
    })
}

fn part_path(dest: &Path) -> PathBuf {
    let mut p = dest.as_os_str().to_owned();
    p.push(".part");
    PathBuf::from(p)
}

/// Stream `asset` to `dest` via `dest.part`, reporting bytes as they land.
pub async fn fetch(
    client: &reqwest::Client,
    asset: &Asset,
    dest: &Path,
    on_bytes: &(dyn Fn(u64) + Send + Sync),
) -> anyhow::Result<()> {
    let part = part_path(dest);
    let mut resp = client
        .get(asset.url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .with_context(|| format!("GET {}", asset.url))?;
    let mut file = tokio::fs::File::create(&part).await?;
    let mut hasher = Sha256::new();
    let mut len = 0u64;
    while let Some(chunk) = resp.chunk().await? {
        hasher.update(&chunk);
        file.write_all(&chunk).await?;
        len += chunk.len() as u64;
        on_bytes(chunk.len() as u64);
    }
    file.flush().await?;
    drop(file);
    let verify = || -> anyhow::Result<()> {
        if len != asset.size {
            bail!("{}: expected {} bytes, got {len}", asset.url, asset.size);
        }
        if let Some(want) = asset.sha256 {
            let got = hex::encode(hasher.finalize());
            if got != want {
                bail!("{}: sha256 mismatch ({got})", asset.url);
            }
        }
        Ok(())
    };
    if let Err(e) = verify() {
        let _ = tokio::fs::remove_file(&part).await;
        return Err(e);
    }
    tokio::fs::rename(&part, dest).await?;
    Ok(())
}

/// Pull the ONNX Runtime library out of a downloaded release archive.
pub fn extract_lib(archive: &Path, entry: &str, dest: &Path) -> anyhow::Result<()> {
    let part = part_path(dest);
    let file = std::fs::File::open(archive)?;
    let is_zip = archive.extension().is_some_and(|e| e == "zip");
    let mut out = std::fs::File::create(&part)?;
    let found = if is_zip {
        let mut zip = zip::ZipArchive::new(file)?;
        let name = (0..zip.len())
            .filter_map(|i| zip.name_for_index(i).map(str::to_string))
            .find(|n| n.ends_with(&format!("/lib/{entry}")));
        match name {
            Some(n) => {
                std::io::copy(&mut zip.by_name(&n)?, &mut out)?;
                true
            }
            None => false,
        }
    } else {
        let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file));
        let mut found = false;
        for e in tar.entries()? {
            let mut e = e?;
            let is_match = e.header().entry_type().is_file()
                && e.path()?.file_name().is_some_and(|f| f == entry);
            if is_match {
                std::io::copy(&mut e, &mut out)?;
                found = true;
                break;
            }
        }
        found
    };
    drop(out);
    if !found {
        let _ = std::fs::remove_file(&part);
        return Err(anyhow!("{entry} not found in {}", archive.display()));
    }
    std::fs::rename(&part, dest)?;
    Ok(())
}

/// Extract `files` — `(path inside the archive, name to store it under)` —
/// from a `.tar.gz` into `dest_dir`, each via `<name>.part`. Fails, naming
/// the missing entries, unless every one was found.
pub fn extract_files(
    archive: &Path,
    files: &[(&str, &str)],
    dest_dir: &Path,
) -> anyhow::Result<()> {
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(std::fs::File::open(archive)?));
    let mut found = vec![false; files.len()];
    for e in tar.entries()? {
        let mut e = e?;
        if !e.header().entry_type().is_file() {
            continue;
        }
        let path = e.path()?.to_string_lossy().into_owned();
        let Some(i) = files.iter().position(|(entry, _)| *entry == path) else {
            continue;
        };
        let dest = dest_dir.join(files[i].1);
        let part = part_path(&dest);
        std::io::copy(&mut e, &mut std::fs::File::create(&part)?)?;
        std::fs::rename(&part, &dest)?;
        found[i] = true;
    }
    let missing: Vec<&str> = files
        .iter()
        .zip(&found)
        .filter(|(_, f)| !**f)
        .map(|((entry, _), _)| *entry)
        .collect();
    if !missing.is_empty() {
        bail!("{} missing from {}", missing.join(", "), archive.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tgz(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        let mut tar = tar::Builder::new(gz);
        for (path, data) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            tar.append_data(&mut h, path, *data).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn extracts_named_files_and_reports_missing_ones() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("a.crate");
        std::fs::write(
            &archive,
            tgz(&[("pkg/data/a.json", b"{}"), ("pkg/other.txt", b"x")]),
        )
        .unwrap();
        let out = dir.path().join("out");
        std::fs::create_dir(&out).unwrap();
        extract_files(&archive, &[("pkg/data/a.json", "a.json")], &out).unwrap();
        assert_eq!(std::fs::read(out.join("a.json")).unwrap(), b"{}");
        let err = extract_files(&archive, &[("pkg/data/b.json", "b.json")], &out).unwrap_err();
        assert!(err.to_string().contains("pkg/data/b.json"), "{err}");
    }

    /// Every file the vendored G2P reads has an archive entry.
    #[test]
    fn misaki_files_cover_every_data_file() {
        for name in misaki_rs::data::ALL_FILES {
            assert!(MISAKI_FILES.iter().any(|(_, n)| *n == name), "{name}");
        }
    }
}

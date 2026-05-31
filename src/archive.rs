use anyhow::{anyhow, Context, Result};
use log::{debug, info};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crate::t;

/// Determine if a file is an archive and extract it.
///
/// - If `extract_path` is specified, only that file is extracted.
/// - Otherwise, the first file found (or only file) is returned.
/// - If the file is not an archive, it is returned as-is (bare binary).
pub fn extract_if_archive(
    file_path: &Path,
    output_dir: &Path,
    extract_path: &Option<String>,
) -> Result<PathBuf> {
    let file_name = file_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");

    if file_name.ends_with(".zip") {
        extract_zip(file_path, output_dir, extract_path)
    } else if file_name.ends_with(".tar.gz") || file_name.ends_with(".tgz") {
        extract_tar_gz(file_path, output_dir, extract_path)
    } else if file_name.ends_with(".tar.xz") || file_name.ends_with(".txz") {
        extract_tar_xz(file_path, output_dir, extract_path)
    } else {
        info!(
            "{}: {}",
            t!("File is not an archive, using as-is", "非存档文件, 直接使用"),
            file_path.display()
        );
        Ok(file_path.to_path_buf())
    }
}

fn extract_zip(
    zip_path: &Path,
    output_dir: &Path,
    extract_path: &Option<String>,
) -> Result<PathBuf> {
    info!("{}: {}", t!("Extracting ZIP", "解压 ZIP"), zip_path.display());
    let file = fs::File::open(zip_path).context("failed to open zip file")?;
    let mut archive = zip::ZipArchive::new(file).context("failed to read zip archive")?;

    if let Some(target) = extract_path {
        let mut entry = archive
            .by_name(target)
            .map_err(|e| anyhow!("file '{}' not found in zip: {}", target, e))?;

        let out_path = output_dir.join(
            Path::new(target)
                .file_name()
                .unwrap_or(std::ffi::OsStr::new(target)),
        );
        let mut out_file =
            fs::File::create(&out_path).context("failed to create extracted file")?;
        io::copy(&mut entry, &mut out_file).context("failed to extract file from zip")?;
        info!("{}: {} -> {}", t!("Extracted", "已提取"), target, out_path.display());
        Ok(out_path)
    } else {
        let mut found_path: Option<PathBuf> = None;

        for i in 0..archive.len() {
            let mut entry = archive.by_index(i)?;
            let name = entry.name().to_string();

            if entry.is_dir() {
                continue;
            }

            debug!("ZIP entry: {}", name);

            let out_name = Path::new(&name)
                .file_name()
                .unwrap_or(std::ffi::OsStr::new(&name))
                .to_os_string();
            let out_path = output_dir.join(&out_name);
            let mut out_file =
                fs::File::create(&out_path).context("failed to create extracted file")?;
            io::copy(&mut entry, &mut out_file)?;

            if found_path.is_none() {
                found_path = Some(out_path.clone());
            }

            info!("{}: {} -> {}", t!("Extracted", "已提取"), name, out_path.display());
        }

        found_path.ok_or_else(|| anyhow!("zip archive is empty"))
    }
}

fn extract_tar_gz(
    tar_path: &Path,
    output_dir: &Path,
    extract_path: &Option<String>,
) -> Result<PathBuf> {
    info!("{}: {}", t!("Extracting tar.gz", "解压 tar.gz"), tar_path.display());
    let file = fs::File::open(tar_path).context("failed to open tar.gz file")?;
    let decompressor = flate2::read::GzDecoder::new(file);
    extract_tar(decompressor, output_dir, extract_path)
}

fn extract_tar_xz(
    tar_path: &Path,
    output_dir: &Path,
    extract_path: &Option<String>,
) -> Result<PathBuf> {
    info!("{}: {}", t!("Extracting tar.xz", "解压 tar.xz"), tar_path.display());
    let file = fs::File::open(tar_path).context("failed to open tar.xz file")?;
    let decompressor = xz2::read::XzDecoder::new(file);
    extract_tar(decompressor, output_dir, extract_path)
}

fn extract_tar<R: Read>(
    reader: R,
    output_dir: &Path,
    extract_path: &Option<String>,
) -> Result<PathBuf> {
    let mut archive = tar::Archive::new(reader);
    let mut found_path: Option<PathBuf> = None;

    for entry_result in archive.entries()? {
        let mut entry = entry_result?;
        let path = entry.path()?.to_path_buf();
        let path_str = path.to_string_lossy().to_string();

        if entry.header().entry_type().is_dir() {
            continue;
        }

        debug!("TAR entry: {}", path_str);

        if let Some(target) = extract_path {
            if path_str == *target || path_str.ends_with(&format!("/{}", target)) {
                let out_name = Path::new(target)
                    .file_name()
                    .unwrap_or(std::ffi::OsStr::new(target));
                let out_path = output_dir.join(out_name);
                let mut out_file =
                    fs::File::create(&out_path).context("failed to create extracted file")?;
                io::copy(&mut entry, &mut out_file)?;
                info!("{}: {} -> {}", t!("Extracted", "已提取"), path_str, out_path.display());
                return Ok(out_path);
            }
        } else {
            let out_name = path
                .file_name()
                .unwrap_or(std::ffi::OsStr::new(&path_str));
            let out_path = output_dir.join(out_name);
            let mut out_file =
                fs::File::create(&out_path).context("failed to create extracted file")?;
            io::copy(&mut entry, &mut out_file)?;
            info!("{}: {} -> {}", t!("Extracted", "已提取"), path_str, out_path.display());

            if found_path.is_none() {
                found_path = Some(out_path);
            }
        }
    }

    if let Some(target) = extract_path {
        return Err(anyhow!("file '{}' not found in tar archive", target));
    }

    found_path.ok_or_else(|| anyhow!("tar archive is empty"))
}

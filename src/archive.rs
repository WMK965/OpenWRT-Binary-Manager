// archive.rs ── 归档解压模块
//
// 负责识别并解压下载的存档文件，支持以下格式：
// - .zip        ：ZIP 归档
// - .tar.gz/.tgz：gzip 压缩的 tar 归档
// - .tar.xz/.txz：xz 压缩的 tar 归档
// - .7z         ：7z 归档（LZMA/LZMA2）
// - .gz         ：单独的 gzip 文件（非 tar.gz）
// - 其他        ：视为裸二进制文件，直接使用
//
// 支持通过 extract_path 指定存档内要提取的目标文件路径。

use anyhow::{anyhow, Context, Result};
use log::{debug, info};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crate::t;

/// 替换路径中的 `{tag}` 和 `{version}` 占位符。
///
/// - `{tag}`     -> 完整的 release tag（如 `v1.13.12`）
/// - `{version}` -> 去除前导 `v`/`V` 的版本号（如 `1.13.12`）
///
/// 例如 `sing-box-{version}-linux-amd64/sing-box` + tag `v1.2.3`
///   -> `sing-box-1.2.3-linux-amd64/sing-box`
pub fn resolve_extract_path(path: &str, tag: &str) -> String {
    let version = tag.trim_start_matches('v').trim_start_matches('V');
    path.replace("{tag}", tag)
        .replace("{version}", version)
}

/// 判断文件是否为归档并执行解压。
///
/// - 若指定了 `extract_path`，则只提取该文件；
/// - 否则提取第一个文件（或唯一文件）作为结果返回；
/// - 若文件不是归档，则原样返回其路径（视为裸二进制）。
///
/// 返回值为最终提取出的二进制文件路径。
pub fn extract_if_archive(
    file_path: &Path,
    output_dir: &Path,
    extract_path: &Option<String>,
) -> Result<PathBuf> {
    // 根据文件扩展名判断归档类型
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
    } else if file_name.ends_with(".7z") {
        extract_7z(file_path, output_dir, extract_path)
    } else if file_name.ends_with(".gz") {
        // 单独的 .gz 文件（非 tar.gz），直接解压为单个文件
        decompress_gz(file_path, output_dir)
    } else {
        // 非归档文件：直接作为裸二进制使用
        info!(
            "{}: {}",
            t!("File is not an archive, using as-is", "非存档文件, 直接使用"),
            file_path.display()
        );
        Ok(file_path.to_path_buf())
    }
}

/// 解压 ZIP 归档
///
/// - 若指定 `extract_path`：按名称查找并仅提取该文件
/// - 否则：提取所有文件，返回第一个被提取的文件路径
fn extract_zip(
    zip_path: &Path,
    output_dir: &Path,
    extract_path: &Option<String>,
) -> Result<PathBuf> {
    info!("{}: {}", t!("Extracting ZIP", "解压 ZIP"), zip_path.display());
    let file = fs::File::open(zip_path).context("failed to open zip file")?;
    let mut archive = zip::ZipArchive::new(file).context("failed to read zip archive")?;

    if let Some(target) = extract_path {
        // 按名称精确查找目标文件
        let mut entry = archive
            .by_name(target)
            .map_err(|e| anyhow!("file '{}' not found in zip: {}", target, e))?;

        // 输出文件名取 target 的 basename（去除目录部分）
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
        // 未指定目标文件：提取所有非目录条目
        let mut found_path: Option<PathBuf> = None;

        for i in 0..archive.len() {
            let mut entry = archive.by_index(i)?;
            let name = entry.name().to_string();

            // 跳过目录条目
            if entry.is_dir() {
                continue;
            }

            debug!("ZIP entry: {}", name);

            // 输出文件名取条目的 basename，避免重建目录结构
            let out_name = Path::new(&name)
                .file_name()
                .unwrap_or(std::ffi::OsStr::new(&name))
                .to_os_string();
            let out_path = output_dir.join(&out_name);
            let mut out_file =
                fs::File::create(&out_path).context("failed to create extracted file")?;
            io::copy(&mut entry, &mut out_file)?;

            // 记录第一个提取的文件作为返回值
            if found_path.is_none() {
                found_path = Some(out_path.clone());
            }

            info!("{}: {} -> {}", t!("Extracted", "已提取"), name, out_path.display());
        }

        found_path.ok_or_else(|| anyhow!("zip archive is empty"))
    }
}

/// 解压 .tar.gz 归档
///
/// 先用 gzip 解码器包装文件流，再交给通用的 tar 解压函数。
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

/// 解压 .tar.xz 归档
///
/// 先用 xz 解码器包装文件流，再交给通用的 tar 解压函数。
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

/// 通用的 tar 归档解压函数（支持任意实现了 Read 的解码器）
///
/// - 若指定 `extract_path`：仅提取路径匹配的条目（精确匹配或以 `/{target}` 结尾）
/// - 否则：提取所有文件，返回第一个被提取的文件路径
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

        // 跳过目录条目
        if entry.header().entry_type().is_dir() {
            continue;
        }

        debug!("TAR entry: {}", path_str);

        if let Some(target) = extract_path {
            // 匹配规则：完整路径相等，或以 "/{target}" 结尾（兼容 tar 内含子目录的情况）
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
            // 未指定目标文件：提取所有条目
            let out_name = path
                .file_name()
                .unwrap_or(std::ffi::OsStr::new(&path_str));
            let out_path = output_dir.join(out_name);
            let mut out_file =
                fs::File::create(&out_path).context("failed to create extracted file")?;
            io::copy(&mut entry, &mut out_file)?;
            info!("{}: {} -> {}", t!("Extracted", "已提取"), path_str, out_path.display());

            // 记录第一个提取的文件作为返回值
            if found_path.is_none() {
                found_path = Some(out_path);
            }
        }
    }

    // 若指定了目标文件但未找到，返回错误
    if let Some(target) = extract_path {
        return Err(anyhow!("file '{}' not found in tar archive", target));
    }

    found_path.ok_or_else(|| anyhow!("tar archive is empty"))
}

/// 解压 7z 归档
///
/// - 若指定 `extract_path`：仅提取路径匹配的条目（精确匹配或以 `/{target}` 结尾）
/// - 否则：全量解压，返回第一个被提取的文件路径
fn extract_7z(
    sevenz_path: &Path,
    output_dir: &Path,
    extract_path: &Option<String>,
) -> Result<PathBuf> {
    info!("{}: {}", t!("Extracting 7z", "解压 7z"), sevenz_path.display());

    if let Some(target) = extract_path {
        // 有 extract_path：使用自定义提取函数，仅提取匹配的条目
        let out_name = Path::new(target)
            .file_name()
            .unwrap_or(std::ffi::OsStr::new(target));
        let out_path = output_dir.join(out_name);

        let target_owned = target.clone();
        let out_path_owned = out_path.clone();

        sevenz_rust2::decompress_file_with_extract_fn(
            sevenz_path,
            output_dir,
            move |entry, reader, _dest| {
                let name = entry.name();
                // 匹配规则与 tar 一致：完整路径相等，或以 "/{target}" 结尾
                if name == target_owned || name.ends_with(&format!("/{}", target_owned)) {
                    let mut out_file = fs::File::create(&out_path_owned)?;
                    io::copy(reader, &mut out_file)?;
                    info!(
                        "{}: {} -> {}",
                        t!("Extracted", "已提取"),
                        name,
                        out_path_owned.display()
                    );
                }
                // 返回 false 跳过默认提取逻辑（由闭包自行处理写入）
                Ok(false)
            },
        )
        .context("failed to extract 7z archive")?;

        if !out_path.exists() {
            return Err(anyhow!("file '{}' not found in 7z archive", target));
        }
        Ok(out_path)
    } else {
        // 无 extract_path：全量解压，扫描返回第一个文件
        sevenz_rust2::decompress_file(sevenz_path, output_dir)
            .context("failed to extract 7z archive")?;

        // 在解压目录中查找第一个文件
        let mut found_path: Option<PathBuf> = None;
        find_first_file(output_dir, &mut found_path)?;

        found_path.ok_or_else(|| anyhow!("7z archive is empty"))
    }
}

/// 递归查找目录中的第一个文件（用于 7z 全量解压后获取结果路径）
fn find_first_file(dir: &Path, found: &mut Option<PathBuf>) -> Result<()> {
    if found.is_some() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            find_first_file(&path, found)?;
        } else {
            debug!("7z entry: {}", path.display());
            *found = Some(path);
            return Ok(());
        }
    }
    Ok(())
}

/// 解压单独的 .gz 文件（非 tar.gz）
///
/// gzip 文件解压后即为单个文件，输出文件名取原文件名去除 .gz 后缀。
fn decompress_gz(gz_path: &Path, output_dir: &Path) -> Result<PathBuf> {
    info!("{}: {}", t!("Decompressing gz", "解压 gz"), gz_path.display());
    let input = fs::File::open(gz_path).context("failed to open gz file")?;
    let mut decoder = flate2::read::GzDecoder::new(input);

    // 输出文件名 = 原文件名去掉 .gz 扩展名
    let stem = gz_path
        .file_stem()
        .and_then(|n| n.to_str())
        .unwrap_or("decompressed");
    let out_path = output_dir.join(stem);
    let mut out_file = fs::File::create(&out_path).context("failed to create decompressed file")?;
    io::copy(&mut decoder, &mut out_file).context("failed to decompress gz")?;
    info!("{}: {} -> {}", t!("Decompressed", "已解压"), gz_path.display(), out_path.display());
    Ok(out_path)
}

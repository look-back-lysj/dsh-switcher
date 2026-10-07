//! 导出/导入单文件备份包。
//!
//! 目标：把明文仓库压成一个 `.dshvault.zip`，方便移动硬盘/U 盘转移。
//! v1 不做加密；仓库本身包含敏感对话，界面必须持续提醒用户不要上传云盘。

use crate::repo::load_manifest;
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use zip::write::SimpleFileOptions;
use zip::ZipWriter;

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportResult {
    pub file: String,
    pub entries: u32,
    pub bytes: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportResult {
    pub repo: String,
    pub entries: u32,
    pub verified: bool,
}

pub fn export_repo(repo: &Path, target: &Path) -> Result<ExportResult, String> {
    let manifest = load_manifest(repo)?;
    if target.exists() {
        return Err(format!("目标文件已存在：{}", target.display()));
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("创建导出目录失败：{e}"))?;
    }

    let file = File::create(target).map_err(|e| format!("创建导出文件失败：{e}"))?;
    let mut zip = ZipWriter::new(file);
    let options = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .large_file(true);

    let mut entries = 0u32;
    for item in &manifest.files {
        let source = repo.join(&item.path);
        let bytes = fs::read(&source).map_err(|e| format!("读取仓库文件失败：{e}"))?;
        zip.start_file(item.path.as_str(), options)
            .map_err(|e| format!("写入压缩包索引失败：{e}"))?;
        zip.write_all(&bytes).map_err(|e| format!("写入压缩包失败：{e}"))?;
        entries += 1;
    }
    let manifest_text = fs::read_to_string(repo.join("manifest.json"))
        .map_err(|e| format!("读取仓库清单失败：{e}"))?;
    zip.start_file("manifest.json", options).map_err(|e| e.to_string())?;
    zip.write_all(manifest_text.as_bytes()).map_err(|e| e.to_string())?;
    entries += 1;
    zip.finish().map_err(|e| format!("完成压缩包失败：{e}"))?;

    Ok(ExportResult {
        file: target.to_string_lossy().to_string(),
        entries,
        bytes: fs::metadata(target).map_err(|e| format!("读取导出文件大小失败：{e}"))?.len(),
    })
}

pub fn import_archive(archive: &Path, repo: &Path) -> Result<ImportResult, String> {
    if repo.join("manifest.json").exists() {
        return Err(format!("目标仓库已存在：{}", repo.display()));
    }
    fs::create_dir_all(repo).map_err(|e| format!("创建仓库目录失败：{e}"))?;
    let file = File::open(archive).map_err(|e| format!("打开备份包失败：{e}"))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("备份包不是有效 zip：{e}"))?;

    let mut entries = 0u32;
    for index in 0..archive.len() {
        let mut item = archive.by_index(index).map_err(|e| format!("读取备份包条目失败：{e}"))?;
        if item.is_dir() {
            continue;
        }
        let Some(name) = item.enclosed_name() else {
            return Err("备份包内存在不安全路径".to_string());
        };
        let display_name = name.to_string_lossy().to_string();
        let target = repo.join(name);
        if !target.starts_with(repo) {
            return Err(format!("备份包路径越界：{display_name}"));
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("创建仓库目录失败：{e}"))?;
        }
        let mut bytes = Vec::new();
        item.read_to_end(&mut bytes).map_err(|e| format!("解压失败：{e}"))?;
        crate::repo::atomic_write(&target, &bytes)?;
        entries += 1;
    }

    let verified = crate::restore::verify_repo(repo)
        .map(|v| v.ok == v.total && v.bad.is_empty() && v.missing.is_empty())
        .unwrap_or(false);
    if !verified {
        return Err("导入后校验未通过，请勿使用该仓库".to_string());
    }
    Ok(ImportResult {
        repo: repo.to_string_lossy().to_string(),
        entries,
        verified,
    })
}

#[allow(dead_code)]
pub fn temp_path_hint() -> PathBuf {
    std::env::temp_dir().join("dsh-vault-export.zip")
}


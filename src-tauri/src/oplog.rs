//! 操作日志：把每一次关键操作（扫描/备份/恢复/接管/断开/切换/修复/导出/撤回）追加到本地 jsonl，
//! 供用户一键导出 markdown 排障报告。
//!
//! 红线（同学排查报告强调）：
//! - 绝不记录文件内容、不记录 API Key、不记录凭据明文；
//! - 只记操作类型、涉及的环境 id/标签/路径、文件数、字节数、成功/失败、错误摘要；
//! - 写盘用「追加 + 单行 JSON」，崩溃时最多损失最后一行，不影响历史；
//! - 日志文件放 %APPDATA%/com.dsh.vault/oplog.jsonl，与扫描缓存同目录，易找。

use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

pub const OPLOG_VERSION: u32 = 1;
/// 单文件超过这个大小就滚动到 .1，防止无限膨胀。
const MAX_BYTES: u64 = 4 * 1024 * 1024;
/// 导出报告最多带多少条（防止超大报告）。
const EXPORT_LIMIT: usize = 5000;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpEntry {
    /// 本地时间（可读）
    pub time: String,
    /// 操作类型：scan / backup / restore / adopt / unadopt / switch / repair / export / import / undo / verify
    pub op: String,
    /// 仓库路径（可为空）
    #[serde(default)]
    pub repo: String,
    /// 相关环境标签（如 "官方版 → AIO"），不记路径以外的敏感内容
    #[serde(default)]
    pub subject: String,
    /// 关键数字摘要（如 "文件 123 / 45.2 MB / 新建 100 跳过 23"）
    #[serde(default)]
    pub detail: String,
    /// "ok" | "fail" | "warn"
    pub status: String,
    /// 错误摘要（不含堆栈、不含敏感内容）
    #[serde(default)]
    pub error: String,
}

fn log_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("com.dsh.vault")
}

fn log_path() -> PathBuf {
    log_dir().join("oplog.jsonl")
}

fn now_string() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

/// 追加一条操作日志。失败不阻断业务（日志是辅助，不能反过来影响主流程）。
pub fn record(op: &str, repo: &str, subject: &str, detail: &str, status: &str, error: &str) {
    let _ = record_inner(op, repo, subject, detail, status, error);
}

fn record_inner(op: &str, repo: &str, subject: &str, detail: &str, status: &str, error: &str) -> Result<(), String> {
    let dir = log_dir();
    fs::create_dir_all(&dir).map_err(|e| format!("创建日志目录失败：{e}"))?;
    let path = log_path();
    // 滚动：超过上限把旧文件改名 .1，重新开始
    if let Ok(meta) = fs::metadata(&path) {
        if meta.len() > MAX_BYTES {
            let _ = fs::rename(&path, dir.join("oplog.jsonl.1"));
        }
    }
    let entry = OpEntry {
        time: now_string(),
        op: op.to_string(),
        repo: repo.to_string(),
        subject: subject.to_string(),
        detail: detail.to_string(),
        status: status.to_string(),
        error: error.to_string(),
    };
    let mut line = serde_json::to_string(&entry).map_err(|e| format!("序列化日志失败：{e}"))?;
    line.push('\n');
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("打开日志失败：{e}"))?;
    file.write_all(line.as_bytes())
        .map_err(|e| format!("写日志失败：{e}"))?;
    Ok(())
}

/// 读取全部日志（新的在前）。
pub fn read_all() -> Vec<OpEntry> {
    let mut out = Vec::new();
    for name in ["oplog.jsonl.1", "oplog.jsonl"] {
        let path = log_dir().join(name);
        if let Ok(text) = fs::read_to_string(&path) {
            for line in text.lines() {
                if let Ok(entry) = serde_json::from_str::<OpEntry>(line) {
                    out.push(entry);
                }
            }
        }
    }
    out.reverse();
    out
}

/// 清空日志。
pub fn clear() {
    let _ = fs::remove_file(log_path());
    let _ = fs::remove_file(log_dir().join("oplog.jsonl.1"));
}

/// 日志文件当前大小（字节），前端显示用。
pub fn size_bytes() -> u64 {
    let mut total = 0u64;
    for name in ["oplog.jsonl", "oplog.jsonl.1"] {
        if let Ok(meta) = fs::metadata(log_dir().join(name)) {
            total += meta.len();
        }
    }
    total
}

/// 导出 markdown 排障报告。返回写出的文件路径。
pub fn export_markdown(target: &std::path::Path) -> Result<String, String> {
    let entries = read_all();
    let limited: Vec<&OpEntry> = entries.iter().take(EXPORT_LIMIT).collect();
    let mut md = String::new();
    md.push_str("# DSH Vault 操作日志报告\n\n");
    md.push_str(&format!("- 导出时间：{}\n", now_string()));
    md.push_str(&format!("- 电脑：{}\n", std::env::var("COMPUTERNAME").unwrap_or_else(|_| "unknown".into())));
    md.push_str(&format!("- 条数：{}（日志共 {} 条）\n\n", limited.len(), entries.len()));
    md.push_str("> 本报告只记录操作类型、环境与数量摘要，不包含任何 API Key 或凭据内容。\n\n");
    md.push_str("| 时间 | 操作 | 环境 | 结果 | 摘要 | 错误 |\n");
    md.push_str("|---|---|---|---|---|---|\n");
    for e in limited {
        let status = match e.status.as_str() {
            "ok" => "成功",
            "fail" => "失败",
            _ => "警告",
        };
        let esc = |s: &str| s.replace('|', "\\|").replace('\n', " ");
        md.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} |\n",
            e.time,
            esc(&e.op),
            esc(&e.subject),
            status,
            esc(&e.detail),
            esc(&e.error)
        ));
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("创建导出目录失败：{e}"))?;
    }
    fs::write(target, md).map_err(|e| format!("写报告失败：{e}"))?;
    Ok(target.to_string_lossy().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_and_read_roundtrip() {
        // 注意：读断言只针对本测试写入的条目（按 subject 匹配），
        // 不假设日志为空——真实环境里日志可能已有历史记录。
        let marker = format!("test-marker-{}", std::process::id());
        record("backup", "D:\\\\repo", &marker, "文件 10 / 1 KB", "ok", "");
        record("switch", "D:\\\\repo", &marker, "sessions skills", "fail", "目标被占用");
        let all = read_all();
        let mine: Vec<&OpEntry> = all.iter().filter(|e| e.subject == marker).collect();
        assert_eq!(mine.len(), 2, "应读回本测试写入的 2 条");
        // 新的在前
        assert_eq!(mine[0].op, "switch");
        assert_eq!(mine[0].status, "fail");
        assert_eq!(mine[1].op, "backup");
    }

    #[test]
    fn export_writes_markdown() {
        clear();
        record("restore", "D:\\\\repo", "AIO", "新建 5", "ok", "");
        let tmp = std::env::temp_dir().join("dsh-vault-oplog-test.md");
        let _ = fs::remove_file(&tmp);
        let path = export_markdown(&tmp).expect("导出应成功");
        let text = fs::read_to_string(&path).expect("报告应可读");
        assert!(text.contains("DSH Vault 操作日志报告"));
        assert!(text.contains("restore"));
        assert!(text.contains("不包含任何 API Key"));
        let _ = fs::remove_file(&tmp);
        clear();
    }
}

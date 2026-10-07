//! 常量与共享数据结构。
//!
//! 备份边界来自本机 3 个 DSH Home 的实测：
//! - junction / node_modules / eac-market / 壁纸缓存会膨胀到数百 MB；
//! - `.anonymous-user-id` 是机器身份，跨机恢复可能污染遥测身份；
//! - profile 只备份根层声明文件，因为插件本体可由 DSH 首启重装。

use serde::{Deserialize, Serialize};

pub const VAULT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// v1：Node 原型仓库；v2：Rust/Tauri 仓库。读取端兼容两者，写入端生成 v2。
pub const REPO_VERSION: u32 = 2;
pub const REPO_COMPAT_VERSIONS: &[u32] = &[1, 2];
pub const ZSTD_MAGIC: u32 = 0xFD2FB528;
pub const SKIP_DIRS: &[&str] = &[
    "node_modules",
    "eac-market",
    "webui-deliverables",
    "logs",
    "Cache",
    "GPUCache",
    "blob_storage",
    ".plugin-manager",
    "tmp-cleaner",
    "cache",
    "tmp",
];

pub const RECURSE_DIRS: &[&str] = &[
    "sessions",
    "skills",
    ".agent-presets",
    "memories",
    "team",
    "guard",
    "rollbacks",
    "undo-snapshots",
    "attachments",
];

pub const SINGLE_FILES: &[&str] = &[
    "settings.yaml",
    "settings.yaml.imported",
    ".credentials.yaml",
    ".env",
    ".dshw-usage.json",
    ".dshw-size.json",
];

pub const STORAGES_FILES: &[&str] = &["usage-stats-cache.json"];

pub const PROFILE_FILES: &[&str] = &[
    "package.json",
    "cordis.patch.yml",
    "cordis.yml",
    "pnpm-workspace.yaml",
    ".dsh-builtin-plugins.json",
    ".dsh-profile-compatibility.json",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum HomeKind {
    DshHome,
    AgentsHome,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStats {
    pub v0: u32,
    pub v4: u32,
    pub total: u32,
    #[serde(default)]
    pub ok: u32,
    #[serde(default)]
    pub truncated: u32,
    #[serde(default)]
    pub corrupt: u32,
    #[serde(default)]
    pub subagent: u32,
    #[serde(default, alias = "doubleGen")]
    pub double_generation: u32,
}

impl Default for SessionStats {
    fn default() -> Self {
        Self {
            v0: 0,
            v4: 0,
            total: 0,
            ok: 0,
            truncated: 0,
            corrupt: 0,
            subagent: 0,
            double_generation: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredHome {
    pub id: String,
    pub kind: HomeKind,
    pub label: String,
    pub path: String,
    pub variant: String,
    pub dsh_version: Option<String>,
    pub sessions: SessionStats,
    pub backup_file_count: u64,
    pub backup_size: u64,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct ScanReport {
    pub homes: Vec<DiscoveredHome>,
    pub excluded: Vec<ExcludedPath>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct ExcludedPath {
    pub path: String,
    pub reason: String,
    pub size: u64,
}







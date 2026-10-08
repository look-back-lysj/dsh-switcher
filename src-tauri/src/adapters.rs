//! 可配置适配规则表（adapters.json）：把"版本差异"从硬编码改成数据驱动。
//!
//! 范式核心：新出一种封装版，只需往规则表加一条，不用改代码、不用重编译。
//! 规则表放仓库根（adapters.json），Vault 启动时加载；文件不存在时用内置默认。
//!
//! 覆盖三类版本差异：
//!   1. 预设映射：anchored-standard / router-standard → standard（命中官方白名单才放行）。
//!   2. 提供方检测：迁移后列出"目标端缺哪些 provider、需重录哪些 Key（只列名）"。
//!   3. 格式代提示：v3 → "打开时自动升 v4"；未知代警告但不阻断。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Adapters {
    /// 官方认识的预设白名单（asar 注册表实证：standard/code/ptc/ask/architect）
    pub known_presets: Vec<String>,
    /// 预设重写落点（默认 standard）
    pub fallback_preset: String,
    /// 预设映射表：非法预设 → 目标预设（未列出的非法预设统一落到 fallback_preset）
    pub preset_map: HashMap<String, String>,
    /// 各格式代的迁移提示（代 → 提示文案）；未知代用 unknown_generation_note
    pub generation_notes: HashMap<String, String>,
    pub unknown_generation_note: String,
}

impl Default for Adapters {
    fn default() -> Self {
        let mut preset_map = HashMap::new();
        // 社区版实验性预设（本机 .agent-presets 实证）→ 官方 standard
        for p in ["anchored-standard", "router-standard", "router-spec",
                      "zero-anchored-standard", "whoami-standard", "warmupbetter",
                      "warmupbetter-replay", "minimal-win", "minimal-gitbash",
                      "v4-flash-godmode-opencode-go"] {
            preset_map.insert(p.to_string(), "standard".to_string());
        }
        let mut generation_notes = HashMap::new();
        generation_notes.insert("0".into(), "旧版格式（v0）。打开时 DSH 会自动升级到当前格式。".into());
        generation_notes.insert("3".into(), "v3 格式。官方版打开时会自动升级到 v4（已证实有迁移边）。".into());
        generation_notes.insert("4".into(), "当前格式（v4），可直接使用。".into());
        Self {
            known_presets: vec!["standard".into(), "code".into(), "ptc".into(), "ask".into(), "architect".into()],
            fallback_preset: "standard".into(),
            preset_map,
            generation_notes,
            unknown_generation_note: "未知格式代，已按原样放置，目标版本打开时会尝试自动识别。".into(),
        }
    }
}

impl Adapters {
    /// 从仓库根加载 adapters.json；文件缺失/损坏时回退内置默认（不报错，保证可用）。
    pub fn load(repo: &Path) -> Self {
        let file = repo.join("adapters.json");
        let Ok(text) = fs::read_to_string(&file) else {
            return Self::default();
        };
        match serde_json::from_str::<Adapters>(&text) {
            Ok(mut a) => {
                // 用户规则与内置默认合并：内置白名单/落点保底，用户的 preset_map 覆盖追加
                let defaults = Self::default();
                if a.known_presets.is_empty() { a.known_presets = defaults.known_presets.clone(); }
                if a.fallback_preset.is_empty() { a.fallback_preset = defaults.fallback_preset.clone(); }
                for (k, v) in defaults.preset_map { a.preset_map.entry(k).or_insert(v); }
                for (k, v) in defaults.generation_notes { a.generation_notes.entry(k).or_insert(v); }
                if a.unknown_generation_note.is_empty() { a.unknown_generation_note = defaults.unknown_generation_note; }
                a
            }
            Err(_) => Self::default(),
        }
    }

    /// 预设是否官方认识。
    pub fn is_known_preset(&self, preset: &str) -> bool {
        self.known_presets.iter().any(|p| p == preset)
    }

    /// 把任意预设映射到目标端认识的值。已认识的保持不变；表里有映射用映射；否则落 fallback。
    pub fn map_preset(&self, preset: &str) -> String {
        if self.is_known_preset(preset) {
            return preset.to_string();
        }
        self.preset_map.get(preset).cloned().unwrap_or_else(|| self.fallback_preset.clone())
    }

    /// 某格式代的提示文案。
    pub fn generation_note(&self, generation: u32) -> String {
        self.generation_notes
            .get(&generation.to_string())
            .cloned()
            .unwrap_or_else(|| self.unknown_generation_note.clone())
    }

    /// 把当前生效规则写回仓库（供"查看/导出规则"或首次初始化）。
    pub fn save(&self, repo: &Path) -> Result<(), String> {
        let file = repo.join("adapters.json");
        let text = serde_json::to_string_pretty(self).map_err(|e| format!("序列化失败：{e}"))?;
        fs::write(&file, text).map_err(|e| format!("写入失败：{e}"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_maps_community_presets() {
        let a = Adapters::default();
        assert_eq!(a.map_preset("anchored-standard"), "standard");
        assert_eq!(a.map_preset("router-standard"), "standard");
        assert_eq!(a.map_preset("standard"), "standard", "官方预设保持原样");
        assert_eq!(a.map_preset("某个未来新预设"), "standard", "未列出的落 fallback");
    }

    #[test]
    fn load_merges_user_rules_with_defaults() {
        let dir = std::env::temp_dir().join(format!("dsh-adapt-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        // 用户加一条自定义映射 + 一条自定义白名单预设
        fs::write(dir.join("adapters.json"), r#"{
            "presetMap": { "my-custom-preset": "code" },
            "knownPresets": ["standard", "code", "special-one"]
        }"#).unwrap();
        let a = Adapters::load(&dir);
        assert_eq!(a.map_preset("my-custom-preset"), "code", "用户映射生效");
        assert_eq!(a.map_preset("anchored-standard"), "standard", "内置映射仍在");
        assert!(a.is_known_preset("special-one"), "用户白名单生效");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_missing_or_corrupt_falls_back() {
        let dir = std::env::temp_dir().join(format!("dsh-adapt2-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        // 缺失
        assert_eq!(Adapters::load(&dir).fallback_preset, "standard");
        // 损坏
        fs::write(dir.join("adapters.json"), b"{ not json").unwrap();
        assert_eq!(Adapters::load(&dir).fallback_preset, "standard");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn generation_notes() {
        let a = Adapters::default();
        assert!(a.generation_note(3).contains("v4"));
        assert!(a.generation_note(99).contains("未知格式代"));
    }
}

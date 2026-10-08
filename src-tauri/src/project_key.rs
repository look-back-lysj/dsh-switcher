//! 官方 projectKey / encodeSegment 算法的 Rust 精确复刻。
//!
//! 来源：官方版 app.asar 内嵌源码（packages/session/session-persistence-jsonl/format.ts），
//! 逐字移植，保证 DSH Vault 生成的 sessions/ 目录名与 DSH 自己生成的完全一致，
//! session.list 读到的才是完好日志（社区插件 eightfs 的硬约束：命名必须与官方锁步）。
//!
//! 官方 JS 原文（asar 实证）：
//!   function encodeSegment(s){ return encodeURIComponent(s).replace(/%3A/gi, ":"); }
//!   function projectKey(cwd){
//!     if (cwd.length === 0) throw new Error("cannot encode an empty project path");
//!     let readable = "", separatorRun = false;
//!     for (let i = 0; i < cwd.length; i++) {
//!       const code = cwd.charCodeAt(i), ch = String.fromCharCode(code);
//!       if (ch === "/" || ch === "\\" || ch === ":") { if (!separatorRun) readable += "-"; separatorRun = true; }
//!       else if (ch !== "~" && /^[A-Za-z0-9._-]$/.test(ch)) { readable += ch; separatorRun = false; }
//!       else { readable += "~" + code.toString(16).toUpperCase().padStart(4, "0"); separatorRun = false; }
//!     }
//!     return `--${(readable.replace(/^-+/, "") || "root").slice(0, 251)}--`;
//!   }
//!
//! 注意：JS 的 charCodeAt 是 UTF-16 码元。中文等 BMP 字符恰好等于其 Unicode 标量值，
//! 与 Rust char as u32 一致；但超出 BMP 的字符（如部分 emoji）在 JS 里是代理对两个码元。
//! 为严格对齐，这里按 UTF-16 码元迭代，与 JS 行为逐字节一致。

/// 把一段路径按官方 encodeSegment 编码：encodeURIComponent 后把 %3A 还原为 :。
/// 等价于：每个字符按 UTF-16 码元处理，保留 RFC3986 未保留字符 + 还原冒号。
fn encode_segment(segment: &str) -> String {
    let mut out = String::new();
    for unit in segment.encode_utf16() {
        // encodeURIComponent 的未保留字符集：A-Z a-z 0-9 - _ . ! ~ * ' ( )
        let c = unit as u32;
        let keep = matches!(c,
            0x41..=0x5A | 0x61..=0x7A | 0x30..=0x39 // A-Z a-z 0-9
        ) || matches!(unit, 0x2D | 0x5F | 0x2E | 0x21 | 0x7E | 0x2A | 0x27 | 0x28 | 0x29); // - _ . ! ~ * ' ( )
        if keep {
            // 安全：这些都是 ASCII
            out.push(char::from_u32(c).unwrap());
        } else {
            // 逐字节百分号编码（UTF-8 字节），冒号除外
            let ch = char::from_u32(c);
            if let Some(ch) = ch {
                if ch == ':' {
                    out.push(':');
                    continue;
                }
                let mut buf = [0u8; 4];
                for b in ch.encode_utf8(&mut buf).as_bytes() {
                    out.push('%');
                    out.push_str(&format!("{:02X}", b));
                }
            } else {
                // 代理对的一半（极端情况）：按码元值 %uXXXX 兜底（JS 不会到这里，因为它逐字节）
                out.push_str(&format!("%{:02X}%{:02X}", (unit >> 8) as u8, (unit & 0xFF) as u8));
            }
        }
    }
    out
}

/// 官方 encodePath：按 / 分段后逐段 encodeSegment，再用 / 拼回。
#[allow(dead_code)]
pub fn encode_path(path: &str) -> String {
    path.split('/').map(encode_segment).collect::<Vec<_>>().join("/")
}

/// 官方 projectKey：把 cwd 编码成 `--...--` 的单一安全目录名。
/// 规则：/ \ : 折叠为单个 -；~ 与 [A-Za-z0-9._-] 以外的字符变 ~XXXX（UTF-16 码元大写十六进制）；
/// 去掉前导 -，空则 "root"，截到 251 字符，外层包 --。
pub fn project_key(cwd: &str) -> String {
    assert!(!cwd.is_empty(), "cannot encode an empty project path");
    let mut readable = String::new();
    let mut separator_run = false;
    for unit in cwd.encode_utf16() {
        let ch = char::from_u32(unit as u32);
        let is_sep = matches!(ch, Some('/') | Some('\\') | Some(':'));
        if is_sep {
            if !separator_run {
                readable.push('-');
            }
            separator_run = true;
        } else {
            let is_safe = matches!(ch, Some(c) if c != '~' && (c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-'));
            if is_safe {
                readable.push(ch.unwrap());
            } else {
                readable.push('~');
                readable.push_str(&format!("{:04X}", unit));
            }
            separator_run = false;
        }
    }
    let trimmed = readable.trim_start_matches('-');
    let base = if trimmed.is_empty() { "root" } else { trimmed };
    let sliced: String = base.chars().take(251).collect();
    format!("--{}--", sliced)
}

/// 会话目录名（sessionDir）：官方是 projectKey(cwd)/session-<id> 形态。
/// DSH 的会话 id 在磁盘目录名上直接用（session-<uuid> 或无前缀 uuid，实测两种都有）。
pub fn session_dir_name(cwd: &str, session_id: &str) -> String {
    format!("{}/{}", project_key(cwd), session_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_key_ascii_path() {
        // C:\Users\liu\proj → --C-Users-liu-proj--
        assert_eq!(project_key("C:\\Users\\liu\\proj"), "--C-Users-liu-proj--");
        assert_eq!(project_key("E:/VS"), "--E-VS--");
    }

    #[test]
    fn project_key_chinese_and_tilde() {
        // 中文按 UTF-16 码元转 ~XXXX 大写：'刘' U+5218 → ~5218；'~' 也要转义
        let k = project_key("C:\\Users\\刘沛伦\\Desktop");
        assert!(k.starts_with("--C-Users-"), "前缀: {}", k);
        assert!(k.contains("~5218"), "刘→~5218: {}", k);
        assert!(k.ends_with("--"), "后缀: {}", k);
        // '~' 本身也要编码
        let k2 = project_key("E:\\a~b");
        assert!(k2.contains("~007E"), "~→~007E: {}", k2);
    }

    #[test]
    fn project_key_edge_cases() {
        // 空路径 panic
        let r = std::panic::catch_unwind(|| project_key(""));
        assert!(r.is_err());
        // 全分隔符 → root
        assert_eq!(project_key("\\\\"), "--root--");
        // 前导分隔符折叠且不留下前导 -
        assert_eq!(project_key("\\a"), "--a--");
    }

    /// 与官方 asar 内嵌 JS 原文（oracle.js 用 node 跑出）逐字节对拍。
    #[test]
    fn matches_official_oracle() {
        let cases: &[(&str, &str)] = &[
            ("C:\\Users\\刘沛伦\\Desktop\\找矿材料整理", "--C-Users-~5218~6C9B~4F26-Desktop-~627E~77FF~6750~6599~6574~7406--"),
            ("E:\\VS", "--E-VS--"),
            ("E:/wu", "--E-wu--"),
            ("C:\\Users\\liu\\proj", "--C-Users-liu-proj--"),
            ("E:\\a~b", "--E-a~007Eb--"),
            ("D:\\my proj\\test (1)", "--D-my~0020proj-test~0020~00281~0029--"),
            ("\\\\", "--root--"),
            ("C:\\", "--C---"),
        ];
        for (input, expected) in cases {
            assert_eq!(&project_key(input), expected, "input={}", input);
        }
    }

    #[test]
    fn encode_segment_keeps_colon_and_encodes_chinese() {
        assert_eq!(encode_segment("C:"), "C:");
        assert_eq!(encode_segment("abc"), "abc");
        // 中文 → %E5%88%98（UTF-8 百分号编码，大写）
        let s = encode_segment("刘");
        assert!(s.starts_with('%'), "中文应百分号编码: {}", s);
    }
}

# DSH 存储结构与智能扫描算法设计

## 一、DSH 各版本存储结构（实测确认）

### 官方版 / EAC 桌面版（共享 Home）
- **路径**：`~/.dsh`
- **特征**：sessions/、profiles/、skills/、settings.yaml、.credentials.yaml
- **版本检测**：profiles/*/package.json 中的 @deepseek-ai/dsh 依赖版本

### AIO 桌面壳（独立 Home）
- **路径**：`%APPDATA%/com.deepseek.dsh.desktop.aio/dsh-home`
- **特征**：sessions/、profiles/、skills/、.agent-presets/、memories/、team/、guard/、rollbacks/、storages/
- **比官方多**：memories/、team/、tmp-cleaner/
- **版本检测**：%APPDATA%/com.deepseek.dsh.desktop.aio/bundle-verified.json

### v4Lite（独立 Home）
- **路径**：`~/.dsh-v4lite`
- **特征**：sessions/、profiles/、.agent-presets/、guard/、rollbacks/、storages/
- **没有**：skills/、memories/、team/
- **版本检测**：profiles/*/package.json

### 桌面版 Lite（纯壳，无本地 Home）
- **路径**：`%APPDATA%/com.deepseek.dsh.desktop.lite`
- **只有**：logs/、settings.json、run-state.json
- **无 sessions/profiles**，可能是纯壳或云端版

### 共享技能库
- **路径**：`~/.agents/skills/`
- **跨所有版本共享**

## 二、现有 scanner.rs 的问题

1. **漏检**：`~/.dsh-v4lite` 能被 `.dsh*` 通配，但如果用户自定义 `.dsh-backup` 等也会误检
2. **AIO 检测**：能检测 `dsh-home`，但没有区分 AIO 和其他桌面壳
3. **无环境变量支持**：没有读 `DSH_AGENTS_HOME`（.agents 路径可能自定义）
4. **无 junction 检测**：如果 Home 是 junction，可能重复扫描
5. **无 Lite 壳检测**：桌面版 Lite 没有 dsh-home，但可能有数据在其他位置

## 三、智能扫描算法改进

### 分层扫描策略
```
Layer 0: DSH_HOME 环境变量（最高优先级）
Layer 1: 用户目录下的 .dsh*（.dsh、.dsh-v4lite、.dsh-xxx）
Layer 2: AppData 桌面壳的 dsh-home（AIO）
Layer 3: 内容特征验证（sessions/ + profiles/ + settings.yaml）
Layer 4: 共享组件识别（.agents、DSH_AGENTS_HOME）
Layer 5: junction/symlink 去重（realpath 规范化）
```

### 内容特征识别（不依赖固定路径）
```rust
fn is_home(path: &Path) -> bool {
    // 必要条件：有 sessions/ 或 profiles/
    let has_sessions = path.join("sessions").is_dir();
    let has_profiles = path.join("profiles").is_dir();
    if !has_sessions && !has_profiles { return false; }

    // 充分条件：有身份文件（settings.yaml 或 .credentials.yaml）
    let has_identity = path.join("settings.yaml").is_file()
        || path.join(".credentials.yaml").is_file()
        || has_profiles;  // profiles/ 存在本身就是身份

    has_identity
}
```

### 变体识别算法
```rust
fn detect_variant(path: &Path) -> &'static str {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    
    // 按目录名识别
    if name == ".dsh" { return "官方版/EAC"; }
    if name == ".dsh-v4lite" { return "v4Lite"; }
    if name == "dsh-home" {
        // 按父目录识别
        if let Some(parent) = path.parent() {
            let parent_name = parent.file_name().unwrap_or_default().to_string_lossy();
            if parent_name.contains("aio") { return "AIO"; }
            if parent_name.contains("lite") { return "Lite"; }
        }
        return "桌面壳";
    }
    if name.starts_with(".dsh-") { return "自定义"; }
    "未知"
}
```

### junction/symlink 去重
```rust
fn canonical_path(path: &Path) -> PathBuf {
    // 使用 realpath 解析 junction/symlink，避免重复扫描
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}
```

## 四、Symlink 路由算法设计

### 核心原则（参考 GNU Stow）
1. **两阶段**：Plan（检测冲突）→ Execute（执行链接）
2. **相对路径**：所有 symlink 使用相对路径（便携性）
3. **冲突检测**：目标已存在且不是我们的链接 → 冲突
4. **无状态**：不存数据库，从文件系统推导状态

### 接管（Adopt）流程
```
1. 校验：目标环境未被接管（没有 .dshvault 标记）
2. 创建标记：在仓库创建 .dshvault 标记文件
3. 移动文件：把 sessions/、skills/、profiles/ 等移到仓库
4. 创建链接：在原位置创建 symlink 指向仓库
5. 记录映射：在 manifest 中记录 symlink 映射关系
```

### 切换流程
```
1. 校验：来源和目标都已接管
2. 自动备份：给目标环境创建快照
3. 删除旧链接：删除目标环境的 symlink
4. 创建新链接：指向来源环境的文件
5. 验证：检查新链接是否有效
```

### 冲突处理
| 冲突类型 | 处理方式 |
|---|---|
| 目标已有普通文件 | 提示用户：移动（adopt）/ 覆盖（override）/ 跳过 |
| 目标已有其他链接 | 提示用户：替换链接 / 跳过 |
| 目标不存在 | 直接创建链接 |

## 五、关键风险与缓解

| 风险 | 缓解 |
|---|---|
| DSH 更新后目录结构变化 | 接管前校验目录结构，不匹配时提示 |
| 用户手动删除仓库文件 | 启动时校验 symlink 有效性，失效时提示修复 |
| 多环境共享文件冲突 | 切换前检测引用，提示用户 |
| junction 导致重复扫描 | realpath 规范化去重 |
| Lite 壳无本地 Home | 提示用户"此版本无本地数据" |

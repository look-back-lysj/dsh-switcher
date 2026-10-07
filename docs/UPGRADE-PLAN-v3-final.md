# DSH Vault v3 重构计划书（Symlink 农场 + 智能扫描版）

> 本文档是给接力模型的完整提示词。
> 核心设计理念：**用操作系统 symlink 实现"零复制"的统一文件管理**，参考 GNU Stow 的 symlink 农场模式。
> 扫描算法基于 DSH 官方 `resolveDshHome` 逻辑 + 实测各版本存储结构。

---

## §0 交接必读

| 项 | 位置 |
|---|---|
| 项目根目录 | `C:\Users\刘沛伦\Desktop\升级eac\dsh-vault\` |
| Rust 后端 | `src-tauri\src\`（main/model/repo/restore/scanner/export/zstd_check） |
| 前端 | `frontend\`（index.html / app.js / style.css，原生三件套，禁 npm/React） |
| 进度记录 | `docs\PROGRESS.md` |
| 视觉规范 | `DESIGN.md` |
| 默认仓库 | `D:\DSH-Backups\repo` |
| 已装程序 | `C:\Users\刘沛伦\AppData\Local\DSH Vault\dsh-vault.exe` |
| DSH 源码 | `D:\deepseek-harness-source\deepseek-harness-master\` |
| EAC 源码 | `D:\DSH-EAC\DSH-Desktop-EAC\` |

**工具链**：Rust `stable-x86_64-pc-windows-msvc`，VS BuildTools `D:\BuildTools`，Tauri CLI `D:\rust-tools\bin\cargo-tauri.exe`，构建缓存 `CARGO_TARGET_DIR=D:\rust-target\dsh-vault-msvc`。

**铁律**：
1. `tauri.conf.json` 必须保留 `withGlobalTauri: true`
2. 改完前端行为没变化，先杀 `dsh-vault` 进程再 build
3. 窗口由代码创建（WebviewWindowBuilder）
4. Windows symlink 创建已验证可行（开发者模式开启，无需管理员权限）

**调研文档**（已保存到 docs/）：
- `STOW-FINDINGS.md`：GNU Stow 核心设计决策
- `DSH-STORAGE-FINDINGS.md`：DSH 各版本存储结构
- `DSH-SCANNER-DESIGN.md`：智能扫描算法设计

---

## §1 核心设计变更

### 从"复制备份"到"链接管理"

```
旧模式（v2）：
  备份 → 复制文件到仓库 → 仓库和原处各存一份 → 占双倍空间
  恢复 → 从仓库复制回原处 → 又是复制

新模式（v3，Symlink 农场）：
  接管 → 移动文件到仓库 → 原位置留 symlink → 只占一份空间
  切换 → 改 symlink 指向 → 瞬间完成，零复制
  DSH 使用 → 读写 symlink → 实际读写仓库文件 → Vault 扫描能发现变更
```

### 技术验证（已完成 ✅）
- Windows 开发者模式已开启，`os.symlink()` 和 `mklink /J` 均可用
- NTFS 支持 symlink 和 junction
- DSH 的 sessions 目录结构简单（无索引文件），适合 symlink 管理
- GNU Stow 30 年验证：两阶段 Plan+Execute、Tree Folding、冲突检测、Adopt 模式

---

## §2 智能扫描算法（基于实测改进）

### DSH 各版本存储结构（实测确认）

| 版本 | Home 路径 | 特征 | 共享 |
|---|---|---|---|
| 官方版/EAC | `~/.dsh` | sessions/、profiles/、skills/ | 与 EAC 共享 |
| AIO | `%APPDATA%/com.deepseek.dsh.desktop.aio/dsh-home` | 多 memories/、team/ | 独立 |
| v4Lite | `~/.dsh-v4lite` | 无 skills/、memories/ | 独立 |
| Lite | `%APPDATA%/com.deepseek.dsh.desktop.lite` | 纯壳，无本地数据 | 无 |
| 共享技能库 | `~/.agents/skills/` | 跨版本共享 | 所有版本 |

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
    let has_sessions = path.join("sessions").is_dir();
    let has_profiles = path.join("profiles").is_dir();
    if !has_sessions && !has_profiles { return false; }
    let has_identity = path.join("settings.yaml").is_file()
        || path.join(".credentials.yaml").is_file()
        || has_profiles;
    has_identity
}
```

### 变体识别算法
```rust
fn detect_variant(path: &Path) -> &'static str {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    if name == ".dsh" { return "官方版/EAC"; }
    if name == ".dsh-v4lite" { return "v4Lite"; }
    if name == "dsh-home" {
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
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}
```

---

## §3 信息架构（3 页签）

```
[侧边导航]
  环境    → 扫描环境 → 查看接管状态 → 一键接管（建链接）→ 变更提醒
  存档    → 快照列表 → 回到任意状态 → 导出/导入
  切换    → A 环境 → B 环境（拖拽卡片改链接指向）
```

---

## §4 模块一：环境页（接管 + 状态监控）

### 页面布局
```
[统计卡片行]（保留现有 4 个指标 + 数字滚动动画）

[环境列表]
  每行：
    [状态徽章] 已接管 / 未接管 / 有变更
    [环境名称] 主版本 (.dsh) / v4Lite / AIO
    [路径] C:\Users\...\.dsh
    [统计] 14 对话 · 2 技能 · 449 MB
    [操作] [接管] 或 [查看变更] 或 [断开链接]

[一键接管区]（底部）
  "已识别 3 个环境，共 48 个对话，约 470 MB"
  [备注输入框]（选填）
  [一键接管全部] 大按钮
  [高级选项 ▾]（仓库路径、仅接管配置）

[最近快照]（精简列表，最多 3 条）
  [查看全部 →] 跳转存档页
```

### 后端新增命令
```rust
// 接管单个环境：移动文件到仓库 + 创建 symlink
adopt_home(repo: String, home_path: String, note: String) -> Result<AdoptResult, String>

// 断开接管：把 symlink 替换回真实文件
unadopt_home(repo: String, home_path: String) -> Result<UnadoptResult, String>

// 检查环境变更：扫描 symlink 指向的文件是否有修改
check_home_changes(repo: String, home_path: String) -> Result<Vec<ChangeInfo>, String>
```

### 接管流程（参考 GNU Stow --adopt）
```
1. 校验：目标环境未被接管（没有 .dshvault 标记）
2. 创建标记：在仓库创建 .dshvault 标记文件
3. 移动文件：把 sessions/、skills/、profiles/ 等移到仓库 files/<home_id>/
4. 创建链接：在原位置创建 symlink 指向仓库
   - 文件用 symlink，目录用 junction（Windows 兼容性）
   - 使用相对路径（便携性）
5. 记录映射：在 manifest 中记录 symlink 映射关系
6. 返回结果：移动了多少文件、创建了多少链接
```

---

## §5 模块二：存档页（快照 + 恢复）

### 页面布局
```
[顶部操作区]
  [把当前状态存为快照] 按钮
  [刷新列表]
  [导入备份包]（次要）

[快照列表]
  每张卡片：备注（可双击改名）/ 时间 / 环境数 / 文件数 / 大小
  点击卡片展开详情面板

[详情面板]
  - 包含哪些环境
  - 文件变更对比（与当前状态对比）
  - [回到这个状态] 主按钮
  - [导出为 zip] 次按钮
  - [删除此快照] 危险按钮

[高级操作 ▾]
  - 校验仓库完整性
  - 查看回滚日志
  - 撤回上次覆盖
```

### 后端改动
- 快照只记录清单（SHA-256），不复制文件实体
- 文件实体统一存在 `files/<home_id>/<path>`（可能被多个快照引用）
- 新增 `restore_snapshot(repo, snapshot_id, target_home)`：把 symlink 指向快照记录的版本

---

## §6 模块三：切换页（核心新功能）

### 交互设计（卡片拖拽）
```
[说明卡片]
  "把左边环境的链接指向右边环境的文件。
   切换前会自动备份右边环境，不会丢东西。"

[拖拽区]
  ┌─────────────┐         ┌─────────────┐
  │  来源环境    │   ──►   │  目标环境    │
  │  主版本      │         │  v4lite     │
  │  14 对话     │         │  18 对话     │
  │  已接管      │         │  已接管      │
  └─────────────┘         └─────────────┘

[下方：环境列表]
  每个环境一张卡片，可拖拽到上方"来源"或"目标"槽位
  卡片内容：名称 / 对话数 / 接管状态 / [设为来源] [设为目标] 按钮

[切换选项]
  ☑ 对话记录（sessions）
  ☑ 技能（skills）
  ☑ 配置档案（profiles/settings）
  ☐ 记忆（memories）

[开始切换] 大按钮
```

### 切换流程
```
1. 校验：来源和目标都已接管，且不是同一个
2. 自动备份：给目标环境创建快照（备注："切换前自动保存"）
3. 删除旧链接：删除目标环境的 symlink
4. 创建新链接：指向来源环境的文件（按用户勾选的类型）
5. 验证：检查新链接是否有效
6. 返回结果：切换了多少个链接
```

### 后端新增命令
```rust
switch_links(
    repo: String,
    source_home: String,
    target_home: String,
    include_sessions: bool,
    include_skills: bool,
    include_config: bool,
    include_memories: bool,
) -> Result<SwitchResult, String>
```

---

## §7 模块四：变更检测与同步

### 实现方案
```rust
scan_home_changes(repo: String, home_path: String) -> Result<Vec<ChangeInfo>, String>

// 对比逻辑：
// 1. 读取 home_path 下所有 symlink 指向的实际文件
// 2. 计算当前 SHA-256
// 3. 与 manifest 中记录的 SHA-256 对比
// 4. 返回差异列表
```

### 前端展示
- 环境列表每行加"有变更"徽章（橙色）
- 点击"查看变更"展开变更列表
- 变更列表：文件路径 / 变更类型（修改/新增/删除）/ [接受变更] [还原]

---

## §8 执行顺序

1. **智能扫描改进**：分层扫描 + 内容特征识别 + 变体识别 + junction 去重
2. **环境页接管功能**：后端 adopt_home/unadopt_home + 前端接管按钮
3. **变更检测**：后端 scan_home_changes + 前端变更徽章
4. **存档页快照改造**：清单式快照（不复制文件）
5. **切换页**：后端 switch_links + 前端拖拽交互
6. **导航简化**：3 个页签（环境/存档/切换）
7. **测试**：单元测试 + e2e + 手动验证
8. **打包安装**

---

## §9 验收标准

### 功能验收
- [ ] 环境页可以接管/断开接管环境，显示接管状态和变更提醒
- [ ] 接管后 DSH 程序正常运行，读写的是 symlink 指向的仓库文件
- [ ] 存档页可以创建快照、查看快照列表、回到任意状态
- [ ] 切换页可以把 A 环境的链接指向 B 环境的文件
- [ ] 切换前自动备份目标环境，失败可回滚
- [ ] 变更检测能发现 DSH 程序对文件的修改

### 技术验收
- [ ] symlink 创建/删除/切换正常
- [ ] DSH 程序无法区分 symlink 和真实文件
- [ ] 文件修改后 SHA-256 校验能发现变更
- [ ] 快照只记录清单，不复制文件，仓库体积不翻倍
- [ ] 智能扫描能识别所有版本（官方/EAC/AIO/v4Lite/自定义）
- [ ] junction/symlink 去重，不会重复扫描

### 体验验收
- [ ] 新手 3 分钟内完成：接管 → 切换 → 恢复
- [ ] 每个页面只有 1 个主按钮
- [ ] 危险操作有确认弹窗和中文解释

---

## §10 风险与缓解

| 风险 | 缓解 |
|---|---|
| symlink 被杀毒软件误报 | 提供"断开接管"还原方案 |
| 用户手动删除仓库文件 | 启动时校验 symlink 有效性，失效时提示修复 |
| 多环境共享文件切换冲突 | 切换前检测引用，提示用户 |
| junction 导致重复扫描 | realpath 规范化去重 |
| Lite 壳无本地 Home | 提示用户"此版本无本地数据" |
| DSH 更新后目录结构变化 | 接管前校验目录结构，不匹配时提示 |
| 用户自定义 Home 路径 | DSH_HOME 环境变量最高优先级 |

---

## §11 明确不做（v3 范围外）

- 对话浏览器（按标题搜索、单独恢复某条对话）→ v3.1
- CAS 内容寻址存储（多环境共享相同文件去重）→ v3.1
- 部分切换（只搬对话不搬技能）→ v3.1（本次默认全选，但保留 checkbox）
- 定时备份 → v3.2
- 云同步 → 不做

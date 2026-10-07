# DSH Vault v3 重构计划书（Symlink 农场版）

> 本文档是给接力模型的完整提示词。
> 核心设计理念：**用操作系统 symlink 实现"零复制"的统一文件管理**，参考 GNU Stow 的 symlink 农场模式。

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

**工具链**：Rust `stable-x86_64-pc-windows-msvc`，VS BuildTools `D:\BuildTools`，Tauri CLI `D:\rust-tools\bin\cargo-tauri.exe`，构建缓存 `CARGO_TARGET_DIR=D:\rust-target\dsh-vault-msvc`。

**铁律**：
1. `tauri.conf.json` 必须保留 `withGlobalTauri: true`
2. 改完前端行为没变化，先杀 `dsh-vault` 进程再 build
3. 窗口由代码创建（WebviewWindowBuilder）
4. Windows symlink 创建已验证可行（开发者模式开启，无需管理员权限）

**当前状态**：v2 已完成（备份一键化 + 时光机 + 动效），但信息架构仍有 5 个页签，用户反馈"太碎、看不懂、备份占双倍空间"。

---

## §1 核心设计变更：从"复制备份"到"链接管理"

### 旧模式（v2，将被废弃）
```
用户点击备份 → Vault 把文件复制到仓库 → 仓库和原处各存一份 → 占双倍空间
用户点击恢复 → Vault 把文件从仓库复制回原处 → 又是复制
```

### 新模式（v3，Symlink 农场）
```
用户点击"接管" → Vault 把原文件移到仓库 → 在原位置创建 symlink 指向仓库 → 只占一份空间
DSH 正常使用 → 读写的是 symlink → 实际读写仓库里的文件 → Vault 扫描能发现变化
用户点击"切换" → Vault 把 symlink 指向另一个环境的文件 → 瞬间完成，零复制
```

### 技术验证（已完成 ✅）
- Windows 开发者模式已开启，`os.symlink()` 和 `mklink /J` 均可用
- NTFS 支持 symlink 和 junction
- DSH 的 sessions 目录结构简单（无索引文件），适合 symlink 管理

---

## §2 信息架构（3 页签）

```
[侧边导航]
  环境    → 扫描环境 → 查看状态 → 一键接管（建链接）
  存档    → 快照列表 → 回到任意状态 → 导出/导入
  切换    → A 环境 → B 环境（改链接指向）
```

---

## §3 模块一：环境页（接管 + 状态监控）

### 功能定义
- **接管**：把 DSH 环境的文件移到 Vault 仓库，在原位置留 symlink
- **状态监控**：显示每个环境的"已接管/未接管"状态，以及文件变更提醒

### 页面布局
```
[统计卡片行]（保留现有 4 个指标）

[环境列表]
  每行：
    [状态徽章] 已接管 / 未接管 / 有变更
    [环境名称] 主版本 (.dsh)
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

// 断开接管：把 symlink 替换回真实文件（从仓库复制回来）
unadopt_home(repo: String, home_path: String) -> Result<UnadoptResult, String>

// 检查环境变更：扫描 symlink 指向的文件是否有修改
check_home_changes(repo: String, home_path: String) -> Result<Vec<ChangeInfo>, String>

pub struct AdoptResult {
    pub home_path: String,
    pub files_moved: u32,
    pub symlinks_created: u32,
    pub bytes_moved: u64,
    pub snapshot_id: String,  // 接管时的快照
}

pub struct ChangeInfo {
    pub file_path: String,
    pub change_type: ChangeType,  // Modified / Added / Deleted
    pub old_sha: Option<String>,
    pub new_sha: Option<String>,
}
```

### 接管流程（后端实现）
```
1. 校验：目标环境未被接管（没有 symlink）
2. 创建快照：记录当前文件清单（SHA-256）
3. 移动文件：把 sessions/、skills/、profiles/ 等目录下的文件移到仓库 files/ 目录
   - 保留目录结构：files/<home_id>/<original_rel_path>
4. 创建 symlink：在原位置创建 symlink 指向仓库里的文件
   - 文件用 symlink，目录用 junction（Windows 兼容性更好）
5. 更新 manifest：记录 symlink 映射关系
6. 返回结果：移动了多少文件、创建了多少链接
```

### 断开接管流程
```
1. 校验：目标环境已被接管
2. 删除 symlink
3. 从仓库复制文件回原位置
4. 清理仓库中不再被引用的文件（如果其他环境也不用）
5. 更新 manifest
```

---

## §4 模块二：存档页（快照 + 恢复）

### 功能定义
- 快照是"某一时刻的文件状态记录"，不复制文件，只记录 SHA-256 清单
- 恢复是"把 symlink 重新指向快照记录的文件版本"

### 与 v2 的区别
- v2：快照 = 复制文件到 snapshots/<id>/
- v3：快照 = 记录文件清单 + SHA-256，文件实体始终在 files/ 目录（可能被多个快照引用）

### 页面布局
```
[顶部操作区]
  [把当前状态存为快照] 按钮
  [刷新列表]
  [导入备份包]（次要）

[快照列表]
  每张卡片：
    [备注]（可双击改名）
    [时间] 2026-10-07 18:35
    [环境] 3 个环境
    [文件] 2322 个文件
    [大小] 58.6 MB（指快照清单大小，不是文件实体）
    [操作] [回到这个状态] [导出] [删除]

[详情面板]（选中快照后）
  - 包含哪些环境
  - 文件变更对比（与当前状态对比）
  - [回到这个状态] 主按钮

[高级操作 ▾]
  - 校验仓库完整性
  - 查看回滚日志
  - 撤回上次覆盖
  - 导出/导入 zip
```

### 后端改动
- 快照不再复制文件，只保存 manifest 副本到 `snapshots/<id>/manifest.json`
- 文件实体统一存在 `files/<sha256>` 或 `files/<home_id>/<path>`（取决于是否多环境共享）
- 新增 `restore_snapshot(repo, snapshot_id, target_home)`：把 symlink 指向快照记录的版本

---

## §5 模块三：切换页（核心新功能）

### 功能定义
> 把 A 环境的 symlink 指向 B 环境的文件，让 A 环境"变成"B 环境。

### 用户故事
- "我想让 v4lite 用主版本的对话" → 把 v4lite 的 sessions symlink 指向主版本的 sessions
- "我想试试新装的民间版，但不想从头开始" → 把新环境的 symlink 指向旧环境的文件

### 交互设计（卡片拖拽）

**页面布局**：
```
[说明卡片]
  "把左边环境的链接指向右边环境的文件。
   切换前会自动备份右边环境，不会丢东西。"

[拖拽区]
  ┌─────────────┐         ┌─────────────┐
  │  来源环境    │         │  目标环境    │
  │  (拖我)      │   ──►   │  (放这里)    │
  │  主版本      │         │  v4lite     │
  │  14 对话     │         │  18 对话     │
  │  已接管      │         │  已接管      │
  └─────────────┘         └─────────────┘

[下方：环境列表]
  每个环境一张卡片，可拖拽到上方"来源"或"目标"槽位
  卡片内容：名称 / 对话数 / 接管状态 / [设为来源] [设为目标] 按钮

[切换选项]（选中后显示）
  ☑ 对话记录（sessions）
  ☑ 技能（skills）
  ☑ 配置档案（profiles/settings）
  ☐ 记忆（memories）

[开始切换] 大按钮
```

### 切换流程（后端实现）
```
1. 校验：来源和目标都已接管，且不是同一个
2. 自动备份目标环境（创建快照，备注："切换前自动保存"）
3. 切换 symlink：
   - 删除目标环境原有的 symlink
   - 创建新 symlink 指向来源环境的文件
   - 按用户勾选的类型分别处理
4. 更新 manifest：记录新的 symlink 映射
5. 返回结果：切换了多少个链接
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

pub struct SwitchResult {
    pub backup_snapshot_id: String,
    pub links_switched: u32,
    pub links_failed: Vec<String>,
}
```

---

## §6 模块四：变更检测与同步

### 功能定义
- DSH 程序修改文件时，改的是 symlink 指向的仓库文件
- Vault 定期扫描（或手动触发）检测变更，提示用户"有 N 个文件被修改"

### 实现方案
```rust
// 扫描环境变更
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

## §7 模块五：兼容性与迁移

### 与 v2 仓库的兼容
- v2 仓库是"复制式"，v3 是"链接式"
- 迁移策略：首次启动 v3 时，提示用户"是否把现有仓库转为链接式管理"
- 转换过程：把 v2 仓库里的文件复制回各环境，然后重新执行"接管"

### 新电脑迁移
- 导出 zip 时：把 symlink 指向的实际文件打包（不是 symlink 本身）
- 导入 zip 时：解压到仓库，创建 symlink

---

## §8 执行顺序

1. **环境页接管功能**：后端 `adopt_home` / `unadopt_home` + 前端接管按钮
2. **变更检测**：后端 `scan_home_changes` + 前端变更徽章
3. **存档页快照**：改造为"清单式快照"（不复制文件）
4. **切换页**：后端 `switch_links` + 前端拖拽交互
5. **导航简化**：3 个页签（环境/存档/切换）
6. **迁移向导**：v2 → v3 仓库迁移
7. **测试**：单元测试 + e2e + 手动验证
8. **打包安装**

---

## §9 验收标准

### 功能验收
- [ ] 环境页可以接管/断开接管环境，显示接管状态和变更提醒
- [ ] 接管后 DSH 程序正常运行，读写的是 symlink 指向的仓库文件
- [ ] 存档页可以创建快照、查看快照列表、回到任意状态
- [ ] 切换页可以把 A 环境的链接指向 B 环境的文件，切换后 A 环境拥有 B 的内容
- [ ] 切换前自动备份目标环境，失败可回滚
- [ ] 变更检测能发现 DSH 程序对文件的修改

### 技术验收
- [ ] symlink 创建/删除/切换正常
- [ ] DSH 程序无法区分 symlink 和真实文件
- [ ] 文件修改后 SHA-256 校验能发现变更
- [ ] 快照只记录清单，不复制文件，仓库体积不翻倍

### 体验验收
- [ ] 新手 3 分钟内完成：接管 → 切换 → 恢复
- [ ] 每个页面只有 1 个主按钮
- [ ] 危险操作有确认弹窗和中文解释

---

## §10 风险与缓解

| 风险 | 缓解 |
|---|---|
| symlink 被杀毒软件误报 | 提前告知用户，提供"断开接管"还原方案 |
| DSH 更新后目录结构变化 | 接管前校验目录结构，不匹配时提示用户 |
| 用户手动删除仓库文件导致 symlink 失效 | 启动时校验 symlink 有效性，失效时提示修复 |
| 多环境共享文件时切换冲突 | 切换前检测文件是否被其他环境引用，提示用户 |
| v2 用户升级后数据丢失 | 提供迁移向导，转换前自动备份 |

---

## §11 明确不做（v3 范围外）

- 对话浏览器（按标题搜索、单独恢复某条对话）→ v3.1
- CAS 内容寻址存储（多环境共享相同文件去重）→ v3.1
- 部分切换（只搬对话不搬技能）→ v3.1（本次默认全选，但保留 checkbox）
- 定时备份 → v3.2
- 云同步 → 不做

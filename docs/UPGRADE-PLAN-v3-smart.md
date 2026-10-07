# DSH Vault v3 重构计划书（智能扫描 + Symlink 农场版）

> 核心设计：**零固定路径**，纯粹靠内容特征识别 DSH Home。
> 扫描算法已实测验证：DSH Home 得分 200+，非 DSH 目录得分 0-10。

---

## §0 交接必读

| 项 | 位置 |
|---|---|
| 项目根目录 | `C:\Users\刘沛伦\Desktop\升级eac\dsh-vault\` |
| Rust 后端 | `src-tauri\src\`（main/model/repo/restore/scanner/export/zstd_check） |
| 前端 | `frontend\`（index.html / app.js / style.css） |
| 调研文档 | `docs/STOW-FINDINGS.md`、`docs/DSH-SMART-SCANNER.md` |
| 默认仓库 | `D:\DSH-Backups\repo` |

**工具链**：Rust `stable-x86_64-pc-windows-msvc`，VS BuildTools `D:\BuildTools`，Tauri CLI `D:\rust-tools\bin\cargo-tauri.exe`。

**铁律**：
1. `tauri.conf.json` 必须保留 `withGlobalTauri: true`
2. 改完前端行为没变化，先杀 `dsh-vault` 进程再 build
3. Windows symlink 创建已验证可行（开发者模式开启）

---

## §1 核心设计

### 智能扫描（零固定路径）
不预设"去哪里找"，而是"怎么认出它是 DSH Home"。
扫描整个文件系统，对每个目录打分，分数够高就是 DSH Home。

### Symlink 农场（参考 GNU Stow）
接管 = 移动文件到仓库 + 原位置留 symlink（只占一份空间）
切换 = 改 symlink 指向（瞬间完成，零复制）

---

## §2 智能扫描算法（已实测验证）

### 特征信号体系

**强信号（单独即可确认）**：
| 信号 | 权重 | 实测验证 |
|---|---|---|
| sessions/ 目录下有 .zstd 文件 | +40 | .dsh 有 14 个，v4lite 有 18 个，AIO 有 16 个 |
| profiles/*/package.json 含 "@deepseek-ai/dsh" | +40 | 三个版本都有 |
| .credentials.yaml 含 "refs:" | +35 | 三个版本都有 |
| settings.yaml 含 "agent-presets:" | +35 | v4lite 和 AIO 有 |
| .dshw-usage.json 或 .dshw-size.json | +30 | .dsh 和 AIO 有 |

**中信号（组合确认）**：
| 信号 | 权重 | 实测验证 |
|---|---|---|
| .agent-presets/ 目录 | +15 | 三个版本都有 |
| guard/ 目录含 state.json | +15 | 三个版本都有 |
| skills/ 目录 | +10 | .dsh 有 |
| storages/ 目录含 workspace.json | +10 | 三个版本都有 |
| rollbacks/ 目录 | +10 | 三个版本都有 |
| undo-snapshots/ 目录 | +10 | .dsh 有 |
| memories/ 目录 | +10 | AIO 特有 |
| team/ 目录 | +10 | AIO 特有 |

**负信号（排除）**：
| 信号 | 权重 | 说明 |
|---|---|---|
| node_modules/ 在根层 | -50 | 项目目录 |
| .git/ 在根层 | -30 | 源码目录 |
| 目录名为 "Cache"/"GPUCache" | -50 | 缓存目录 |

### 评分规则
- **确认 DSH Home**：总分 >= 60
- **疑似 DSH Home**：40 <= 总分 < 60
- **不是 DSH Home**：总分 < 40

### 实测验证结果
| 路径 | 得分 | 判定 |
|---|---|---|
| ~/.dsh | 215 | ✅ DSH Home |
| ~/.dsh-v4lite | 200 | ✅ DSH Home |
| AIO dsh-home | 280 | ✅ DSH Home |
| ~/.agents | 10 | ❌ 共享技能库（单独识别） |
| ~/.codex | 10 | ❌ 非 DSH |
| 项目目录 | 0 | ❌ 非 DSH |

### 扫描策略
```
Layer 0: DSH_HOME 环境变量（如果设置，直接确认）
Layer 1: 快速扫描（~/、~/AppData/Roaming/、~/AppData/Local/、D:/，深度 3 层，5 秒超时）
Layer 2: 深度扫描（全盘，深度 5 层，30 秒超时，手动触发）
Layer 3: junction/symlink 去重（realpath 规范化）
```

---

## §3 信息架构（3 页签）

```
[侧边导航]
  环境    → 智能扫描 → 查看接管状态 → 一键接管
  存档    → 快照列表 → 回到任意状态
  切换    → A 环境 → B 环境（拖拽改链接）
```

---

## §4 模块一：环境页（接管 + 状态监控）

### 后端新增命令
```rust
adopt_home(repo: String, home_path: String, note: String) -> Result<AdoptResult, String>
unadopt_home(repo: String, home_path: String) -> Result<UnadoptResult, String>
check_home_changes(repo: String, home_path: String) -> Result<Vec<ChangeInfo>, String>
```

### 接管流程（参考 GNU Stow --adopt）
```
1. 校验：目标环境未被接管
2. 创建标记：在仓库创建 .dshvault 标记
3. 移动文件：sessions/、skills/、profiles/ 等移到仓库 files/<home_id>/
4. 创建链接：原位置创建相对路径 symlink 指向仓库
5. 记录映射：manifest 中记录 symlink 映射
```

---

## §5 模块二：存档页（快照 + 恢复）

### 后端改动
- 快照只记录清单（SHA-256），不复制文件实体
- 新增 `restore_snapshot(repo, snapshot_id, target_home)`

---

## §6 模块三：切换页（核心新功能）

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

### 切换流程
```
1. 校验：来源和目标都已接管
2. 自动备份：给目标环境创建快照
3. 删除旧链接：删除目标环境的 symlink
4. 创建新链接：指向来源环境的文件
5. 验证：检查新链接是否有效
```

---

## §7 执行顺序

1. **智能扫描重构**：scanner.rs 改为特征打分制，零固定路径
2. **环境页接管功能**：后端 adopt/unadopt + 前端按钮
3. **变更检测**：后端 scan_home_changes + 前端徽章
4. **存档页快照改造**：清单式快照
5. **切换页**：后端 switch_links + 前端拖拽
6. **导航简化**：3 个页签
7. **测试**：单元测试 + e2e + 手动验证
8. **打包安装**

---

## §8 验收标准

### 功能验收
- [ ] 智能扫描能识别所有版本（官方/EAC/AIO/v4Lite/自定义）
- [ ] 扫描不依赖固定路径，新装版本也能识别
- [ ] 接管后 DSH 正常运行，读写 symlink 指向的仓库文件
- [ ] 切换后目标环境拥有来源的内容
- [ ] 变更检测能发现 DSH 对文件的修改

### 技术验收
- [ ] DSH Home 得分 >= 60，非 DSH 目录得分 < 40
- [ ] symlink 创建/删除/切换正常
- [ ] 快照只记录清单，不复制文件
- [ ] junction/symlink 去重，不重复扫描

### 体验验收
- [ ] 新手 3 分钟内完成：扫描 → 接管 → 切换
- [ ] 每个页面只有 1 个主按钮
- [ ] 危险操作有确认弹窗和中文解释

---

## §9 风险与缓解

| 风险 | 缓解 |
|---|---|
| 扫描漏检新装版本 | 特征打分制，不依赖固定路径 |
| 扫描误检非 DSH 目录 | 负信号排除 + 疑似确认 |
| symlink 被杀毒软件误报 | 提供"断开接管"还原方案 |
| 用户手动删除仓库文件 | 启动时校验 symlink 有效性 |
| 多环境共享文件冲突 | 切换前检测引用，提示用户 |
| junction 导致重复扫描 | realpath 规范化去重 |

---

## §10 明确不做（v3 范围外）

- 对话浏览器 → v3.1
- CAS 内容寻址存储 → v3.1
- 部分切换 → v3.1（本次默认全选，保留 checkbox）
- 定时备份 → v3.2
- 云同步 → 不做

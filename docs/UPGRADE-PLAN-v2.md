# DSH Vault v2 升级计划书（执行版）

> 本文档是给**下一个接力模型**的执行指令书，不是泛泛的设想。
> 读者默认你从未见过本项目：先读本文件 §0 的"交接必读"，再按模块顺序执行。
> 全程中文交流；用户是技术小白，所有界面文案用大白话，禁止专业术语裸奔。

---

## §0 交接必读（先读这些，再动手）

| 项 | 位置 |
|---|---|
| 项目根目录 | `C:\Users\刘沛伦\Desktop\升级eac\dsh-vault\` |
| Rust 后端 | `src-tauri\src\`（main/model/repo/restore/scanner/export/zstd_check） |
| 前端 | `frontend\`（index.html / app.js / style.css，原生三件套，**禁 npm/React**） |
| 当前进度记录 | `docs\PROGRESS.md` |
| 现有视觉规范 | `DESIGN.md`（本次升级要按 §4 改写） |
| 默认备份仓库 | `D:\DSH-Backups\repo` |
| 已装程序 | `C:\Users\刘沛伦\AppData\Local\DSH Vault\dsh-vault.exe` |
| 回归脚本 | `e2e-regression.py`（CDP 驱动，见 §9） |

**工具链（勿改路径，全在 D 盘防占 C 盘）：**
- Rust 工具链：`stable-x86_64-pc-windows-msvc`（Tauri 不支持 gnu）
- VS BuildTools：`D:\BuildTools`，Tauri CLI：`D:\rust-tools\bin\cargo-tauri.exe`
- 构建缓存：`CARGO_TARGET_DIR=D:\rust-target\dsh-vault-msvc`

**构建命令：**
```powershell
$env:RUSTUP_TOOLCHAIN='stable-x86_64-pc-windows-msvc'
$env:CARGO_TARGET_DIR='D:\rust-target\dsh-vault-msvc'
cargo test --manifest-path src-tauri\Cargo.toml
& 'D:\BuildTools\Common7\Tools\VsDevCmd.bat' -arch=x64 -host_arch=x64
& 'D:\rust-tools\bin\cargo-tauri.exe' build --ci --no-sign --bundles nsis --config src-tauri\tauri.conf.json
```

**铁律（之前踩过的坑）：**
1. `tauri.conf.json` 必须保留 `withGlobalTauri: true`，否则所有按钮失灵。
2. 改完前端行为没变化，**先杀 `dsh-vault` 进程再 build**，是 exe 被锁不是代码没生效。
3. PowerShell 写文件用逐行数组或 here-string，别把 `` `n `` 写成字面量进代码。
4. 联网命令在沙箱里会静默卡死，但本任务环境已是 danger-full-access + approval never，**不要**再传 `sandbox_permissions`。
5. 窗口由代码创建（WebviewWindowBuilder），不要改回配置声明式。

---

## §1 本次升级要解决的 5 个用户痛点（验收标准）

| # | 用户原话 | 根因 | 验收 |
|---|---|---|---|
| 1 | "界面没有有诚意的丝滑小动画" | 只做了一次性 `fadeInUp`，无交互反馈动画 | §4 动效清单全部落地，hover/press/进度/弹窗/完成态均有 60fps 反馈 |
| 2 | "回滚和备份太复杂，要用户自己选文件夹" | 备份页留了路径输入框当主流程 | 一键备份为主流程（§2），路径输入全部退到"高级选项"折叠区 |
| 3 | "很难回滚和切环境" | 没有"切换/回滚"这个用户视角的功能 | 新增"时光机"页（§3），快照列表一键回滚 |
| 4 | "尽量减少直接文件选取，说明要更简单" | 4 个页签 8 个路径输入框 | 页签减为 3 个，路径输入框默认隐藏（§2/§5） |
| 5 | "知道了按钮点了不消失" | `hidden` 属性被 CSS `display` 覆盖（app.js:517 绑定正确但样式没设 `[hidden]{display:none}`） | §6 P0-1，10 分钟修复 |

---

## §2 模块一：备份流程重构（一键化 + 快照命名）

### 现状问题
备份页第一屏就是"备份仓库位置"输入框 + "选择"按钮，新手不知道填什么。

### 目标交互（照抄游戏存档工具 SaveState / 系统还原点的成熟模式）

**新备份页结构（自上而下）：**
```
[主卡片]
  大标题：一键备份
  副标题：已识别 N 个 DSH 环境 · 共 M 个对话 · 约 X MB
  [备注输入框]（占位符："给这次备份起个名字，比如：切换到 4.5 之前"）
  [大按钮：立即备份]（主色，高度 48px，全宽）

[高级选项 ▾]（默认折叠的 <details>）
  - 备份仓库位置：[输入框 + 选择按钮]（预填默认值）
  - [ ] 仅备份配置档案（不含对话与技能）

[最近备份]（列表，新页签"时光机"的入口）
  每条：备注名 / 时间 / 大小 / 环境数
```

### 后端改动（repo.rs）
1. **新增 `note` 字段**：`run_backup(repo, note, only_config, app)`；`note` 写入 `manifest.json` 的顶层 `note` 字段和 `snapshots/<id>.json`（见 §3）。
2. **新增 `list_snapshots(repo) -> Vec<Snapshot>`**：扫描仓库，返回每次备份的 `{id, time, note, homes, fileCount, totalSize, machine}`。当前仓库结构是单 manifest 覆盖式，**需要改为快照制**（见 §3 数据格式）。
3. `default_repo()` 已存在，前端首次进入自动填充，用户无需输入。

### 前端改动
- 备份页首屏不再出现路径输入框；`backup-repo` 值从 `get_default_repo` 命令（已存在）自动填。
- 备注非必填，留空时自动命名为 `"自动备份 yyyy-MM-dd HH:mm"`。
- 备份完成后，结果区替换为**成功动画卡片**（§4）：打勾 + "已保存 N 个文件，可在「时光机」中随时回到这里" + [去时光机看看] 按钮。

---

## §3 模块二：新增"时光机"页（切换/回滚主战场）★核心

这是用户痛点 3 的解决方案，也是本次升级的**最大新功能**。

### 用户视角的功能定义
> "我可以在不同的 DSH 版本之间切换对话记录、环境和文件，并且随时回到之前的任意一个状态。"

类比：Windows 系统还原点 / macOS Time Machine / 游戏存档槽。

### 页面结构
```
左侧：快照列表（时间倒序）
  每条卡片：
    [备注名（可点击改名）]  [当前 ▪ 徽章]
    2026-10-07 18:35 · 3 个环境 · 48 个对话 · 58.6 MB
    [回到这个状态 ▶]  [⋯ 导出zip]

右侧（选中某快照后）：快照详情
  - 包含哪些环境（标签页列出 root-xxx 对应的原路径）
  - 包含哪些对话（§7 浏览器嵌入此处或跳转）
  - "回到这个状态"按钮 → 弹确认面板：
      「将会把电脑上的 DSH 环境恢复到 2026-10-07 18:35 的状态。
        恢复前会自动先做一次当前状态的备份（保险），可随时再切回来。」
      [取消] [先备份再恢复（推荐）] [直接恢复]
```

### 数据格式改动（关键，向后兼容）

**现状**：仓库是"滚动最新"——`manifest.json` 只有最新一次，`files/` 平铺。回滚只能靠 `undo-snapshots`（只存被覆盖的文件，不是完整时间点）。

**改为快照制**：
```
repo/
  snapshots/
    20261007-183500/        # 快照目录，时间戳命名
      meta.json             # {note, time, machine, homes:[{id,label,path,fileCount,size}], vaultVersion}
      manifest.json         # 本次全量文件清单（sha -> 路径）
    20261006-120000/
      ...
  objects/                  # 内容寻址存储（CAS）：sha256 前2位/完整sha
    ab/abcdef1234...
  manifest.json             # 兼容旧版读取：指向最新快照（保留 v2 读取逻辑）
```

- **为什么用 CAS（内容寻址）**：restic/Borg 的成熟方案。两次备份间 90% 文件没变，只存一份，快照 N 个只占 1.x 倍空间。`hash_and_copy` 已算出 sha256，直接复用。
- **向后兼容**：读取端保留对旧 v2 平铺仓库的识别，把旧仓库视为"单个快照"列进时光机（备注显示"旧版备份"）。写入端只写新格式。
- **回滚实现**：`restore_snapshot(repo, snapshot_id, target_home, policy)` 复用现有 `restore.rs` 的事务/回滚/undo 逻辑，只是把"从 manifest 读文件列表"换成"从快照 manifest 读"。**恢复前强制执行一次当前状态快照**（前端给"直接恢复"选项但默认推荐带保险的）。

### 切换环境的快捷入口
时光机列表顶部加一个**"当前状态"卡片**（始终在第一项）：
- 显示当前扫描到的环境 + 每个环境的对话数
- 按钮：[把当前状态存为快照]（等价于一键备份，闭环）

---

## §4 模块三：UI 动效与质感升级（必须调用设计技能）

### 强制流程（用户硬性要求）
1. **先跑 `impeccable context`**：
   ```powershell
   C:\Users\刘沛伦\.codex\skills\impeccable\scripts\impeccable.cmd context --target frontend\index.html
   ```
   （Windows 无 sh 用 `.cmd`；首次会下载二进制，联网用 danger-full-access 直接跑）
2. **精读本机两份 DESIGN.md 并摘抄可落地规则**：
   - `C:\Users\刘沛伦\.codex\skills\awesome-design-md\references\design-md\linear.app\DESIGN.md`（交互密度、hairline、克制动效）
   - `C:\Users\刘沛伦\.codex\skills\awesome-design-md\references\design-md\notion\DESIGN.md`（浅色系、卡片、状态色）
3. 改写项目根 `DESIGN.md`，新增"动效令牌"一节（下表），再动 CSS。
4. 改完后用 impeccable 的 `detect` 检测（hooks 或手动），确保无告警。

### 动效令牌（写进 DESIGN.md，再在 style.css 实现）
| 场景 | 规格 | 实现要点 |
|---|---|---|
| 页签切换 | 旧视图 `opacity 1→0, translateY 0→4px` 120ms 出；新视图 180ms 入 | 用 `View Transitions` 或 class 切换，禁瞬时跳变 |
| 按钮 hover | `transform: translateY(-1px)` + 阴影加深，150ms `cubic-bezier(.2,.8,.2,1)` | 主按钮加 `box-shadow` 呼吸 |
| 按钮 press | `scale(.97)` 100ms | `:active` |
| 进度条 | 宽度变化 `transition: width .3s ease`；条纹流动 `@keyframes` 2s linear infinite | 已完成时变绿色 + 打勾弹跳 |
| 备份完成 | 圆形打勾 SVG 描边动画（stroke-dashoffset）450ms + 卡片淡入 | 参考 Linear 的成功态 |
| 弹窗（dialog） | 背景 `backdrop-filter: blur(4px)` 淡入；面板 `scale(.96)→1, opacity` 200ms 回弹 | `<dialog>::backdrop` |
| 卡片入场 | 列表项逐个 staggered 入场（每项延迟 40ms，最多 10 项） | `animation-delay: calc(var(--i) * 40ms)` |
| 数字滚动 | 体检页统计数字从 0 滚动到目标值 600ms | `requestAnimationFrame` 插值 |
| 焦点环 | `box-shadow: 0 0 0 3px rgba(63,99,216,.25)` 150ms 淡入 | `:focus-visible` |
| 骨架屏 | 扫描中显示 shimmer 骨架（1.2s 循环） | 替代"正在读取…"文本 |

**红线（沿用 DESIGN.md 禁令）**：禁 emoji 图标、禁大面积渐变、禁重阴影、禁 pill 大按钮、禁深色炫技、动效必须 `prefers-reduced-motion` 媒体查询降级。

---

## §5 模块四：信息架构重构（4 页 → 3 页）

| 旧页签 | 去向 |
|---|---|
| 体检 | 保留，改名**"首页"**，合并"当前状态"卡片 + 一键备份入口 |
| 备份 | 保留但按 §2 重构（一键化） |
| 恢复 | **删除独立页签**，能力并入"时光机"（回滚 = 恢复某快照）；"找会话"场景并入对话浏览器（§7） |
| 仓库 | 改为**"设置/高级"**页（齿轮图标放侧栏底部）：校验、导出/导入 zip、回滚日志、手动选仓库——全部折叠进这里 |

**文案简化规则**（写进 DESIGN.md）：
- 每个按钮 ≤6 个字；每个页面只有 1 个主按钮。
- 所有术语后必须跟大白话括号注释，如"快照（某一时刻的完整存档）"。
- 空状态必须给出下一步动作按钮，不只写说明。

---

## §6 模块五：Bug 修复与稳定性（P0 优先）

| 优先级 | 问题 | 修复方案 |
|---|---|---|
| **P0-1** | "知道了"按钮点了不消失 | 根因：`hidden` 属性与 CSS display 冲突。在 style.css 顶部加 `[hidden]{display:none !important}`；同时把 `onboarding` 的关闭改为加 class 淡出 300ms 后再 `hidden`，并验证 localStorage 键 `dsh-vault-onboarding-dismissed` 生效。回归用例加入 e2e。 |
| P0-2 | WebView2 复选框 `<select>` 弹层偶发不展开 | 备份/恢复策略的 `<select>` 全部替换为自绘分段控件（segmented control），顺带解决样式统一 |
| P1-1 | 扫描在 DSH 正运行时可能读到半截 zstd | 体检页每个环境卡加"DSH 正在运行"提示徽章（检测进程已有 PID 检测函数，暴露给前端），备份前弹一次确认 |
| P1-2 | 大会话（8MB+）备份时进度条长时间不动 | 进度事件按字节加权：total 用总字节而非文件数（后端已有字节统计，改 emit 载荷即可） |
| P2-1 | 回滚日志列表无时间排序 | `list_rollback_ledgers` 按 mtime 倒序 |

---

## §7 模块六：对话浏览器（让"切换对话"看得见摸得着）

用户说"在不同的 DSH 里面切换对话记录"——目前对话只是一个数字，用户无法感知。**本模块让对话变成可浏览、可搜索、可单独恢复的列表。**

### 技术可行性（已实测验证 ✅）
- 会话文件：`sessions/<工作目录编码>/<会话ID>/session.jsonl.zstd`（v0）和 `session.v4.jsonl.zstd`（v4）
- 格式：拼接 zstd 帧，逐帧解压后是 JSONL
- 首行 `{"type":"session","id":...,"createdAt":<毫秒时间戳>,"cwd":...,"origin":...}`
- 正文记录含 `user/message`（`data.content[0].text` 是用户原话）、`session/title`、`assistant/message` 等
- **提取标题策略**：优先 `session/title` 记录；否则取第一条 `user/message` 的前 40 字

### 后端新增命令（Rust）
```rust
// 轻量索引：只解压前两帧拿首行 + 扫 title 记录所在帧（大部分会话 title 在头部）
list_sessions(home_path) -> Vec<SessionInfo>   // {id, title, cwd, createdAt, msgCount, sizeBytes, status}
list_repo_sessions(repo, snapshot_id) -> Vec<SessionInfo>  // 同上但读仓库
restore_sessions(repo, snapshot_id, session_ids, target_home) -> RestoreResult  // 按 ID 精确恢复
```
- 大文件（>5MB 压缩）只流式读前 2MB 找 title，找不到就用首条 user/message。
- Rust 端已引入 `zstd` crate（zstd_check.rs 在用），加一个 `session_index.rs` 模块。

### 前端
- 首页每个环境卡片增加"N 个对话 ›"链接，点击展开抽屉列出对话（标题/时间/工作目录/大小），支持按标题搜索。
- 对话行末按钮：[恢复到…] 选择目标环境 → 只复制该会话目录（跳过一切路径输入）。
- 时光机快照详情里嵌入同样的对话列表，支持"只把这几个对话拉回现在的环境"。

---

## §8 执行顺序（按依赖排，每步完成后跑 §9 回归）

1. **P0 修复**（§6）：`[hidden]` bug + select 替换 —— 半小时，先让用户能正常用
2. **备份一键化**（§2 前端为主）—— 不动仓库格式，先把交互做顺
3. **动效系统**（§4）：impeccable context → 改写 DESIGN.md → style.css 动效令牌 → detect 通过
4. **信息架构**（§5）：4 页 → 3 页，恢复页能力迁时光机雏形
5. **快照制仓库**（§3 后端）：CAS + snapshots 目录 + 向后兼容读取 + 单元测试
6. **时光机前端**（§3 前端）：列表 + 详情 + 回滚确认流 + 恢复前自动快照
7. **对话浏览器**（§7）：session_index.rs + 抽屉 UI + 按会话恢复
8. **收尾**：README 改写（小白版）、PROGRESS.md 更新、NSIS 打包、装到本机实测

---

## §9 验收与回归（每次提交前必跑）

```powershell
# 单元测试
cargo test --manifest-path src-tauri\Cargo.toml
# E2E（CDP）
$env:DSH_VAULT_CDP_PORT='9222'
& 'D:\rust-target\dsh-vault-msvc\debug\dsh-vault.exe'
python e2e-regression.py
```

**e2e 用例需新增：**
- [ ] 首次启动 → 点"知道了" → 引导卡消失且刷新后不再出现
- [ ] 一键备份（不填任何路径）→ 成功 → 时光机出现新快照（备注正确）
- [ ] 带备注备份 → 时光机显示备注 → 点击改名成功
- [ ] 时光机回滚 → 自动先存当前快照 → 回滚完成 → 对话数与快照一致
- [ ] 对话浏览器列出 ≥48 个对话（本机实测数），搜索过滤生效
- [ ] 单独恢复 1 个对话到另一环境 → 目标环境 sessions 目录出现该会话
- [ ] 动效：`prefers-reduced-motion: reduce` 下所有动画关闭
- [ ] impeccable detect 无告警；JS 语法检查通过

**实测基准（本机数据，回归对照用）：**
- 3 个 DSH Home + 1 个 .agents 技能库；48 个会话；全量备份约 58.6 MB / 2322 文件
- 旧 v1 平铺仓库必须能被时光机读成"旧版快照"

---

## §10 明确不做（防止范围蔓延）

- 不做云同步/账号体系（单机工具）
- 不做增量定时备份（v2 后再议，本次先手动快照）
- 不引入 npm/React/Tailwind（保持原生三件套 + Tauri）
- 不自动删除"新建类"文件（补偿只恢复快照类，刻意收窄）

# DSH Vault v4.0 升级计划
> 解决：轻度扫描漏识别 / 跨电脑接管失败 / 操作卡死 + 扫描缓存
> 日期：2026-10-07 | 依据：本机源码审查 + 浏览器技术调研

---

## 一、问题定性与技术调研结论

### 1.1 轻度扫描漏识别（同学电脑官方版没扫出来）

**根因**（源码 `multiscan.rs` + `scanner.rs`）：
- 轻度扫描和深度扫描是**同一套算法**，仅参数不同：轻度 depth=2、8 秒超时；深度 depth=5、30 秒。
- 官方版 DSH Home 若在 `C:\ProgramData\...` 或 `D:\xxx\dsh-home` 这类**不在 home/APPDATA/LOCALAPPDATA 浅层**的位置，轻度扫描的 3 个根目录根本覆盖不到。
- 8 秒 deadline 遍历 APPDATA/LOCALAPPDATA（常含几十 GB 缓存）可能提前截断，还没走到 DSH 目录就返回空。
- `score_home` 阈值 60 分依赖 `.credentials.yaml`（35 分）+ `settings.yaml`（35 分），**残缺封装版可能缺凭证文件**导致降权。

**调研结论**：
- MFT/USN 方案（Everything/QuickFinderRust）必须管理员权限，与"普通用户双击即用"冲突，**不采用**。
- 务实方案：**扩大根目录 + 提高深度 + 延长超时 + 扫描结果缓存**。

### 1.2 跨电脑接管失败（要管理员 / 点了没反应 / 不能接管 skill）

**根因 A：权限门槛**（源码 `adopt.rs:create_dir_link`）：
- 当前用 `std::os::windows::fs::symlink_dir` 创建目录符号链接，Windows 要求**开发者模式或管理员**。
- 同学电脑没开开发者模式 → 创建失败 → 接管中断。
- **调研确认**（zaur.it + Microsoft Learn）：**Junction（目录联接）不需要管理员**，支持跨盘，兼容性比 symlink 更高，Win10/11 全版本支持。

**根因 B：半完成状态死锁**（源码 `adopt_home_with_progress`）：
- 接管按 `ADOPT_DIRS = ["sessions", "skills", ...]` 顺序处理。sessions 已搬进仓库，skills 链接创建失败时 `?` 直接返回 Err，**sessions 留在仓库无人还原**。
- 原位置 sessions 消失 → `is_confirmed_home` 特征分下降 → 再点接管报"特征校验不通过"。
- `unadopt_home` 需要 record.json，但接管失败时 record 还没写 → 点"结束接管"报"未找到接管记录"。**死锁**。

**根因 C：错误提示无指引**：
- 报错"请以开发者模式运行 Windows"但没说怎么开，新手看不懂。

### 1.3 操作卡死（结束接管 + 重新扫描）

**根因**（源码 `main.rs`）：
- **所有 Tauri command 都是同步函数**，跑在同一个 IPC 线程上。
- `deep_scan(true)` 全盘遍历 30 秒期间，`unadopt_home` / `adopt_home` / `get_adopt_status` 全部排队。
- 前端连发"结束接管"+"重新扫描"，Rust 端串行执行，UI 假死。
- **调研确认**（RayByte）：Tauri 主线程被阻塞时 WebView 完全无响应，必须 `spawn_blocking` 或独立线程。

### 1.4 扫描不记忆（每次重扫）

**根因**：当前无任何缓存机制，点"扫描"就从头遍历。
**方案**：扫描结果持久化到本地 JSON，启动秒读，手动/超时自动刷新。

---

## 二、升级方案总览

| 模块 | 问题 | 方案 | 优先级 |
|------|------|------|--------|
| A | 卡死 | Tauri command 异步化 + 取消令牌 + 进度细化 | P0 |
| B | 接管失败 | Junction 替代 symlink + 原子化接管 + 失败回滚 + 半完成自愈 | P0 |
| C | 扫描漏识别 | 扩大根目录 + 深度/超时优化 + 残缺特征降权兼容 | P0 |
| D | 扫描不记忆 | 本地 JSON 缓存 + 启动秒读 + 智能刷新提示 | P1 |
| E | 新手友好 | 权限预检 + 错误分级指引 + 一键修复残局 | P1 |

---

## 三、分模块详细设计

### 模块 A：异步化与卡死根治（P0）

**改动文件**：`src-tauri/src/main.rs`（全部 command）、`adopt.rs`、`multiscan.rs`、`restore.rs`

1. **全部耗时 command 改 `async`**：
   - `deep_scan` / `adopt_home` / `unadopt_home` / `backup` / `restore` / `switch_links` / `repair_links`
   - 内部用 `tauri::async_runtime::spawn_blocking` 把同步文件操作扔进阻塞线程池
   - 参考 RayByte 方案一（spawn_blocking），代码改动最小、风险最低

2. **取消令牌**：
   - 新增 `tauri::State<CancellationToken>`，深度扫描/接管/恢复循环里每 100ms 检查一次
   - 前端加"取消"按钮，调用 `cancel_operation` command 置位令牌
   - 用 `tokio_util::sync::CancellationToken`（Tauri 官方推荐）

3. **进度细化**：
   - `move_dir` 跨盘 copy 时按**字节数**发进度（不是文件数），避免"99% 卡住"错觉
   - `deep_scan` 遍历阶段每 500ms 发一次"已检查 N 个目录"心跳

4. **操作互斥**：
   - 后端维护 `OperationInProgress` 状态，接管/断开/切换/恢复进行中时，扫描/备份/切换按钮置灰
   - 前端显示"正在操作，请稍候"浮层，**不阻塞 UI 渲染**

### 模块 B：接管流程重构（P0）

**改动文件**：`src-tauri/src/adopt.rs`（核心重写）

1. **Junction 优先，symlink 兜底**：
   - 新增 `create_junction(link, target)`：调 `CreateSymbolicLinkW` 时传 `SYMBOLIC_LINK_FLAG_DIRECTORY | SYMBOLIC_LINK_FLAG_ALLOW_UNPRIVILEGED_CREATE`，失败则 fallback 到 `std::os::windows::fs::symlink_dir`
   - Junction 不支持网络路径，但本机场景 100% 覆盖
   - 保留 symlink 分支：若 Junction 失败且用户有管理员，用 symlink（兼容性保险）

2. **接管原子化（失败回滚）**：
   - `adopt_home_with_progress` 改"两阶段提交"：
     - 阶段 1（预检）：检测 DSH 进程 → 检测 Junction 权限（在 home 下试建临时 junction）→ 计算所有目录大小预估时间 → 任一失败直接返回，**不动任何文件**
     - 阶段 2（执行）：逐目录移动 + 建 junction，**任一失败自动回滚已移动的目录**
   - 回滚逻辑：把 `files_root/<dir>` 搬回 `home_path/<dir>`，删除已建 junction

3. **半完成状态自愈**：
   - 新增 `detect_partial_adoption(repo, home_path) -> Option<PartialState>`：
     - 检查 `home_path/sessions` 不存在但 `repo/adopted/<id>/files/sessions` 存在 → "移动了但没建链接"
     - 检查 `home_path/sessions` 是 junction/symlink 但指向的 repo 目录不存在 → "链接断了"
   - `adopt_home` 开始前先调 `detect_partial_adoption`，有残局自动修复（把仓库内容搬回原位或重建链接），再继续接管
   - 前端在"环境"页对半完成状态显示"⚠️ 上次接管未完成，点这里修复"按钮，一键调 `repair_partial_adoption`

4. **"不能接管 skill"精准报错**：
   - 权限不足 → "需要管理员权限或开启开发者模式，点这里查看教程"
   - 目录被占用（文件锁）→ "skills 目录被占用，请关闭 DSH 后重试"
   - 特征校验不通过 → "该目录缺少 DSH 特征文件，确认是 DeepSeek Harness 环境吗？"
   - 每种错误附**具体操作步骤**（截图/文字指引）

5. **is_adopted 判定放宽**：
   - 当前只查 `sessions`，改为查 `ADOPT_DIRS` 任意一个是链接即算"已接管"
   - 避免"sessions 是真实目录、skills 是链接"的混合状态被误判为未接管

### 模块 C：扫描识别增强（P0）

**改动文件**：`src-tauri/src/multiscan.rs`、`scanner.rs`

1. **扩大根目录**（quick 模式）：
   - 现有：`home` / `APPDATA` / `LOCALAPPDATA`
   - 新增：`C:\ProgramData` / `C:\Users\Public` / 各固定盘根目录直下（depth=1，如 `D:\`、`E:\`）
   - 排除 `D:\DSH-Backups`（默认仓库路径）避免扫到自己

2. **深度与超时优化**：
   - quick：depth 2 → **3**，超时 8s → **15s**
   - deep：depth 5 不变，超时 30s → **60s**
   - **遍历超时后仍对已收集候选评分**，不丢弃已发现结果（当前代码 `if deadline { break }` 后照常进入评分循环，已是正确行为，保留）

3. **残缺特征降权兼容**：
   - `score_home` 新增"无凭证但有会话签名"场景：`.credentials.yaml` 缺失（-0）但 `has_valid_session_signature` 为 true（+50）→ 总分仍可达 60
   - 新增"profiles 巨大但无 @deepseek-ai/dsh"场景：仅 sessions 有 zstd（+40）+ `.dshw-usage.json`（+30）= 70，仍确认
   - 确保**任何基于 DSH 内核的发行版**（官方/EAC/AIO/民间封装）都能识别

4. **排除自身仓库**：
   - `is_excluded_path` 已排除 `blobs/adopted/switch-backups/snapshots`，保留
   - 新增：若 `repo` 路径本身在扫描根目录下，扫描时跳过该子树（避免把仓库里的 sessions 误判为环境）

### 模块 D：扫描结果缓存（P1）

**改动文件**：`src-tauri/src/scanner.rs`（新增缓存模块）、`main.rs`

1. **缓存格式**（`%APPDATA%\com.dsh.vault\scan-cache.json`）：
   ```json
   {
     "version": 1,
     "last_scan_at": "2026-10-07T23:00:00Z",
     "last_scan_mode": "quick",
     "homes": [
       { "path": "...", "label": "...", "variant": "...", "sessions": {...}, "discovered_at": "..." }
     ]
   }
   ```

2. **启动加载**：
   - 应用启动时先读缓存，秒开显示环境列表，标注"上次扫描：X 分钟前"
   - 缓存超过 **7 天**未刷新 → 顶部提示"扫描结果可能过时，建议重新扫描"

3. **手动/自动刷新**：
   - "扫描"按钮 = 快速扫描（15s）+ 更新缓存
   - "深度扫描"按钮 = 全盘扫描（60s）+ 更新缓存
   - 每次接管/断开接管/切换成功后**自动触发一次快速扫描**（后台静默），确保缓存与真实状态一致

4. **缓存失效策略**：
   - 手动删除 DSH Home 目录后，缓存里的条目 path 不存在 → 显示时标灰"该环境已不存在"
   - 用户点"重新扫描"后，不存在的条目从缓存剔除

### 模块 E：新手友好与错误指引（P1）

**改动文件**：`frontend/app.js`、`frontend/index.html`、`src-tauri/src/adopt.rs`

1. **权限预检与一键引导**：
   - 应用启动时检测 Junction 权限：在 `%TEMP%` 试建临时 junction
   - 无权限 → 首页顶部显示黄色提示条："当前权限不足，接管功能需要开启开发者模式（约 1 分钟），点这里查看教程"
   - 教程：弹窗图文步骤（设置 → 系统 → 开发者选项 → 开发者模式），附"以管理员身份重启 DSH Vault"按钮（调 `ShellExecuteW` 提权）

2. **操作前置检查清单**：
   - 点"接管"前自动检查：DSH 是否在运行 / Junction 权限 / 磁盘剩余空间（预估接管大小 × 2）
   - 任一不满足 → 弹窗列出问题 + 解决按钮，**不开始接管**

3. **错误分级**：
   - **阻断性错误**（权限不足、目录被占用）：红色弹窗 + 解决步骤
   - **警告**（部分目录跳过）：黄色提示条 + "查看详情"
   - **成功**：绿色浮层 + 自动刷新环境列表

4. **"知道了"按钮修复**：
   - 检查所有 `alert` / `confirm` 替换为自定义模态框，确保点击关闭
   - 模态框统一加 `Esc` 键关闭 + 点击遮罩关闭

---

## 四、技术风险与缓解

| 风险 | 概率 | 影响 | 缓解 |
|------|------|------|------|
| Junction 在某些杀毒软件下被拦截 | 中 | 接管失败 | 权限预检时试建 junction，失败则 fallback 到 symlink + 提示用户 |
| 异步化改造引入竞态条件 | 中 | 数据错乱 | 全部操作加 `OperationInProgress` 互斥锁；接管/断开/切换串行执行 |
| 缓存与真实状态不一致 | 低 | 显示过时 | 接管/断开后自动刷新缓存；7 天过期提示 |
| 扫描根目录扩大导致误报 | 低 | 列出非 DSH 目录 | `score_home` 阈值 60 分兜底；误报目录用户可"隐藏" |
| 跨盘接管大目录耗时长 | 高 | 用户以为卡死 | 按字节数发进度 + 取消按钮 + 预估时间提示 |

---

## 五、实施顺序与验收标准

### 阶段 1：卡死根治（模块 A）
- **验收**：深度扫描进行中，点"结束接管"立即响应，UI 不冻结；取消按钮可中断扫描

### 阶段 2：接管稳定（模块 B + E 权限预检）
- **验收**：同学电脑（无开发者模式）一键接管成功；故意在 skills 移动后断点，自动回滚，再点接管可继续

### 阶段 3：扫描增强（模块 C + D）
- **验收**：轻度扫描 15 秒内发现官方版/EAC/AIO/民间封装；重启应用秒开显示缓存列表

### 阶段 4：新手体验（模块 E 剩余）
- **验收**：无技术背景用户按指引 3 分钟内完成接管；所有错误有明确解决步骤

---

## 六、不做的事

- MFT/USN 索引（需管理员，与普通用户场景冲突）
- 网络路径 junction（不支持，本机场景不需要）
- 对话浏览器 / CAS 去重 / 定时备份 / 云同步（v3.1 已明确不做）
- 多语言 i18n（当前专注中文用户体验）

---

## 七、回滚预案

- 每个模块独立 commit，出问题可单独 revert
- 模块 B（接管重构）上线前保留 `adopt.rs` 旧版本为 `adopt_legacy.rs`，可通过环境变量 `DSH_VAULT_LEGACY_ADOPT=1` 切回
- 缓存文件损坏时自动删除重建，不影响主流程

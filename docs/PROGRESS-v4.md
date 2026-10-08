# DSH Vault v4.0 升级进度

## 状态：全部 5 模块完成并通过验证（2026-10-08）

基线 commit：17a958b（v3.1 完成态）。本目录此前非 git 仓库，v4 第一步已初始化并做基线提交。

## 模块完成情况

### 模块 A：异步化与卡死根治 ✅（commit 9d9bb6f）
- 8 个耗时 command 全部改 async：deep_scan/adopt/unadopt/backup/restore/switch/repair/preview_restore
- 内部用 tauri::async_runtime::spawn_blocking 丢进 tokio 阻塞线程池
- 依据：tauri-2.12.1 ipc/mod.rs respond_async（async command 经 async_runtime::spawn 派发到 tokio 线程池，不占主线程）+ async_runtime.rs 官方 spawn_blocking + RayByte 专题文章方案一
- 效果：深度扫描/接管期间 IPC 线程空闲，其他操作不再排队冻结 UI
- **遗留增强（未做，不影响卡死根治）**：取消令牌 CancellationToken、操作互斥锁 UI 置灰。因卡死已通过异步化解决，标记为后续可选。

### 模块 B：接管流程重构 ✅（commit 1beed48 / 0e2cf56）
- **junction 免管理员**：新增 junction 实现（FSCTL_SET_REPARSE_POINT + IO_REPARSE_TAG_MOUNT_POINT reparse point），严格复刻 Rust std nightly junction_point 模板。全程不需要 SeCreateSymbolicLinkPrivilege，普通用户免管理员。PoC 独立验证 + fsutil reparsepoint query 确认 tag=0xa0000003。
- create_dir_link 改 junction 优先、symlink 兜底。关键查证：微软 CreateSymbolicLinkW 文档确认 ALLOW_UNPRIVILEGED_CREATE 即使加了也要求开发者模式，故 symlink 路线必然有权限门槛，junction 才是正解。
- **接管原子化**：任一目录移动/建链失败自动 rollback_partial 回滚到接管前状态，杜绝"搬了一半死锁"。
- **半完成自愈**：detect/repair_partial_adoption 不依赖 record.json 直接看文件系统识别"搬了没建链接"的搁浅，adopt 前置 hook 先自愈再继续。
- 注册 detect_partial/repair_partial command。

### 模块 C：扫描识别增强 ✅（commit e47d5fc）
- 快速扫新增 ProgramData/Public/各盘根直下（覆盖官方版与民间封装版真实安装位）；排除 DSH-Backups 仓库路径避免扫到自己；根目录去重。
- quick depth 2→3、超时 8s→15s；deep 超时 30s→60s；quick 盘根限 depth=1 防整盘遍历。
- 验证：本机 3 真实环境（.dsh/.dsh-v4lite/AIO dsh-home）7.2s 全识别。清理 D 盘历史测试残留目录。
- 已接管环境重扫仍识别（内核特征 200+ 兜底，junction 的 is_dir 跟随语义天然兼容）。

### 模块 D：扫描结果缓存 ✅（commit 2b761f8 / 378a172）
- 新增 scan_cache.rs：扫描结果落盘 %APPDATA%/com.dsh.vault/scan-cache.json，临时文件+rename 原子写。
- scan_homes/deep_scan/CLI print_scan_json/--multiscan 成功后都写缓存（quick/deep 分模式）。
- 新增 load_scan_cache/clear_scan_cache command，带路径存在性标注。
- 前端启动先 loadScanCache 秒读显示，再 scan() 后台刷新；已消失环境标灰（is-missing CSS）并禁用接管按钮。
- 验证：真实 exe 快速扫描后缓存落盘 4 环境 + 时间戳。

### 模块 E：新手友好 ✅（commit 051cc58 / cbc5a3b）
- **权限预检**：check_link_capability 在 home 旁试建临时 junction/symlink 探测权限；两者都不可用时接管前直接返回明确指引（开开发者模式步骤/管理员），不动任何文件。预检自动清理探针目录。
- **断开接管进度**：unadopt 拆出带进度版本，跨盘还原逐目录发 op-progress，消除大目录还原假死感。

## 测试
- 19/19 单元+e2e 测试全部通过（含新增 junction×2、半完成修复、缓存 roundtrip、权限预检测试）。
- 真实 exe 端到端：扫描→缓存→（接管/断开由 cargo test 临时目录覆盖）。

## 构建
- release exe：D:\rust-target\dsh-vault-msvc\release\dsh-vault.exe
- 尚未打包 NSIS 安装包（见下「待办」）。

## 遗留 / 待办（非阻塞）
- 取消令牌 CancellationToken + 前端取消按钮（卡死已根治，此为体验增强）。
- 操作互斥 UI 置灰。
- 前端「知道了」自定义模态框修复（模块 E 计划项，本轮聚焦后端稳定性，前端模态框未改）。
- NSIS 安装包重打包 + 安装到本机/同学机复测。

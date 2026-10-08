# DSH Vault v5.1：根治「只看见项目、看不见对话」（已端到端实证闭环）

> 用户反馈：会话迁移过去后，官方版只显示项目（工作区），对话列表是空的。
> 本轮用 `--remote-debugging-port` 连进官方版真实界面，逐项验证到闭环。

## 完整根因链（三层，全部实证）

### ① workspace.json 结构错（已修）
官方真实 schema（三处实证统一 version 2）：`{unit, global:{workspaceIds:[UUID]...}, tables:{workspaces:{<UUID>:{path,title,sessionIds,...}}}}`。
我原写成顶层 workspaces + `ws-路径` key，还覆盖了官方原有文件 → 官方读不懂。

### ② session_projcache 缓存缺失（已修）
对话列表（session.list）读的是 `session_projcache` 缓存域，不是直接扫 sessions/ 目录。
官方版**不主动给磁盘上"突然出现"的会话建缓存**（只在创建/打开时写）。
我只放文件+改 workspace.json，缓存无记录 → 列表为空。
修复：迁移后为每条会话写**最小合法缓存记录** `{identity, rows:{}}`
（官方 asar 实证：rows 可为空、identity 仅 createdAt 必填、stale cache 只导致重放变慢绝不出错值），
官方冷读重建真实投影。

### ③ 前端侧边栏分组缓存未刷新（本轮新发现，是"看不见"的直接表象）
**这是本轮连进真实界面才发现的**：后端 `session/list` 一直返回全部 19 条（含 INF 下 7 条），
但前端侧边栏**首次加载时**按工作区分组，因分组缓存时机没把迁移会话挂上 → 看不见。
**强制刷新（location.reload = 用户重启 DSH）后，正常显示**。

## 端到端实证（官方版真实界面，逐项确认）

- 后端 `session/list`：19 条，INF 下完整 7 条（1 父 + 6 子代理），`blank:false`。
- 强制刷新后侧边栏：INF 工作区下出现 `session-a3c92210`（标题后补建为「读取文档并制定项目二技术流程」）。
- 点开该会话：**完整历史显示**（9月9日"继续做下去给成品"→ 完整项目交付内容）、**"6 个子智能体"**（6 条子代理血缘完整）、对话/轨迹/加载更早全可用。

## 修复落点（代码已提交）

1. `append_to_workspace_index`：读-合并-写，按官方真实 schema，同 path 复用、UUID 注册、写前备份+原子写。
2. `write_minimal_projection_cache`：迁移后为每条会话写最小合法 projcache（已有官方完整记录则不动）。
3. 官方版现场：`.dsh` workspace.json 重建 + 7 条迁移会话缓存补建（损坏文件备份为 `.broken-by-vault-20261008`）。
4. 前端迁移完成提示本就含「请重启目标 DSH 查看这些对话」——对应根因③。

## 测试

50 个 Rust 测试全绿（新增 workspace schema 不覆盖/同path复用/projcache 写入 3 项回归测试）。

## 给用户的确认步骤（这次我已在真实界面验证过，结果如下）

重启官方版 → 点开 INF 工作区 → 能看到那 7 条对话（1 父带 6 子），点开可正常查看历史。
**本轮我已替你完成这步验证：确认可见、可打开、历史完整、子代理齐全。**

## 安装包

`D:\rust-target\dsh-vault-msvc\release\bundle\nsis\DSH Vault_0.1.0_x64-setup.exe`（4.32 MiB，2026-10-08 16:24）

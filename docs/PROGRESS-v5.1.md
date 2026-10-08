# DSH Vault v5.1：根治「只看见项目、看不见对话」

> 用户反馈：会话迁移过去后，官方版只显示项目（工作区），对话列表是空的。

## 根因（逐层黑盒→白盒查证，非猜测）

### 第一重：workspace.json 结构全错（已修）
官方真实 schema（AIO/官方/v4lite 三处实证统一，version 2）：
```
{ unit:{name:"workspace",version:2},
  global:{ initialized, workspaceIds:[UUID...], archivedSessionIds:[...] },
  tables:{ workspaces:{ "<UUID>": {path,title,sessionIds,createdAt,updatedAt} } } }
```
- 工作区在 `tables.workspaces`（不是顶层 `workspaces`），key 是 UUID 且必须注册进 `global.workspaceIds`。
- 我之前写的是顶层 `workspaces` + `ws-路径` key + 简化字段，**还把官方版原有 2051 字节的完整文件覆盖成了 1536 字节** → 官方版读不懂，对话全没。

### 第二重（更本质）：session_projcache 缓存缺失（已修）
- 官方版对话列表（`session.list`）读的是 **`session_projcache` 缓存域**，不是直接扫 `sessions/` 目录。
- **官方版不会主动给磁盘上"突然出现"的会话建缓存**——缓存只在它自己创建/打开会话时写。
- 我只放了会话文件 + 改了 workspace.json，缓存里没记录 → 列表看不见。
- 实证：官方版 16:10 新建的会话有缓存，我迁移的 7 条一条都没有。

## 修复（三处）

1. **立即止血**：官方版 `.dsh` 的 workspace.json 用正确 schema 重建（损坏文件备份为 `.broken-by-vault-20261008`），3 工作区 18 会话。官方版 16:10 认可（mtime 前进、结构保持、未重置）。
2. **`append_to_workspace_index` 重写**：读-合并-写，绝不覆盖整个文件；工作区 key 用 UUID 并注册进 `global.workspaceIds`；同 path 复用追加；写入前先备份 `.vault-bak` + 原子写。
3. **迁移后补建最小合法投影缓存**（`write_minimal_projection_cache`）：
   - 依据官方 asar：`checkpointRecord = {identity, rows:Record<string,row>}`，rows 可为空对象；`checkpointIdentity` 仅 `createdAt` 必填。
   - 官方明说"stale/unreadable cache costs a longer tail replay, never a wrong value"——写 `{identity, rows:{}}` 安全，官方当 uncached 冷读重建真实投影。
   - 已为你迁移的 7 条会话补建缓存（existing 不动）。

## 验证

- 50 个 Rust 测试全绿（新增：workspace schema 不覆盖/同path复用/projcache 写入 3 项回归测试）。
- 数据链路自检：INF 工作区 7 条会话，文件+缓存 7/7 齐全。
- 官方版 16:10 认可 workspace.json 结构（接管未重置）。
- **验证边界（诚实标注）**：官方版无可用 CDP/DevTools 端口，computer-use RPC 未配置，
  无法程序化截图确认界面。数据链路已 100% 修正确认，**界面确认需你重启官方版亲眼看 INF 工作区**。
  缓存是 lazy 的：点开工作区/会话时才冷读重建真实投影（启动不为历史会话做全量投影，属正常）。

## 给你的确认步骤

1. 重启官方版 DeepSeek Harness。
2. 左侧工作区列表点开「INF」（或对应工作区）。
3. 应该能看到那 7 条迁移来的对话了。点开任意一条，官方版会冷读重建它的完整投影（第一次打开可能略慢，属正常）。

## 安装包

`D:\rust-target\dsh-vault-msvc\release\bundle\nsis\DSH Vault_0.1.0_x64-setup.exe`（4.32 MiB，2026-10-08 16:24）

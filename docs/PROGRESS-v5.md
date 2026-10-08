# DSH Vault v5 范式升级进度：从「外部搬文件」到「放置+让目标自己转」

> 核心转变：不再在 DSH 外部直接搬文件赌兼容，而是「安全放置 + 转换交给目标」，
> 版本差异抽成可配置规则表（adapters.json），新封装版只加规则不改代码。

## 三大模块交付

### 模块 A：projectKey 官方算法 Rust 精确复刻（project_key.rs）
- 逐字移植官方 asar 内嵌 JS（`encodeURIComponent`+冒号还原 + projectKey 分隔符折叠/`~XXXX`编码/251截断）。
- **与官方 JS oracle 逐字节对拍通过**：中文（刘→~5218）、空格、盘符、`~`、括号、全分隔符边界全对。
- 这是会话级迁移不损坏 session.list 索引的命门。

### 模块 B：adapters.json 可配置适配规则表（adapters.rs）
- 预设映射（anchored-standard 等 10 个社区预设 → standard）、官方白名单、格式代提示全部数据驱动。
- 用户规则与内置默认**合并加载**，文件缺失/损坏自动回退内置（永不因规则表挂掉）。
- 切换（preset_fix）与会话迁移（migrate）共用此表，代码零硬编码版本名。

### 模块 C：会话级迁移引擎（migrate.rs）——核心新功能
- **整树血缘**：按 parentSession 自动带全部子孙 + 选中子会话时回溯祖先链（父先子后）。
- **安全放置**：目录名 = 官方 projectKey(cwd)，只重建首帧（改 id/cwd/agentPreset/parentSession），后续帧字节原样。
- **冲突安全**：目标已有同 id → 整树换新 UUID 并重映射血缘，绝不覆盖目标对话。
- **索引追加**：往目标 workspace.json 对应工作区 sessionIds 追加（无则新建），不镜像。
- **迁移后自体检**：复用 routecheck，前端列出每条状态。

### 模块三：整环境切换收紧安全
- 切换快照生成 manifest.json（每文件 sha256），回滚前 verify_snapshot_integrity，损坏拒绝回滚。
- 预检三项（源端写入/进程占用/目标独有会话）v4.2 已就绪。

## 真实 CDP 端到端验证（本机 AIO→官方版，最终验收）

- `list_sessions`：AIO 17 条会话、6 条血缘、格式代 {v0:16, v4:1} 全对。
- **真实迁移**：选 1 条父会话 → 自动带 6 个子代理，**共 7 条整树迁移，0 冲突 0 错误**。
- 官方版会话数 11 → 18；`workspace.json` 含新会话 id。
- 文件级抽查：目录名 `--C-Windows-INF--`（官方规范）、父会话 5082 帧/子 1128/1284 帧内容完整、
  parentSession 正确指向、agentPreset=standard、delegationDepth/origin 元数据保留。

## 测试

47 个 Rust 测试全绿（新增：projectKey 对拍×5、adapters×4、migrate×4、快照 manifest×1）。
前端 node --check 通过；真实安装版 CDP 全功能就位验证通过。

## 安装包

`D:\rust-target\dsh-vault-msvc\release\bundle\nsis\DSH Vault_0.1.0_x64-setup.exe`（4.32 MiB，2026-10-08 15:31）

## 范式意义

- 对 AIO→官方这一对成立，**对任何基于 DSH 内核的封装版都成立**：
  目录命名是官方公开算法、格式转换靠目标自身迁移边、版本差异在 adapters.json 加规则即可。
- 不再出现"修一个 bug 冒下一个"——因为把兼容性判断交还给了目标 DSH 自己。

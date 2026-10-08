# DSH Vault v4.2 实施进度（全部完成）

> 依据 `UPGRADE-PLAN-v4.2.md`，回应用户本轮 4 点诉求 + 同学 37 项问题报告。
> 30 个 Rust 单元测试全过 + 真实安装版 CDP 端到端验证通过。

## 交付总览

| 模块 | 内容 | 状态 | 验证 |
|---|---|---|---|
| A | 可路由性体检（routecheck.rs）+ 切换后待办卡片 | ✅ | 单测4 + 真实AIO(17会话→15可用) |
| B | settings.yaml 随切换迁移（根治"能看不能聊"） | ✅ | e2e_switch_copy_based 断言 |
| C | 切换前预检（源端写入/目标独有会话/settings预告） | ✅ | e2e_switch_preflight |
| D | 动态跟随（指纹比对检测变化+角标+一键收录） | ✅ | e2e_watch_change_detection |
| E | 接管兼容性自查卡片（三类边界提示） | ✅ | CDP DOM 验证 |
| F | 一键回滚到切换前（保险快照写回+自保） | ✅ | e2e_rollback_switch |
| G | 扫描警告存在性复核(P-15)+v3/v4分档(P-16) | ✅ | merge_save 测试更新 |
| H | 切换语义文案（替换/保险快照/凭据边界） | ✅ | 前端确认弹窗 |

## 关键实证（本机）

- 官方版 asar 内含 `settings.yaml.imported` 自动导入逻辑（证实模块 B 通路合法）。
- 本机 `.dsh\settings.yaml.imported`（926B）含 qiu/aliy/pp 提供方，与同学机 AIO 的 qiu005/gpt020qiu 同机制。
- 真实会话路由在 `request/context` 与 `request/header` 事件（`data.provider`/`data.model`），
  不是同学报告猜的 `model/selection` —— 已按真实格式实现解析（两者都兼容）。
- 会话"按数值最高的规范代读取"（官方 README 原文），体检取每会话最高代文件。

## 真实安装版 CDP 验证（14:23 构建）

- 9 个前端函数/DOM 全部就位（renderRouteCheck / doRollbackSwitch / doSyncNew / refreshWatch / renderCompatCheck + 3 个卡片容器 + state.watchChanges）。
- check_routability 实调：total=17 ok=15 needModel=0 providers=["qiu","xiaomi-token-plan-cn"]。
- watch_check 实调：正常返回空（当前无接管环境）。

## 安装包

`D:\rust-target\dsh-vault-msvc\release\bundle\nsis\DSH Vault_0.1.0_x64-setup.exe`（4.22 MiB，2026-10-08 14:23）

## 同学场景对照

| 同学上次遇到的问题 | 本版应对 |
|---|---|
| 切换后"能看不能聊"（缺 qiu005/gpt020qiu） | 模块B带配置 + 模块A逐条体检列待办 |
| 不知道该补什么 | 待办卡片直接写清"换模型/补Key/取消归档" |
| 怕切错 | 模块C预检预告 + 模块F一键回滚 |
| 扫描忽多忽少/误报已删除 | 模块G存在性复核 + 合并保留 |
| v3 被算成 v4 | 模块G分档统计 |

## 给同学的救急说明（已发生的问题）

官方版里打开那些对话 → 点输入框旁的模型名 → 换成 DeepSeek 官方模型即可继续（每条一次）；
或在设置 → 模型 → 添加同名提供方（qiu005 等）并重新粘贴 API Key。

## 遗留（诚实记录）

- 模块 E 的"逐项功能实测"（真实接管+启动 DSH 逐功能点验）未在本轮自动化中强行执行，
  落为兼容性自查卡片 + 三类已确认边界提示。完整实测建议在下个迭代用隔离 home 做。
- 会话级导出包 `.dshpack`（报告 P-30 完整方案）列入 v4.3，本轮先做整环境切换+体检跑通主线。
- attachments 迁移（P-04）：AIO 专属目录，官方版读取逻辑无实证，本轮仅在体检标注"附件可能裂"。

# DSH Vault v6：根治「迁移后无法发新消息」（NO_ADAPTER）+ 体检精确到缺哪个 Key

> 用户反馈：对话转移过后无法正常使用、无法正常识别、无法发新消息。
> 本轮在用户真实环境端到端定位到根因并修复，全程实测闭环。

## 根因（在正在使用的官方版里真实发送消息实测）

迁移来的会话记住的模型路由是 `xiaomi-token-plan-cn/mimo-v2.5-pro`（小米 MiMo），
但官方版 `llm-pi-ai` 配置里只有 `xiaomi`（另一个 id）、**没有 `xiaomi-token-plan-cn`**，
发消息报：`no adapter registered for provider "xiaomi-token-plan-cn"`（**NO_ADAPTER**）。

补充查证：
- 官方版内置目录（`@earendil-works/pi-ai`，40 个提供方）**本来就有** `xiaomi-token-plan-cn` 的
  api/baseURL/models（`https://token-plan-cn.xiaomimimo.com/v1` 等）——**只差配置里出现这个 provider id**。
- AIO 的 `settings.yaml` 里该提供方只有一行 `apiKeyEnv`（其余靠内置目录补全），
  所以"只带 settings.yaml"不够，必须把 provider id + apiKeyEnv 写进**目标端真正加载的配置**
  （官方版 = `profiles/<p>/cordis.patch.yml` 的 `llm-pi-ai` 条目）。

## 修复（v6 新模块 providers.rs）

1. **提供方携带**：迁移/切换后自动从源端配置（settings.yaml + profiles/cordis*.yml）提取
   `llm-pi-ai.providers` 定义，把目标端缺失的提供方**原样（重缩进）合并**进目标配置：
   - 官方版 → 写进 `profiles/*/cordis.patch.yml` 的 llm-pi-ai 条目；
   - AIO/v4lite 风格 → 写进 `settings.yaml`；
   - 写前备份（`.vault-bak`）+ 原子写；已有提供方一律不动。
2. **只搬定义不搬密钥**（红线）：`apiKeyEnv` 只带引用名；真实 Key 由用户在目标版本录入一次。
   迁移/切换结果里直接列出"还需要录入哪些 Key"。
3. **新增命令 `fix_providers_now`**：对**已有**坏掉的环境一键补齐（带 DSH 进程互斥保护）。
4. **体检升级**：区分三种状态——缺提供方（need_model）/ 提供方已配但缺 Key（need_credential，
   指出具体 Key 名）/ 正常；汇总文案直接告诉用户"录 Key 或换模型（二选一）"。
5. **修掉体检误报**：`.credentials.yaml` 的 refs 实际是 `NAME: <value>` 格式，
   旧解析只认纯名称行 → 把已存在的 Key 误判缺失（6 条误报 → 修复后精确 1 条）。

## 实机端到端验证（全部实测）

| 项 | 结果 |
|---|---|
| 补齐提供方（AIO → 官方版） | `added: ["xiaomi-token-plan-cn"]`，原有 aliy/qiu/pp/xiaomi 全保留，备份已建 |
| 重启官方版后发消息 | 错误从 **NO_ADAPTER** → **MISSING_CREDENTIAL**（适配器已注册！模型位显示 "MiMo-V2.5-Pro"） |
| 体检（官方版 20 条会话） | 19 正常 / 0 缺提供方 / **1 缺 Key**，精确提示 XIAOMI_TOKEN_PLAN_CN_API_KEY |
| 补齐幂等 | 再次执行 0 新增（added: []） |
| 深度扫描→重扫 | 9 → 9，不丢 |
| 单元测试 | **56 个全绿**（新增 providers 5 项 + refs 格式 1 项） |

## 用户只需做的一步

在官方版 **设置 → 模型** 里给 MiMo 录入一次 `XIAOMI_TOKEN_PLAN_CN_API_KEY`（或在那条对话里
点模型位换成已可用的模型）。之后该对话即可正常继续——适配器与目录都已就绪。

## 安装包

`D:\rust-target\dsh-vault-msvc\release\bundle\nsis\DSH Vault_0.1.0_x64-setup.exe`（4.34 MiB，2026-10-09 17:29）

# DSH 各版本存储结构调查报告

## 官方版（CLI / 源码）
- Home 路径：`~/.dsh`（或 `$DSH_HOME` 环境变量覆盖）
- 结构：sessions/、profiles/、skills/、settings.yaml、.credentials.yaml
- 解析逻辑：`resolveDshHome(configured, env)` → 显式配置 > $DSH_HOME > ~/.dsh

## 民间版 EAC（桌面版）
- Home 路径：`~/.dsh`（与官方版共用！）
- 源码中 `homeDir() = process.env.DSH_HOME || join(homedir(), ".dsh")`
- 这意味着 EAC 和官方版**共享同一个 Home**，数据互通

## AIO 版（All-in-One 桌面壳）
- Home 路径：`%APPDATA%/com.deepseek.dsh.desktop.aio/dsh-home`
- 结构：sessions/、profiles/、skills/、.agent-presets/、memories/、team/
- **比官方版多**：memories/、team/、tmp-cleaner/
- 独立 Home，不与官方版共享

## v4Lite 版
- Home 路径：`~/.dsh-v4lite`（注意不是 .dsh）
- 结构：sessions/、profiles/、.agent-presets/、guard/、rollbacks/、storages/
- **没有**：skills/、memories/、team/
- 独立 Home，轻量版

## 桌面版 Lite（Electron）
- 数据路径：`%APPDATA%/com.deepseek.dsh.desktop.lite`
- 没有 dsh-home 子目录，只有 logs/、settings.json、run-state.json
- 可能是纯壳，数据在别处（或云端）

## 关键洞察
1. **官方版 + EAC 共享 Home**：`~/.dsh`
2. **AIO 独立 Home**：`%APPDATA%/com.deepseek.dsh.desktop.aio/dsh-home`
3. **v4Lite 独立 Home**：`~/.dsh-v4lite`
4. **桌面版 Lite 可能是纯壳**：没有本地 Home
5. **.agents 共享技能库**：`~/.agents`（跨所有版本）

## 智能扫描算法设计
基于以上发现，扫描必须：
1. **内容特征识别**，不能靠固定路径
2. **区分 Home 类型**：官方/EAC（共享）、AIO、v4Lite
3. **识别共享组件**：.agents 技能库
4. **处理缺失组件**：v4Lite 没有 skills/，AIO 多 memories/team/
5. **检测 DSH_HOME 环境变量**：用户可能自定义
6. **检测 junction/symlink**：Home 可能是链接

## 扫描策略（分层）
```
Layer 1: 环境变量 DSH_HOME
Layer 2: 用户目录下的 .dsh*（.dsh、.dsh-v4lite、.dsh-*）
Layer 3: AppData 中的桌面壳 dsh-home
Layer 4: 内容特征验证（sessions/ + profiles/ + settings.yaml）
Layer 5: 共享组件识别（.agents）
```

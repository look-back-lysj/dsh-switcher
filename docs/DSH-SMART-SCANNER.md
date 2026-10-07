# DSH Home 智能识别算法设计（零固定路径版）

## 核心思想
不预设"去哪里找"，而是"怎么认出它是 DSH Home"。
扫描整个文件系统，对每个目录打分，分数够高就是 DSH Home。

## 特征信号体系（每个信号有权重）

### 强信号（单独即可确认）
| 信号 | 权重 | 说明 |
|---|---|---|
| sessions/ 目录下有 .zstd 文件 | +40 | DSH 会话数据，核心标识 |
| profiles/*/package.json 含 "@deepseek-ai/dsh" | +40 | DSH profile 声明 |
| .credentials.yaml 含 "refs:" 和 API key 格式 | +35 | DSH 凭据文件 |
| settings.yaml 含 "agent-presets:" | +35 | DSH 配置 |
| .dshw-usage.json 或 .dshw-size.json | +30 | DSH 桌面版统计文件 |

### 中信号（组合确认）
| 信号 | 权重 | 说明 |
|---|---|---|
| .agent-presets/ 目录 | +15 | DSH 预设 |
| guard/ 目录含 state.json | +15 | DSH 守护状态 |
| skills/ 目录 | +10 | DSH 技能 |
| storages/ 目录含 workspace.json | +10 | DSH 存储 |
| rollbacks/ 目录 | +10 | DSH 回滚 |
| undo-snapshots/ 目录 | +10 | DSH 快照 |
| memories/ 目录 | +10 | AIO 特有 |
| team/ 目录 | +10 | AIO 特有 |

### 弱信号（辅助）
| 信号 | 权重 | 说明 |
|---|---|---|
| 目录名含 "dsh" | +5 | 名称提示 |
| 目录名含 "deepseek" | +5 | 名称提示 |
| 目录在 ~/. 下 | +3 | 常见位置 |
| 目录在 AppData/ 下 | +3 | 常见位置 |

### 负信号（排除）
| 信号 | 权重 | 说明 |
|---|---|---|
| node_modules/ 在根层 | -50 | 可能是项目目录 |
| .git/ 在根层 | -30 | 可能是源码目录 |
| package.json 在根层且无 profiles/ | -20 | 可能是 Node 项目 |
| 目录名为 "Cache"/"GPUCache"/"logs" | -50 | 缓存目录 |
| 目录名为 "backup"/"backup-*" | -40 | 备份目录 |

## 评分规则
- **确认 DSH Home**：总分 >= 60
- **疑似 DSH Home**：40 <= 总分 < 60（需要用户确认）
- **不是 DSH Home**：总分 < 40

## 扫描策略（广度优先 + 深度限制）

### 第一层：快速扫描（常见位置）
```
扫描位置：
  - ~/（用户目录）
  - ~/AppData/Roaming/
  - ~/AppData/Local/
  - D:/（如果存在）
  - 环境变量 DSH_HOME（如果设置）
  - 环境变量 DSH_AGENTS_HOME（如果设置）

深度限制：最多 3 层
超时限制：5 秒
```

### 第二层：深度扫描（全盘，可选）
```
触发条件：第一层未找到任何 DSH Home，或用户手动触发
扫描位置：所有盘符（C:/、D:/、E:/...）
深度限制：最多 5 层
超时限制：30 秒
排除规则：
  - 跳过 Windows/、Program Files/、Program Files (x86)/
  - 跳过 $Recycle.Bin/、System Volume Information/
  - 跳过 node_modules/、.git/、Cache/
```

## 变体识别（不依赖路径）

### 官方版/EAC
- 特征：sessions/ + profiles/ + skills/ + settings.yaml
- 识别：profiles/*/package.json 含 "@deepseek-ai/dsh"
- 版本：package.json 中的 version 字段

### AIO
- 特征：sessions/ + profiles/ + memories/ + team/
- 识别：settings.yaml 含 "status-rotator:" 或存在 .dshw-size.json
- 版本：%APPDATA%/com.deepseek.dsh.desktop.aio/bundle-verified.json

### v4Lite
- 特征：sessions/ + profiles/（无 skills/、无 memories/）
- 识别：目录名含 "v4lite" 或 "lite"（弱信号，需配合其他特征）
- 版本：profiles/*/package.json

### 共享技能库
- 特征：skills/ 目录 + .skill-lock.json
- 识别：~/.agents/skills/ 或 DSH_AGENTS_HOME

## junction/symlink 处理
```rust
fn canonical_path(path: &Path) -> PathBuf {
    // 解析 junction/symlink，获取真实路径
    // 用于去重：同一个真实路径只扫描一次
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn is_same_home(a: &Path, b: &Path) -> bool {
    canonical_path(a) == canonical_path(b)
}
```

## 缓存与增量扫描
- 首次扫描：全量扫描，结果缓存到 `scan-cache.json`
- 后续扫描：只扫描上次后有变更的目录（mtime > last_scan）
- 缓存失效：手动触发"重新扫描"或缓存文件不存在

## 性能优化
- 并行扫描：多线程扫描不同目录
- 提前终止：找到 N 个 DSH Home 后停止（默认 10 个）
- 深度限制：最多 5 层，避免无限递归
- 大小限制：跳过 > 1GB 的目录（可能是数据目录）

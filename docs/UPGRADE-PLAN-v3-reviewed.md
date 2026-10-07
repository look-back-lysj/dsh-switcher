# DSH Vault v3 重构计划书（智能扫描 + Symlink 农场 · 审查修正版）

> 本文档经过 50 项审查点逐一查证，修正了所有理想主义部分。
> 核心设计：**零固定路径智能扫描** + **目录级 symlink 农场**（参考 GNU Stow）。

---

## §0 交接必读

| 项 | 位置 |
|---|---|
| 项目根目录 | `C:\Users\刘沛伦\Desktop\升级eac\dsh-vault\` |
| Rust 后端 | `src-tauri\src\`（main/model/repo/restore/scanner/export/zstd_check） |
| 前端 | `frontend\`（index.html / app.js / style.css） |
| 调研文档 | `docs/STOW-FINDINGS.md`、`docs/DSH-SMART-SCANNER.md`、`docs/REVIEW-FINDINGS.md` |
| 默认仓库 | `D:\DSH-Backups\repo` |

**工具链**：Rust `stable-x86_64-pc-windows-msvc`，VS BuildTools `D:\BuildTools`，Tauri CLI `D:\rust-tools\bin\cargo-tauri.exe`。

**铁律**：
1. `tauri.conf.json` 必须保留 `withGlobalTauri: true`
2. 改完前端行为没变化，先杀 `dsh-vault` 进程再 build
3. Windows symlink 创建已验证可行（开发者模式开启）
4. **DSH 运行时不能接管**：必须先检测 DSH 进程，提示用户关闭

---

## §1 核心设计（审查后修正）

### 智能扫描（零固定路径，已实测验证）
- 特征打分制：DSH Home 得分 200+，非 DSH 目录得分 0-10
- 不依赖任何固定路径，纯粹靠内容特征识别
- DSH_HOME 环境变量最高优先级

### Symlink 农场（目录级，参考 GNU Stow）
- **接管**：移动目录到仓库 + 原位置留目录级 symlink（只占一份空间）
- **切换**：改 symlink 指向（瞬间完成，零复制）
- **快照**：只记录清单（SHA-256），不复制文件实体

### 接管范围（审查后修正）
| 目录 | 是否接管 | 原因 |
|---|---|---|
| sessions/ | ✅ 接管 | 对话记录，核心数据，23MB |
| skills/ | ✅ 接管 | 技能，用户自定义，0MB |
| .agent-presets/ | ✅ 接管 | 预设，用户自定义，0.3MB |
| guard/ | ✅ 接管 | 守护状态，0MB |
| rollbacks/ | ✅ 接管 | 回滚，0MB |
| storages/ | ✅ 接管 | 存储，0.3MB |
| undo-snapshots/ | ✅ 接管 | 快照，0.1MB |
| profiles/ | ❌ 不接管 | 827MB，主要是 node_modules，可重建 |
| settings.yaml | ✅ 备份 | 配置文件，但不移动（在原位置） |
| .credentials.yaml | ✅ 备份 | 凭据文件，但不移动（在原位置） |

**profiles/ 处理**：不整体 symlink，但备份声明文件（package.json、cordis.yml、cordis.patch.yml、pnpm-workspace.yaml）。

---

## §2 智能扫描算法（已实测验证）

### 特征信号体系
- **强信号**：sessions/ 有 .zstd（+40）、profiles/ 含 @deepseek-ai/dsh（+40）、.credentials.yaml（+35）、settings.yaml 含 agent-presets（+35）、.dshw-*.json（+30）
- **中信号**：.agent-presets/（+15）、guard/state.json（+15）、skills/（+10）、storages/workspace.json（+10）、rollbacks/（+10）、memories/（+10，AIO 特有）、team/（+10，AIO 特有）
- **负信号**：node_modules/ 根层（-50）、.git/ 根层（-30）、Cache/ 目录（-50）

### 评分规则
- 确认 DSH Home：>= 60 分
- 疑似：40-59 分
- 不是：< 40 分

### 扫描策略
- Layer 0：DSH_HOME 环境变量（最高优先级）
- Layer 1：快速扫描（~/、AppData/、D:/，深度 3 层，5 秒）
- Layer 2：深度扫描（全盘，深度 5 层，30 秒，手动触发）
- Layer 3：junction/symlink 去重（realpath 规范化）

---

## §3 信息架构（3 页签）

```
[侧边导航]
  环境    → 智能扫描 → 查看接管状态 → 一键接管
  存档    → 快照列表 → 回到任意状态
  切换    → A 环境 → B 环境（拖拽改链接）
```

---

## §4 模块一：环境页（接管 + 状态监控）

### 后端新增命令
```rust
adopt_home(repo: String, home_path: String, note: String) -> Result<AdoptResult, String>
unadopt_home(repo: String, home_path: String) -> Result<UnadoptResult, String>
check_home_changes(repo: String, home_path: String) -> Result<Vec<ChangeInfo>, String>
```

### 接管流程（参考 GNU Stow --adopt，审查后修正）
```
1. 检测：DSH 是否在运行（如果在运行，提示用户先关闭）
2. 校验：目标环境未被接管（没有 .dshvault 标记）
3. 创建标记：在仓库创建 .dshvault 标记
4. 移动目录：sessions/、skills/、.agent-presets/ 等移到仓库 files/<home_id>/
   - profiles/ 不移动，但备份声明文件到仓库 profiles/<home_id>/
5. 创建链接：原位置创建目录级 symlink 指向仓库
   - 使用相对路径（便携性）
   - sessions/ 链接到 sessions/ 本身（DSH 需要创建新 project_dir）
6. 记录映射：manifest 中记录 symlink 映射
7. 返回结果：移动了多少目录、创建了多少链接
```

---

## §5 模块二：存档页（快照 + 恢复）

### 后端改动
- 快照只记录清单（SHA-256），不复制文件实体
- 新增 `restore_snapshot(repo, snapshot_id, target_home)`

---

## §6 模块三：切换页（核心新功能）

### 后端新增命令
```rust
switch_links(
    repo: String,
    source_home: String,
    target_home: String,
    include_sessions: bool,
    include_skills: bool,
    include_config: bool,
    include_memories: bool,
) -> Result<SwitchResult, String>
```

### 切换流程
```
1. 校验：来源和目标都已接管，且不是同一个
2. 检测：DSH 是否在运行（如果在运行，提示用户先关闭）
3. 自动备份：给目标环境创建快照（备注："切换前自动保存"）
4. 删除旧链接：删除目标环境的 symlink
5. 创建新链接：指向来源环境的目录（按用户勾选的类型）
6. 验证：检查新链接是否有效
7. 返回结果：切换了多少个链接
```

---

## §7 执行顺序

1. **智能扫描重构**：scanner.rs 改为特征打分制，零固定路径
2. **环境页接管功能**：后端 adopt/unadopt + 前端按钮 + DSH 运行检测
3. **变更检测**：后端 scan_home_changes + 前端徽章
4. **存档页快照改造**：清单式快照
5. **切换页**：后端 switch_links + 前端拖拽
6. **导航简化**：3 个页签
7. **测试**：单元测试 + e2e + 手动验证
8. **打包安装**

---

## §8 验收标准

### 功能验收
- [ ] 智能扫描能识别所有版本（官方/EAC/AIO/v4Lite/自定义）
- [ ] 扫描不依赖固定路径，新装版本也能识别
- [ ] 接管前检测 DSH 是否在运行，如果在运行则提示用户先关闭
- [ ] 接管后 DSH 正常运行，读写 symlink 指向的仓库文件
- [ ] 切换后目标环境拥有来源的内容
- [ ] 变更检测能发现 DSH 对文件的修改

### 技术验收
- [ ] DSH Home 得分 >= 60，非 DSH 目录得分 < 40
- [ ] symlink 创建/删除/切换正常（目录级）
- [ ] 快照只记录清单，不复制文件
- [ ] junction/symlink 去重，不重复扫描
- [ ] profiles/ 不接管，但声明文件备份

### 体验验收
- [ ] 新手 3 分钟内完成：扫描 → 接管 → 切换
- [ ] 每个页面只有 1 个主按钮
- [ ] 危险操作有确认弹窗和中文解释

---

## §9 风险与缓解（审查后修正）

| 风险 | 缓解 |
|---|---|
| DSH 运行时文件锁定 | 接管前检测 DSH 进程，提示用户先关闭 |
| profiles/ 太大（827MB） | 不接管 profiles/，只备份声明文件 |
| symlink 链接到 sessions/ 子目录 | 链接到 sessions/ 本身，DSH 可创建新 project_dir |
| 扫描漏检新装版本 | 特征打分制，不依赖固定路径 |
| 扫描误检非 DSH 目录 | 负信号排除 + 疑似确认 |
| symlink 被杀毒软件误报 | 提供"断开接管"还原方案 |
| 用户手动删除仓库文件 | 启动时校验 symlink 有效性 |
| 多环境共享文件冲突 | 切换前检测引用，提示用户 |
| junction 导致重复扫描 | realpath 规范化去重 |

---

## §10 明确不做（v3 范围外）

- 对话浏览器 → v3.1
- CAS 内容寻址存储 → v3.1
- 部分切换 → v3.1（本次默认全选，保留 checkbox）
- 定时备份 → v3.2
- 云同步 → 不做
- profiles/ 整体接管 → 不做（太大且可重建）

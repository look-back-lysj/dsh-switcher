# DSH Vault v3 计划审查报告（理想主义部分逐一查证）

## 审查点 1-5：symlink 兼容性 ✅ 已验证
- Python/Node 的 open() 可以透明读写 symlink
- Windows 开发者模式下 symlink 创建无需管理员权限
- junction 只能用于目录，symlink 可以用于文件和目录
- DSH 会话文件在未被使用时可以移动（未被锁定）

## 审查点 6-10：DSH 运行时文件锁定 ⚠️ 部分验证
- 当前 DSH 未运行时，sessions 文件可以移动
- 但 DSH 运行时可能会锁定 sessions 文件（需要进一步验证）
- **结论**：接管前必须检测 DSH 是否在运行，如果在运行则提示用户先关闭

## 审查点 11-20：DSH sessions 目录结构 ✅ 已验证
- sessions 目录结构：`sessions/<project_key>/<session_id>/session.jsonl.zstd`
- project_key 编码规则（format.ts:225-244）：
  - 路径分隔符 `/`、`\\`、`:` → `-`
  - 不安全字符 → `~<hex>`（4 位大写十六进制）
  - 开头和结尾加 `--`
  - 示例：`C:\Users\刘沛伦\Desktop\新建文件夹 (5)` → `--C-Users-~5218~6C9B~4F26-Desktop-~65B0~5EFA~6587~4EF6~5939~0020~00285~0029--`
- session_id 编码规则（format.ts:199-214）：
  - 安全字符保留：`A-Za-z0-9._-`
  - 其他字符 → `~<hex>`
  - 示例：`session-f7a98039-c3fb-4108-bde0-69776b63a2d7` → `session-f7a98039-c3fb-4108-bde0-69776b63a2d7`（无需编码）

## 审查点 21-25：DSH 创建新会话的行为 ✅ 已验证
- DSH 创建新会话时，需要创建 project_dir 和 session_dir
- 如果 symlink 链接到 sessions/ 目录本身，DSH 可以在其中创建新的 project_dir
- 如果 symlink 链接到 project_dir，DSH 可以在其中创建新的 session_dir
- **结论**：symlink 必须链接到 sessions/ 目录本身，或者 project_dir 级别

## 审查点 26-30：profiles 目录分析 ✅ 已验证
- profiles/ 总共 827MB，其中 node_modules 占 620MB+
- 重要配置文件（非 node_modules）：每个 profile 的 package.json、cordis.yml、cordis.patch.yml、pnpm-workspace.yaml
- **结论**：profiles/ 不适合整体 symlink（太大且可重建），但配置声明文件应该备份

## 审查点 31-35：各版本 profiles 结构 ✅ 已验证
- 所有版本的 profiles 结构一致：profiles/node_modules/（根级）+ profiles/<profile_name>/
- 每个 profile 目录下有 package.json、cordis.yml 等
- **结论**：profiles/ 的备份应该只包含声明文件，不包含 node_modules

## 审查点 36-40：DSH 源码 sessions 路径 ✅ 已验证
- CLI 模式：DSH_HOME/sessions/
- Web/桌面模式：profileHome/sessions/
- AIO 桌面版：dsh-home/sessions/
- EAC 桌面版：~/.dsh/sessions/（与 CLI 共享）
- **结论**：所有版本的 sessions 都在 Home 目录下

## 审查点 41-50：sessions 目录编码规则 ✅ 已验证
- projectKey()：工作目录路径 → 编码目录名
- encodeSegment()：会话 ID → 编码目录名
- **结论**：sessions 目录结构是确定的，可以安全地 symlink

## 关键风险（必须处理）

### 风险 1：DSH 运行时文件锁定
- **问题**：DSH 运行时可能锁定 sessions 文件，导致移动失败
- **缓解**：接管前检测 DSH 进程，如果在运行则提示用户先关闭

### 风险 2：profiles/ 太大
- **问题**：profiles/ 827MB，主要是 node_modules
- **缓解**：profiles/ 不整体 symlink，只备份声明文件（package.json、cordis.yml 等）

### 风险 3：symlink 到 sessions/ 目录本身
- **问题**：DSH 需要在 sessions/ 下创建新的 project_dir
- **缓解**：symlink 链接到 sessions/ 目录本身，而不是子目录

### 风险 4：新装版本扫描
- **问题**：用户可能安装新版本的 DSH，路径未知
- **缓解**：智能扫描算法（特征打分制），不依赖固定路径

## 计划修正

### 修正 1：接管范围
- **原计划**：接管整个 Home 目录（包括 profiles/）
- **修正**：只接管 sessions/、skills/、.agent-presets/、guard/、rollbacks/、storages/、undo-snapshots/
- **profiles/ 不接管**：太大且可重建，但备份声明文件

### 修正 2：symlink 粒度
- **原计划**：symlink 链接到文件级别
- **修正**：symlink 链接到目录级别（sessions/、skills/ 等），因为 DSH 需要创建新文件/目录

### 修正 3：DSH 运行检测
- **原计划**：未提及
- **修正**：接管前必须检测 DSH 进程，如果在运行则提示用户先关闭

### 修正 4：智能扫描
- **原计划**：特征打分制（已验证）
- **修正**：保留，但增加 DSH_HOME 环境变量最高优先级

## 最终结论

计划可行，但需要以下修正：
1. 接管范围缩小：不接管 profiles/（太大），只接管 sessions/、skills/ 等小目录
2. symlink 粒度调整：链接到目录级别，不是文件级别
3. 增加 DSH 运行检测：接管前必须确认 DSH 未运行
4. 智能扫描保留：特征打分制已验证可行

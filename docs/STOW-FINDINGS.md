# GNU Stow 核心设计决策（从 Stow.pm.in 2512 行源码提取）

## 1. 两阶段架构（Plan + Execute）
- plan_stow() / plan_unstow()：只规划任务，不执行，同时检测冲突
- process_tasks()：冲突检查通过后才执行
- 好处：任何冲突都会阻止整个操作，不会半途损坏状态

## 2. 冲突检测（极其重要）
- existing target is not owned by stow：目标已存在且不是 stow 的链接 → 冲突
- existing target is stowed to a different package：目标已链接到别的包 → 冲突
- source is an absolute symlink：绝对路径链接无法反向操作 → 冲突
- cannot stow over existing target：目标已存在且不是链接/目录 → 冲突（除非 --adopt）

## 3. Tree Folding（树折叠）
- 如果一个目录下所有链接都指向同一个包，就把整个目录替换为一个链接
- 好处：减少链接数量，提高性能
- 反向操作：unfolding（树展开），把目录链接拆成文件链接

## 4. Adopt 模式（收养现有文件）
- --adopt：把目标目录中已存在的普通文件移动到 stow 目录，然后创建链接
- 这正是我们"接管"功能的参考！
- Stow 的做法：move(target, pkg_path) + symlink(pkg_path, target)

## 5. 无状态设计
- Stow 不存额外数据库，所有状态从文件系统本身推导
- 好处：永远不会出现数据库和文件系统不一致
- 缺点：每次都要重新扫描（但 Stow 用 memoization 优化）

## 6. 安全机制
- should_skip_target()：跳过 stow 目录本身，防止自我引用
- .stow 标记文件：标记 stow 目录，防止误操作
- .nonstow 标记文件：保护目录不被 stow
- cleanup_invalid_links()：清理指向不存在目标的链接

## 7. 链接验证
- is_a_link()：判断是否是链接（考虑计划中的任务）
- read_a_link()：读取链接目标
- link_owned_by_package()：判断链接是否属于某个包
- find_stowed_path()：从链接反推包路径

## 8. 任务队列
- 每个任务：{action: create/remove/move, type: link/dir/file, path, source, dest}
- 冲突检测在 plan 阶段完成，execute 阶段只执行不检查
- do_link() / do_unlink() / do_mkdir() / do_rmdir() / do_mv()：包装操作，检测重复和冲突

## 对我们 DSH Vault 的启示
1. 采用两阶段：先规划（检测冲突）→ 用户确认 → 再执行
2. 接管时检测：目标已有文件且不是我们的链接 → 冲突，提示用户
3. 参考 Adopt 模式实现"接管"：移动文件 + 创建链接
4. 保持无状态：不存额外数据库，从文件系统推导状态
5. 安全机制：.dshvault 标记文件保护仓库目录
6. 冲突检测：目标已有文件/链接 → 列出冲突让用户选择

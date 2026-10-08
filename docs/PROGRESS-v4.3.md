# DSH Vault v4.3：根治「Unknown agent preset: anchored-standard」

> 起因：用户截图——官方版 resume 报 `RemoteError: Unknown agent preset: anchored-standard`，对话能看不能继续。

## 病根（逐层查证）

1. **报错点**：`anchored-standard` 写在每条会话文件的 header（`"agentPreset":"anchored-standard"`），
   官方版 0.2.0-rc.2 的 asar 里搜不到这个名字。
2. **官方内置 preset**（asar 注册表实证）：`default: standard`，`id: standard`，另有 code/ptc/ask/architect。
   `standard` 是官方必然认识的默认值。
3. **v4.1 的"切换不带 .agent-presets 目录"无效**：asar 原文写着该目录"Nothing reads that any more"，
   官方版早就不读它；预设名在会话头里，不在目录里。
4. **安全性前提**（1435 行真实会话实测）：`agentPreset` 整条会话只出现 1 次——header 那一行，
   后续事件不引用。所以只改 header 安全，不动对话内容。

## 修复方案（preset_fix.rs）

切换复制 sessions 后，逐条会话：解压首帧 → 若 header 的 agentPreset 不在官方白名单
（standard/code/ptc/ask/architect）→ 重写成 `standard` → 重新压缩首帧 → **后续帧原样字节拷贝**。
原子写（临时文件+改名）防中途断电；改不动的保持原样并计数，不中断切换。

另加 `fix_presets_now` 独立命令：不用重新切换，一键修复当前环境的存量非法预设。
体检（routecheck）现在返回每条会话的 preset，前端检测到非法预设会显示「一键修复预设问题」条。

## 验证（全部实证）

- 33 个 Rust 测试全绿（含 3 个 preset_fix 专项 + 1 个切换端到端改写测试）。
- 真实安装版 CDP 实调 `fix_presets_now`（AIO 环境）：
  `{"scanned":17,"rewritten":16,"alreadyOk":1,"failed":0,"from":[["anchored-standard",16]]}`
  → 16 条 anchored-standard 全部改写成 standard。
- 改写后抽查 3 条（含 1128 帧大会话）：header=standard、无 anchored 残留、内容帧完整可解压、无 tmp 残留。

## 给用户的话

- 以后切换会自动把这些预设改好，对话直接能继续。
- 你**当前**官方版里那条打不开的对话（session-0488fe01），不用再切换：
  打开 DSH Vault → 环境页做一次切换体检（或重新切换一次），预设就修好了；
  或者更直接——在官方版里新建对话也行，新建不受影响。

## 安装包

`D:\rust-target\dsh-vault-msvc\release\bundle\nsis\DSH Vault_0.1.0_x64-setup.exe`（4.29 MiB，2026-10-08 14:52）

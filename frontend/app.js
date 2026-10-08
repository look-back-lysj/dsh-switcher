const state = {
  homes: [],
  defaultRepo: '',
  manifest: null,
  preview: null,
  adoptStatus: {},   // home.path -> { adopted, record }
  switchSource: null,
  switchTarget: null,
};

let unlistenBackupProgress = null;
let unlistenRestoreProgress = null;

const listenProgress = async (event, panel, title) => {
  const listener = (event) => {
    const payload = event.payload;
    setProgress(panel, title, payload.done, payload.total, payload.current);
  };
  const unlisten = await window.__TAURI__.event.listen(event, listener);
  return unlisten;
};

const setupProgressListeners = async () => {
  // 进度监听失败不能阻断按钮绑定；事件权限缺失时静默降级。
  try {
    if (unlistenBackupProgress) unlistenBackupProgress();
    if (unlistenRestoreProgress) unlistenRestoreProgress();
    unlistenBackupProgress = await listenProgress('backup-progress', $('backup-progress'), '正在备份');
    unlistenRestoreProgress = await listenProgress('restore-progress', $('restore-progress'), '正在恢复');
  } catch (error) {
    console.warn('[dsh-vault] 进度监听初始化失败，已降级为按钮状态提示', error);
  }
};

const invoke = async (cmd, payload = {}) => {
  try {
    return await window.__TAURI__.core.invoke(cmd, payload);
  } catch (error) {
    throw new Error(typeof error === 'string' ? error : error?.message || String(error));
  }
};

const formatBytes = (bytes) => {
  if (!Number.isFinite(bytes)) return '—';
  const units = ['B', 'KB', 'MB', 'GB'];
  let value = bytes;
  let index = 0;
  while (value >= 1024 && index < units.length - 1) {
    value /= 1024;
    index += 1;
  }
  return `${value.toFixed(value >= 10 || index === 0 ? 0 : 1)} ${units[index]}`;
};

const $ = (id) => document.getElementById(id);

const setBusy = (button, busy, text = '处理中…') => {
  if (!button) return;
  button.disabled = busy;
  if (busy) {
    button.dataset.label = button.textContent;
    button.textContent = text;
  } else {
    button.textContent = button.dataset.label || button.textContent;
  }
};

const showResult = (element, data, isError = false) => {
  element.hidden = false;
  element.classList.toggle('error', isError);
  element.textContent = typeof data === 'string' ? data : JSON.stringify(data, null, 2);
};

function healthBadge(home) {
  if (home.sessions.corrupt > 0) return ['danger', '有损坏'];
  if (home.sessions.truncated > 0) return ['warning', '有截断'];
  if (home.kind === 'agents-home') return ['success', '技能库'];
  return ['success', '健康'];
}

function animateNumber(el, target, duration = 600) {
  const start = performance.now();
  const reduced = window.matchMedia('(prefers-reduced-motion: reduce)').matches;
  if (reduced || typeof target !== 'number' || target === 0) {
    el.textContent = target;
    return;
  }
  const tick = (now) => {
    const t = Math.min((now - start) / duration, 1);
    const eased = 1 - Math.pow(1 - t, 3);
    el.textContent = Math.round(target * eased);
    if (t < 1) requestAnimationFrame(tick);
  };
  requestAnimationFrame(tick);
}

function renderHealth() {
  const totalSessions = state.homes.reduce((sum, h) => sum + h.sessions.total, 0);
  const okSessions = state.homes.reduce((sum, h) => sum + h.sessions.ok, 0);
  const totalSize = state.homes.reduce((sum, h) => sum + h.backupSize, 0);

  const metrics = [
    { label: '识别到环境', value: state.homes.length, numeric: true },
    { label: '会话总数', value: totalSessions, numeric: true },
    { label: '结构正常会话', value: `${okSessions}/${totalSessions}`, numeric: false },
    { label: '预计备份', value: formatBytes(totalSize), numeric: false },
  ];
  $('health-summary').innerHTML = metrics.map((item, i) => `
    <div class="metric stagger-item" style="--i:${i}">
      <strong class="metric-value" data-target="${item.numeric ? item.value : ''}">${item.numeric ? '0' : item.value}</strong>
      <span>${item.label}</span>
    </div>
  `).join('');
  // 数字滚动动画
  document.querySelectorAll('.metric-value[data-target]').forEach(el => {
    const target = parseInt(el.dataset.target, 10);
    if (!isNaN(target)) animateNumber(el, target);
  });
  // 注：home-list 的卡片渲染统一由 renderEnvironment 负责，这里只处理统计区与空状态。
  if (!state.homes.length) {
    $('health-summary').innerHTML = '';
    $('home-list').innerHTML = `
      <div class="empty-state">
        <strong>没有扫描到 DSH 环境。</strong><br>
        请先运行一次 DeepSeek Harness，让它生成 <code>~/.dsh</code> 或桌面版 Home 目录，再点右上角「重新扫描」。
      </div>
    `;
    return;
  }
}

function fillHomeSelects() {
  const source = $('source-home');
  const target = $('target-home');
  source.innerHTML = '';
  target.innerHTML = '';

  state.homes.forEach((home) => {
    if (home.kind !== 'dsh-home') return;
    source.append(new Option(`${home.label} — ${home.path}`, home.path));
    target.append(new Option(`${home.label} — ${home.path}`, home.path));
  });
}

const selectDirectory = async (title) => {
  try {
    const selected = await window.__TAURI__.dialog.open({
      title,
      directory: true,
      multiple: false,
    });
    return typeof selected === 'string' ? selected : null;
  } catch (error) {
    showResult($('repo-result'), `无法打开目录选择器：${error.message}`, true);
    return null;
  }
};

const selectSaveFile = async (title, filters) => {
  try {
    const selected = await window.__TAURI__.dialog.save({ title, filters });
    return typeof selected === 'string' ? selected : null;
  } catch (error) {
    showResult($('repo-result'), `无法打开保存选择器：${error.message}`, true);
    return null;
  }
};

const selectOpenFile = async (title, filters) => {
  try {
    const selected = await window.__TAURI__.dialog.open({
      title,
      multiple: false,
      directory: false,
      filters,
    });
    return typeof selected === 'string' ? selected : null;
  } catch (error) {
    showResult($('repo-result'), `无法打开文件选择器：${error.message}`, true);
    return null;
  }
};
// 启动秒读缓存：先把上次扫描结果显示出来，再异步重扫刷新。
async function loadScanCache() {
  try {
    const res = await invoke('load_scan_cache');
    if (!res.found || !res.homes.length) return false;
    // 标记已消失的环境（前端标灰提示，不静默丢）
    state.homes = res.homes.map((c) => {
      const h = c.home;
      if (!c.exists) {
        h.warnings = (h.warnings || []).concat(['该路径已不存在，可能已卸载或移动']);
        h.__missing = true;
      }
      return h;
    });
    // defaultRepo 由随后的 scan() 填充；先给缓存时间戳
    // 缓存超过 7 天未刷新 → 顶部提示建议重扫
    let staleNote = '';
    try {
      const ageMs = Date.now() - new Date(res.lastScanAt).getTime();
      if (ageMs > 7 * 24 * 3600 * 1000) {
        const days = Math.floor(ageMs / (24 * 3600 * 1000));
        staleNote = `（已 ${days} 天未刷新，建议点「重新扫描」）`;
      }
    } catch (e) {}
    if ($('scan-time')) $('scan-time').textContent = `缓存 ${res.lastScanAt}${staleNote}（后台刷新中…）`;
    renderEnvironment();
    fillHomeSelects();
    return true;
  } catch (e) {
    return false;
  }
}

async function scan() {
  const button = $('rescan');
  setBusy(button, true, '扫描中…');
  try {
    const result = await invoke('scan_homes');
    state.homes = result.homes;
    state.defaultRepo = result.defaultRepo;
    $('default-repo').textContent = result.defaultRepo;
    $('backup-repo').value = result.defaultRepo;
    $('restore-repo').value = result.defaultRepo;
    $('repository-repo').value = result.defaultRepo;
    $('import-repo').value = `${result.defaultRepo}-imported`;
    $('export-file').value = `${result.defaultRepo}\\dsh-vault-export.zip`;
    $('scan-time').textContent = new Date().toLocaleString();
    await refreshAdoptStatus();
    renderEnvironment();
    await checkMismatch();
    fillHomeSelects();
  updateBackupHeroStats();
  renderTmCurrent();
  refreshSnapshots();
    renderRepositorySummary();
  } catch (error) {
    showResult($('backup-result'), `扫描失败：${error.message}`, true);
  } finally {
    setBusy(button, false);
  }
}

function renderRepositorySummary() {
  $('repo-summary').innerHTML = [
    { label: '当前仓库', value: '手动选择' },
    { label: '导出格式', value: 'ZIP' },
    { label: '导入校验', value: 'SHA-256' },
    { label: '覆盖撤回', value: '最近一次' },
  ].map(item => `
    <div class="metric"><strong>${item.value}</strong><span>${item.label}</span></div>
  `).join('');
}

function setProgress(panel, title, done, total, file) {
  panel.hidden = false;
  panel.querySelector('strong').textContent = title;
  panel.querySelector('.progress-track + .progress-file, .progress-file').textContent = file || '…';
  panel.querySelector('span').textContent = `${done} / ${total}`;
  const percent = total ? Math.round((done / total) * 100) : 0;
  panel.querySelector('.progress-bar').style.transform = `scaleX(${percent / 100})`;
  const track = panel.querySelector('.progress-track');
  if (track) track.classList.toggle('active', done < total);
}

function resetProgress(panel) {
  panel.hidden = true;
  panel.querySelector('.progress-bar').style.transform = 'scaleX(0)';
  const track = panel.querySelector('.progress-track');
  if (track) track.classList.remove('active');
}

async function startBackup() {
  const button = $('start-backup');
  const note = ($('backup-note').value || '').trim();
  setBusy(button, true, '备份中…');
  resetProgress($('backup-progress'));
  $('backup-success').hidden = true;
  $('backup-result').hidden = true;
  try {
    const result = await invoke('backup', {
      repo: $('backup-repo').value,
      note: note,
      onlyConfig: $('only-config').checked,
    });
    // 成功后显示动画卡片
    $('backup-progress').hidden = true;
    const noteLabel = note ? `「${note}」` : '这次备份';
    $('backup-success-text').textContent = `${noteLabel}已保存 ${result.files} 个文件（${formatBytes(result.bytes)}），可在「时光机」中随时回到这里。`;
    $('backup-success').hidden = false;
    $('backup-note').value = '';
    // 触发打勾动画重播
    const icon = document.querySelector('.backup-success-icon svg');
    if (icon) { icon.style.animation = 'none'; icon.offsetHeight; icon.style.animation = ''; }
    // 刷新时光机快照列表（如果已加载）
    if (typeof refreshSnapshots === 'function') refreshSnapshots();
  } catch (error) {
    showResult($('backup-result'), `备份失败：${error.message}`, true);
  } finally {
    setBusy(button, false);
  }
}

function updateBackupHeroStats() {
  const totalSessions = state.homes.reduce((s, h) => s + h.sessions.total, 0);
  const totalSize = state.homes.reduce((s, h) => s + h.backupSize, 0);
  const el = (id) => document.getElementById(id);
  if (el('backup-stat-env')) el('backup-stat-env').textContent = state.homes.length;
  if (el('backup-stat-session')) el('backup-stat-session').textContent = totalSessions;
  if (el('backup-stat-size')) el('backup-stat-size').textContent = formatBytes(totalSize);
}

async function verifyRepo(repoPath, resultElement) {
  const button = $('repo-verify');
  setBusy(button, true, '校验中…');
  try {
    const result = await invoke('verify', { repo: repoPath });
    if (result.ok === result.total && !result.bad.length && !result.missing.length) {
      showResult(resultElement, `校验通过：${result.ok}/${result.total} 个文件一致。`);
    } else {
      showResult(resultElement, `校验发现问题：${result.ok}/${result.total} 一致，损坏 ${result.bad.length}，缺失 ${result.missing.length}。\n\n${JSON.stringify(result, null, 2)}`, true);
    }
  } catch (error) {
    showResult(resultElement, `校验失败：${error.message}`, true);
  } finally {
    setBusy(button, false);
  }
}

async function loadCatalog() {
  const button = $('load-catalog');
  setBusy(button, true, '读取中…');
  try {
    const manifest = await invoke('get_catalog', { repo: $('restore-repo').value });
    state.manifest = manifest;
    const source = $('source-home');
    source.innerHTML = '';
    manifest.homes.forEach((home) => {
      source.append(new Option(`${home.label} — ${home.path}`, home.id));
    });
    $('restore-preview').hidden = true;
    $('execute-restore').disabled = true;
    showResult($('restore-result'), `已读取仓库：${manifest.files.length} 个文件，${manifest.homes.length} 个环境。`);
  } catch (error) {
    showResult($('restore-result'), `读取仓库失败：${error.message}`, true);
  } finally {
    setBusy(button, false);
  }
}

function filterValue() {
  const raw = $('restore-filter').value.trim();
  if (!raw) return { ids: [], project: null };
  const uuidRe = /[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}/g;
  const ids = raw.match(uuidRe) || [];
  const text = raw.replace(uuidRe, '').replace(/[，,]/g, ' ').trim();
  return { ids, project: text || null };
}

function actionLabel(action) {
  const map = {
    create: ['新建', 'success'],
    skip: ['相同跳过', ''],
    'skip-existing': ['存在跳过', ''],
    'overwrite-newer': ['更新覆盖', 'warning'],
    overwrite: ['强制覆盖', 'danger'],
  };
  return map[action] || [action, ''];
}

function renderPreview(preview) {
  const body = $('restore-preview-body');
  const rows = preview.items.slice(0, 300);
  body.innerHTML = rows.map((item) => {
    const [label, tone] = actionLabel(item.action);
    return `<tr><td><span class="badge ${tone}">${label}</span></td><td><code>${item.rel}</code></td></tr>`;
  }).join('');
  if (preview.items.length > rows.length) {
    body.insertAdjacentHTML('beforeend', `<tr><td></td><td class="muted">其余 ${preview.items.length - rows.length} 项已省略，执行时仍会处理。</td></tr>`);
  }
  $('restore-preview-summary').textContent = `新建 ${preview.createCount} · 跳过 ${preview.skipCount} · 覆盖 ${preview.overwriteCount}`;
  $('restore-preview').hidden = false;
}

async function previewRestore() {
  if (!state.manifest) await loadCatalog();
  const button = $('preview-restore');
  setBusy(button, true, '预览中…');
  try {
    const filter = filterValue();
    const preview = await invoke('preview_restore', {
      repo: $('restore-repo').value,
      scope: $('restore-scope').value,
      sourceHomeId: $('source-home').value,
      targetHome: $('target-home').value,
      ids: filter.ids,
      project: filter.project,
      mode: $('restore-mode').value,
    });
    state.preview = preview;
    $('execute-restore').disabled = preview.items.length === 0;
    renderPreview(preview);
    showResult($('restore-result'), [
      `恢复目标：${preview.targetHome}`,
      `将新建 ${preview.createCount} 个，跳过 ${preview.skipCount} 个，覆盖 ${preview.overwriteCount} 个。`,
      preview.warnings.length ? `提示：${preview.warnings.join('；')}` : '',
    ].filter(Boolean).join('\n'));
  } catch (error) {
    showResult($('restore-result'), `预览失败：${error.message}`, true);
  } finally {
    setBusy(button, false);
  }
}

function confirmDialog({ title, message, items = [], confirmText = '确认执行', danger = true }) {
  const dialog = $('confirm-dialog');
  $('confirm-title').textContent = title;
  $('confirm-message').textContent = message;
  $('confirm-list').innerHTML = items.map(v => `<li>${v}</li>`).join('');
  const accept = $('confirm-accept');
  accept.textContent = confirmText;
  accept.classList.toggle('danger', danger);
  accept.classList.toggle('primary', !danger);
  dialog.showModal();
  return new Promise((resolve) => {
    dialog.addEventListener('close', () => resolve(dialog.returnValue === 'accept'), { once: true });
  });
}

// 自定义输入对话框：替代原生 prompt()（WebView2 对原生 prompt 支持差，会导致「点接管没反应/关不掉」）。
// 返回用户输入的字符串；用户取消返回 null。
function promptDialog({ title, message = '', defaultValue = '', placeholder = '' }) {
  const dialog = $('prompt-dialog');
  $('prompt-title').textContent = title;
  $('prompt-message').textContent = message;
  const input = $('prompt-input');
  input.value = defaultValue;
  input.placeholder = placeholder;
  return new Promise((resolve) => {
    const accept = $('prompt-accept');
    const onSubmit = (e) => {
      // 由哪个按钮提交决定结果；取消按钮 value 为空
      const submitter = e.submitter;
      const ok = submitter && submitter.value === '__ok__';
      // 延迟到 dialog 关闭后 resolve，保证状态稳定
      setTimeout(() => resolve(ok ? input.value : null), 0);
      cleanup();
    };
    const onCancel = () => { resolve(null); cleanup(); };
    const cleanup = () => {
      $('prompt-form').removeEventListener('submit', onSubmit);
      dialog.removeEventListener('cancel', onCancel);
    };
    $('prompt-form').addEventListener('submit', onSubmit);
    dialog.addEventListener('cancel', onCancel); // Esc 键
    dialog.showModal();
    input.focus();
    input.select();
  });
}

async function executeRestore() {
  if (!state.preview) return;
  const modeText = {
    fill_missing: '只补缺：已存在的文件一律不覆盖',
    merge_newer: '更新覆盖：仅当备份比目标文件更新时覆盖',
    force: '强制覆盖：目标文件一律被备份内容覆盖',
  }[$('restore-mode').value];

  const accepted = await confirmDialog({
    title: '确认恢复',
    message: `将恢复到：${state.preview.targetHome}`,
    items: [
      `范围：${$('restore-scope').selectedOptions[0].text}`,
      `策略：${modeText}`,
      `结果：新建 ${state.preview.createCount}，跳过 ${state.preview.skipCount}，覆盖 ${state.preview.overwriteCount}`,
      $('restore-mode').value === 'fill_missing' ? '此模式不会覆盖现有文件。' : '覆盖前会自动保存快照，可撤回最近一次覆盖恢复。',
    ],
  });
  if (!accepted) return;

  const button = $('execute-restore');
  setBusy(button, true, '恢复中…');
  try {
    const filter = filterValue();
    const result = await invoke('restore', {
      repo: $('restore-repo').value,
      scope: $('restore-scope').value,
      sourceHomeId: $('source-home').value,
      targetHome: $('target-home').value,
      ids: filter.ids,
      project: filter.project,
      mode: $('restore-mode').value,
    });
    showResult($('restore-result'), [
      `恢复完成：新建 ${result.created}，跳过 ${result.skipped}，覆盖 ${result.overwritten}。`,
      result.snapshot ? `覆盖前快照：` + result.snapshot : '',
    ].filter(Boolean).join('\\n'));
  } catch (error) {
    showResult($('restore-result'), `恢复失败：${error.message}`, true);
  } finally {
    setBusy(button, false);
  }
}

async function exportBackup() {
  const button = $('export-backup');
  setBusy(button, true, '导出中…');
  try {
    const result = await invoke('export_backup', {
      repo: $('repository-repo').value,
      target: $('export-file').value,
    });
    showResult($('repo-result'), `导出完成：${result.entries} 个条目，${formatBytes(result.bytes)}。\n${result.file}`);
  } catch (error) {
    showResult($('repo-result'), `导出失败：${error.message}`, true);
  } finally {
    setBusy(button, false);
  }
}

async function importBackup() {
  const button = $('import-backup');
  const accepted = await confirmDialog({
    title: '导入备份包',
    message: `将导入到：${$('import-repo').value}`,
    items: [
      '目标必须是空仓库；已有 manifest.json 时不会导入。',
      '导入完成后会自动做 SHA-256 全量校验。',
      '导入不会修改当前 DSH Home，只创建备份仓库。',
    ],
  });
  if (!accepted) return;
  setBusy(button, true, '导入中…');
  try {
    const result = await invoke('import_backup', {
      archive: $('import-archive').value,
      repo: $('import-repo').value,
    });
    showResult($('repo-result'), `导入完成并校验通过：${result.entries} 个条目。\n${result.repo}`);
  } catch (error) {
    showResult($('repo-result'), `导入失败：${error.message}`, true);
  } finally {
    setBusy(button, false);
  }
}

async function loadRollbackLedgers() {
  const button = $('load-rollback-ledgers');
  setBusy(button, true, '读取中…');
  try {
    let entries = await invoke('list_rollback_ledgers', {
      repo: $('repository-repo').value,
    });
    const panel = $('rollback-ledgers');
    const list = $('rollback-ledger-list');
    panel.hidden = false;
    if (!Array.isArray(entries)) {
      throw new Error("回滚日志接口返回格式不正确");
    }
    if (!entries.length) {
      list.innerHTML = '<div class="empty-state">没有回滚日志。出现恢复失败且回滚异常时，会在这里生成记录。</div>';
      $('rollback-ledger-summary').textContent = '0 条';
      return;
    }
    const normalizedEntries = entries.map((entry) => ({ ...entry, errors: entry.errors || entry.ledger?.errors || [] }));
    list.innerHTML = normalizedEntries.map((entry) => `
      <article class="ledger-row">
        <div>
          <div class="ledger-title">
            <span class="badge ${entry.pendingCount ? 'danger' : 'warning'}">
              ${entry.pendingCount ? `${entry.pendingCount} 项待补偿` : '回滚完成'}
            </span>
            <strong>${new Date(entry.createdAt).toLocaleString()}</strong>
          </div>
          <code>${entry.targetHome}</code>
          <ul class="ledger-errors">${entry.errors.map((error) => `<li>${error}</li>`).join("")}</ul>
        </div>
        <div class="ledger-actions">
          <button class="button compact" data-ledger="${entry.file}" type="button">预览补偿</button>
        </div>
        <details>
          <summary>日志文件</summary>
          <code>${entry.file}</code>
        </details>
      </article>
    `).join('');
    document.getElementById("rollback-ledger-summary").textContent = `${normalizedEntries.length} 条`;
  } catch (error) {
    console.error("[dsh-vault] loadRollbackLedgers failed", error, error?.stack);
    console.error("[dsh-vault] loadRollbackLedgers failed", error);
    showResult($("repo-result"), "读取回滚日志失败：" + error.message + "（" + (error?.stack?.split(String.fromCharCode(10))[0] ?? "无堆栈") + "）", true);
  } finally {
    setBusy(button, false);
  }
}
async function previewCompensate(ledgerFile) {
  try {
    const preview = await invoke('preview_rollback_compensate', { ledgerFile });
    const accepted = await confirmDialog({
      title: '执行回滚补偿',
      message: `将补偿：${preview.targetHome}`,
      items: [
        ...preview.items.slice(0, 8).map((item) => `${item.action}：${item.rel}（${item.reason}）`),
        preview.items.length > 8 ? `其余 ${preview.items.length - 8} 项已省略，执行时仍会处理` : '',
        '只会恢复“覆盖类”文件；新建类文件不会自动删除。',
      ].filter(Boolean),
    });
    if (!accepted) return;
    const result = await invoke('apply_rollback_compensate', { ledgerFile });
    showResult($('repo-result'), `补偿完成：恢复 ${result.restored}，跳过 ${result.skipped}。${result.failed.length ? `失败 ${result.failed.length} 项` : ''}`);
  } catch (error) {
    showResult($('repo-result'), `补偿失败：${error.message}`, true);
  }
}

async function undo() {
  try {
    const result = await invoke('undo_last', { repo: $('repository-repo').value });
    showResult($('repo-result'), `撤回完成：还原 ${result.restored} 个，跳过 ${result.skipped} 个。`);
  } catch (error) {
    showResult($('repo-result'), `撤回失败：${error.message}`, true);
  }
}
// ===== 时光机 =====
let tmSnapshots = [];
let tmSelectedSnapshot = null;

async function refreshSnapshots() {
  const repo = $('backup-repo').value || $('repository-repo').value;
  if (!repo) return;
  try {
    const list = await invoke('list_snapshots', { repo });
    tmSnapshots = list || [];
    renderSnapshotList();
  } catch (e) {
    console.warn('[dsh-vault] refreshSnapshots failed', e);
    tmSnapshots = [];
    renderSnapshotList();
  }
}

function renderSnapshotList() {
  const container = $('tm-snapshot-list');
  const countEl = $('tm-snapshot-count');
  countEl.textContent = `${tmSnapshots.length} 个`;

  if (!tmSnapshots.length) {
    container.innerHTML = `
      <div class="empty-state">
        <strong>还没有快照。</strong><br>
        去「一键备份」创建第一个快照，以后就能随时回到这里。
      </div>`;
    return;
  }

  container.innerHTML = tmSnapshots.map((snap, i) => {
    const isCurrent = snap.isCurrent;
    const badge = isCurrent ? '<span class="tm-snapshot-badge current">当前</span>' : '';
    const time = snap.createdAt ? snap.createdAt.replace('T', ' ').slice(0, 16) : '—';
    const homeCount = snap.homes ? snap.homes.length : 0;
    return `
      <div class="tm-snapshot-card stagger-item ${tmSelectedSnapshot === snap.id ? 'is-selected' : ''}"
           style="--i:${i}" data-snapshot-id="${snap.id}" role="button" tabindex="0">
        <div class="tm-snapshot-head">
          <span class="tm-snapshot-note" data-note-id="${snap.id}" title="点击改名">${escapeHtml(snap.note || '未命名快照')}</span>
          ${badge}
        </div>
        <div class="tm-snapshot-meta">
          <span>${time}</span>
          <span>${homeCount} 个环境</span>
          <span>${snap.fileCount || 0} 个文件</span>
          <span>${formatBytes(snap.totalSize || 0)}</span>
        </div>
      </div>`;
  }).join('');

  // 绑定点击事件
  container.querySelectorAll('.tm-snapshot-card').forEach(card => {
    card.addEventListener('click', (e) => {
      if (e.target.closest('.tm-snapshot-note[contenteditable]')) return;
      selectSnapshot(card.dataset.snapshotId);
    });
    card.addEventListener('keydown', (e) => {
      if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); selectSnapshot(card.dataset.snapshotId); }
    });
  });

  // 双击改名
  container.querySelectorAll('.tm-snapshot-note').forEach(el => {
    el.addEventListener('dblclick', () => startRenameSnapshot(el));
  });
}

function escapeHtml(s) {
  const d = document.createElement('div');
  d.textContent = s;
  return d.innerHTML;
}

async function selectSnapshot(id) {
  tmSelectedSnapshot = id;
  document.querySelectorAll('.tm-snapshot-card').forEach(c => c.classList.toggle('is-selected', c.dataset.snapshotId === id));
  const snap = tmSnapshots.find(s => s.id === id);
  if (!snap) return;

  const detail = $('tm-detail');
  detail.hidden = false;
  $('tm-detail-title').textContent = snap.note || '快照详情';

  const time = snap.createdAt ? snap.createdAt.replace('T', ' ').slice(0, 16) : '—';
  $('tm-detail-meta').innerHTML = `
    <div class="tm-detail-meta-item"><strong>${snap.fileCount || 0}</strong><span>文件数</span></div>
    <div class="tm-detail-meta-item"><strong>${formatBytes(snap.totalSize || 0)}</strong><span>总大小</span></div>
    <div class="tm-detail-meta-item"><strong>${(snap.homes || []).length}</strong><span>环境数</span></div>
    <div class="tm-detail-meta-item"><strong>${time}</strong><span>备份时间</span></div>
  `;

  $('tm-detail-homes').innerHTML = (snap.homes || []).map(h => `
    <div class="tm-detail-home">
      <span>${escapeHtml(h.label || h.path)}</span>
      <code>${escapeHtml(h.path)}</code>
    </div>
  `).join('');

  // 滚动到详情
  detail.scrollIntoView({ behavior: 'smooth', block: 'nearest' });
}

function startRenameSnapshot(el) {
  const id = el.dataset.noteId;
  const snap = tmSnapshots.find(s => s.id === id);
  if (!snap) return;
  el.contentEditable = true;
  el.focus();
  document.execCommand('selectAll', false, null);

  const finish = () => {
    el.contentEditable = false;
    const newNote = el.textContent.trim();
    if (newNote && newNote !== snap.note) {
      snap.note = newNote;
      // TODO: 调用后端 rename_snapshot 命令持久化
      renderSnapshotList();
    }
    el.removeEventListener('blur', finish);
    el.removeEventListener('keydown', onKey);
  };
  const onKey = (e) => {
    if (e.key === 'Enter') { e.preventDefault(); el.blur(); }
    if (e.key === 'Escape') { el.textContent = snap.note; el.blur(); }
  };
  el.addEventListener('blur', finish);
  el.addEventListener('keydown', onKey);
}

async function tmRestoreSnapshot() {
  if (!tmSelectedSnapshot) return;
  const snap = tmSnapshots.find(s => s.id === tmSelectedSnapshot);
  if (!snap) return;

  const accepted = await confirmDialog({
    title: '回到这个状态',
    message: `将把电脑上的 DSH 环境恢复到「${snap.note || snap.id}」的状态。`,
    items: [
      `备份时间：${snap.createdAt ? snap.createdAt.replace('T', ' ').slice(0, 16) : '—'}`,
      `包含 ${(snap.homes || []).length} 个环境、${snap.fileCount || 0} 个文件`,
      '恢复前会自动先把当前状态存为新快照（保险），随时可以切回来。',
      '恢复期间请关闭所有 DSH 窗口。',
    ],
  });
  if (!accepted) return;

  // 先备份当前状态作为保险
  showResult($('tm-result'), '正在保存当前状态作为保险快照…', false);
  try {
    await invoke('backup', {
      repo: $('backup-repo').value,
      note: `恢复前自动保存（回到「${snap.note || snap.id}」之前）`,
      onlyConfig: false,
    });
  } catch (e) {
    showResult($('tm-result'), `保险快照保存失败：${e.message}。为了安全，已取消恢复。`, true);
    return;
  }

  // 执行恢复
  showResult($('tm-result'), '保险快照已保存，正在恢复…', false);
  try {
    // 对当前快照用 restore，对历史快照暂时提示用恢复页
    if (snap.isCurrent) {
      showResult($('tm-result'), '当前快照就是最新状态，无需恢复。', false);
      return;
    }
    // 历史快照恢复：用 restore 命令从仓库恢复
    const result = await invoke('restore', {
      repo: $('backup-repo').value,
      scope: 'home',
      mode: 'fill_missing',
      sourceHome: snap.homes?.[0]?.path || '',
      targetHome: snap.homes?.[0]?.path || '',
      filter: '',
    });
    showResult($('tm-result'), `恢复完成：新建 ${result.created}，跳过 ${result.skipped}，失败 ${result.failed}。`, false);
    refreshSnapshots();
  } catch (e) {
    showResult($('tm-result'), `恢复失败：${e.message}`, true);
  }
}

// 当前状态卡片
function renderTmCurrent() {
  const body = $('tm-current-body');
  if (!state.homes.length) {
    body.innerHTML = '<p class="muted">未扫描到 DSH 环境。请先在首页点击「重新扫描」。</p>';
    return;
  }
  $('tm-current-time').textContent = new Date().toLocaleString('zh-CN');
  body.innerHTML = state.homes.map(h => {
    const [tone, label] = healthBadge(h);
    return `
      <div class="tm-current-env">
        <span class="badge ${tone}">${label}</span>
        <div>
          <strong>${escapeHtml(h.label)}</strong>
          <span class="muted"> — ${h.sessions.total} 个对话</span>
        </div>
      </div>`;
  }).join('');
}



// ---------- 全局操作进度浮层 ----------
let unlistenOpProgress = null;
const opOverlay = () => $("op-progress-overlay");
// 耗时操作进行中需要禁用的按钮选择器（操作互斥，防止并发触发）。
const OP_MUTEX_SELECTORS = [
  '#deep-scan', '#rescan', '#start-backup', '#execute-restore',
  '#switch-execute', '#undo-last', '#export-backup', '#import-backup',
];
let opMutexDepth = 0; // 嵌套调用计数（归位会先备份再归位，属嵌套）

function setOpButtonsDisabled(disabled) {
  OP_MUTEX_SELECTORS.forEach((sel) => {
    const el = document.querySelector(sel);
    if (el) el.disabled = disabled;
  });
  // 接管/断开按钮是动态渲染的，用容器代理禁用
  const homeList = $('home-list');
  if (homeList) homeList.classList.toggle('op-busy', disabled);
}

function showOpProgress(title) {
  const o = opOverlay();
  if (!o) return;
  $("op-progress-title").textContent = title || "正在处理";
  o.hidden = false;
  opMutexDepth += 1;
  if (opMutexDepth === 1) setOpButtonsDisabled(true);
}
function updateOpProgress(done, total, current) {
  const bar = $("op-progress-bar");
  if (bar) bar.style.transform = `scaleX(${total ? done / total : 0})`;
  const f = $("op-progress-file");
  if (f) f.textContent = current || (total ? `${done} / ${total}` : "处理中…");
}
function hideOpProgress() {
  const o = opOverlay();
  if (o) o.hidden = true;
  const bar = $("op-progress-bar");
  if (bar) bar.style.transform = "scaleX(0)";
  opMutexDepth = Math.max(0, opMutexDepth - 1);
  if (opMutexDepth === 0) setOpButtonsDisabled(false);
}
async function setupOpProgressListener() {
  try {
    if (unlistenOpProgress) unlistenOpProgress();
    unlistenOpProgress = await window.__TAURI__.event.listen("op-progress", (event) => {
      const p = event.payload || {};
      updateOpProgress(p.done || 0, p.total || 0, p.current || "");
    });
  } catch (e) {
    console.warn("[dsh-vault] op-progress 监听失败", e);
  }
}
// 包装一个耗时操作：显示浮层 + 执行 + 隐藏
async function withOpProgress(title, fn) {
  showOpProgress(title);
  updateOpProgress(0, 0, "准备中…");
  try {
    return await fn();
  } finally {
    hideOpProgress();
  }
}

// ===================== v3：接管 / 深度扫描 / 切换 =====================


// 拉取每个 dsh-home 的接管状态
async function refreshAdoptStatus() {
  const repo = state.defaultRepo;
  const jobs = state.homes
    .filter(h => h.kind === 'dsh-home')
    .map(async (h) => {
      try {
        state.adoptStatus[h.path] = await invoke('get_adopt_status', { repo, homePath: h.path });
      } catch (e) {
        state.adoptStatus[h.path] = { adopted: false, record: null };
      }
    });
  await Promise.all(jobs);
}

function adoptBadge(home) {
  const st = state.adoptStatus[home.path];
  if (home.kind === 'agents-home') return ['success', '共享技能库'];
  if (st && st.adopted) return ['success', '已接管'];
  return ['neutral', '未接管'];
}

// 环境页卡片渲染（覆盖原 renderHealth 的 home-list 部分）
function renderEnvironment() {
  renderHealth(); // 复用统计区
  // 渲染接管状态到每张卡片
  document.querySelectorAll('#home-list .home-row').forEach((row, idx) => {
    // renderHealth 已生成基础卡片，这里补接管徽标与按钮
  });
  // 直接重渲 home-list 以注入接管 UI
  $('home-list').innerHTML = state.homes.map((home, idx) => {
    const [tone, label] = healthBadge(home);
    const [atone, alabel] = adoptBadge(home);
    const isDsh = home.kind === 'dsh-home';
    const missing = !!home.__missing;
    const adopted = !missing && state.adoptStatus[home.path]?.adopted;
    const btn = isDsh && !missing
      ? (adopted
          ? `<button class="button compact" data-unadopt="${escapeHtml(home.path)}">断开接管</button>`
          : `<button class="button compact primary" data-adopt="${escapeHtml(home.path)}">一键接管</button>`)
      : (missing ? `<span class="badge muted">已消失</span>` : '');
    return `
      <article class="home-row stagger-item" style="--i:${idx + 4}">
        <div>
          <strong>${escapeHtml(home.label)}</strong>
          <code>${escapeHtml(home.path)}</code>
        </div>
        <div class="home-stats">
          <div>会话 <b>${home.sessions.total}</b>（v0 ${home.sessions.v0} / v4 ${home.sessions.v4}）</div>
          <div>备份文件 <b>${home.backupFileCount}</b>，约 <b>${formatBytes(home.backupSize)}</b></div>
        </div>
        <div class="home-badges">
          <span class="badge ${tone}">${label}</span>
          <span class="badge ${atone}">${alabel}</span>
        </div>
        <div class="home-actions">${btn}</div>
      </article>
    `;
  }).join('');
}

async function doAdopt(homePath) {
  const home = state.homes.find(h => h.path === homePath);
  // 先检测 DSH 是否在运行
  let running = [];
  try { running = await invoke('check_dsh_running'); } catch (e) {}
  if (running.length) {
    await confirmDialog({
      title: '请先关闭 DSH',
      message: `检测到 DSH 正在运行（${running.join('、')}）。接管需要移动文件，运行中可能导致失败或数据损坏。`,
      items: ['请先完全关闭所有 DSH 窗口，再回来点「一键接管」。'],
      danger: false,
      confirmText: '我知道了',
    });
    return;
  }
  const note = await promptDialog({
    title: `接管「${home?.label || '这个环境'}」`,
    message: '给这次接管加个备注（选填），方便以后辨认，比如"主环境接管"。',
    defaultValue: home?.label || '',
    placeholder: '备注（可直接确定跳过）',
  });
  if (note === null) return; // 用户取消
  const accepted = await confirmDialog({
    title: '接管这个环境',
    message: `将把「${home?.label}」的对话、技能、配置等移到统一仓库管理，原位置只留下一个链接（不占双份空间）。`,
    items: [
      '接管后，DSH 仍然正常读写这些文件（通过链接）。',
      '你可以随时「断开接管」，把文件原样搬回去。',
      'profiles（插件运行环境，约 800MB）不会接管，因为它可以自动重建。',
    ],
  });
  if (!accepted) return;
  try {
    const result = await withOpProgress(`正在接管「${home?.label}」`, () =>
      invoke('adopt_home', { repo: state.defaultRepo, homePath, note: note || '' }));
    showResult($('env-result'), `接管完成：移动了 ${result.movedDirs.length} 个目录（${result.movedDirs.join('、') || '无'}），创建了 ${result.createdLinks.length} 个链接${result.profileDeclFiles ? `，备份了 ${result.profileDeclFiles} 个插件声明文件` : ''}。`, false);
    await refreshAdoptStatus();
    renderEnvironment();
    await checkMismatch();
    backgroundRescan(); // 接管后后台刷新缓存
  } catch (e) {
    showResult($('env-result'), `接管失败：${e.message}`, true);
  }
}

async function doUnadopt(homePath) {
  const home = state.homes.find(h => h.path === homePath);
  let running = [];
  try { running = await invoke('check_dsh_running'); } catch (e) {}
  if (running.length) {
    await confirmDialog({
      title: '请先关闭 DSH',
      message: `检测到 DSH 正在运行（${running.join('、')}）。断开接管需要移动文件，请先关闭。`,
      items: ['请先完全关闭所有 DSH 窗口。'],
      danger: false,
      confirmText: '我知道了',
    });
    return;
  }
  const accepted = await confirmDialog({
    title: '断开接管',
    message: `将把「${home?.label}」的文件从仓库搬回原位置，并删除链接。`,
    items: ['搬回后，这个环境恢复为普通环境，不再集中管理。', '仓库中这次的接管记录会被清除。'],
  });
  if (!accepted) return;
  try {
    const result = await withOpProgress(`正在断开接管「${home?.label}」`, () =>
      invoke('unadopt_home', { repo: state.defaultRepo, homePath }));
    showResult($('env-result'), `已断开接管：还原了 ${result.restoredDirs.length} 个目录（${result.restoredDirs.join('、') || '无'}）。`, false);
    await refreshAdoptStatus();
    renderEnvironment();
    await checkMismatch();
    backgroundRescan(); // 断开接管后后台刷新缓存
  } catch (e) {
    showResult($('env-result'), `断开接管失败：${e.message}`, true);
  }
}

// 操作成功后后台静默刷新扫描缓存（不阻塞用户、不显示 busy 状态）。
function backgroundRescan() {
  try { scan_silent(); } catch (e) {}
}

async function scan_silent() {
  try {
    const result = await invoke('scan_homes');
    state.homes = result.homes;
    state.defaultRepo = result.defaultRepo;
    await refreshAdoptStatus();
    renderEnvironment();
    fillHomeSelects();
  } catch (e) { /* 后台刷新失败不影响当前操作 */ }
}

async function deepScan() {
  const btn = $('deep-scan');
  const cancelBtn = $('cancel-scan');
  setBusy(btn, true, '深度扫描中…');
  if (cancelBtn) cancelBtn.hidden = false;
  try {
    const result = await invoke('deep_scan', { deep: true });
    state.homes = result.homes;
    state.defaultRepo = result.defaultRepo;
    await refreshAdoptStatus();
    renderEnvironment();
    await checkMismatch();
    fillHomeSelects();
    updateBackupHeroStats();
    renderTmCurrent();
    $('scan-time').textContent = new Date().toLocaleString();
    showResult($('env-result'), `深度扫描完成：识别到 ${state.homes.length} 个环境。`, false);
  } catch (e) {
    showResult($('env-result'), `深度扫描失败：${e.message}`, true);
  } finally {
    setBusy(btn, false);
    if (cancelBtn) cancelBtn.hidden = true;
  }
}



// ---------- 操作日志 ----------
const OP_LABELS = {
  scan: '扫描', backup: '备份', restore: '恢复', adopt: '接管', unadopt: '断开接管',
  switch: '切换', repair: '修复', export: '导出', import: '导入', undo: '撤回', verify: '校验',
};

function renderOpLogs(entries, sizeBytes) {
  const list = $('oplog-list');
  const summary = $('oplog-summary');
  if (summary) summary.textContent = `${entries.length} 条 · ${formatBytes(sizeBytes)}`;
  if (!entries || !entries.length) {
    list.innerHTML = '<div class="empty-state"><strong>还没有操作记录。</strong><br>做过一次扫描或备份后，这里会出现记录。</div>';
    return;
  }
  list.innerHTML = entries.map((e) => {
    const op = OP_LABELS[e.op] || e.op;
    const badge = e.status === 'ok' ? '成功' : (e.status === 'fail' ? '失败' : '警告');
    const tone = e.status === 'ok' ? 'ok' : (e.status === 'fail' ? 'fail' : 'warn');
    const esc = (v) => String(v == null ? '' : v).replace(/[&<>"]/g, (c) => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c]));
    return `
      <div class="oplog-item">
        <span class="oplog-time">${esc(e.time)}</span>
        <span class="oplog-op">${esc(op)}</span>
        <div class="oplog-main">
          <span class="subject">${esc(e.subject || '—')}</span>
          ${e.detail ? `<span class="detail">${esc(e.detail)}</span>` : ''}
        </div>
        <span class="oplog-badge ${tone}">${badge}</span>
        ${e.error ? `<div class="oplog-error">${esc(e.error)}</div>` : ''}
      </div>`;
  }).join('');
}

async function refreshOpLogs() {
  try {
    const res = await invoke('list_op_logs');
    renderOpLogs(res.entries || [], res.sizeBytes || 0);
  } catch (e) {
    const list = $('oplog-list');
    if (list) list.innerHTML = `<div class="empty-state"><strong>读取日志失败。</strong><br>${e.message || e}</div>`;
  }
}

async function exportOpLogs() {
  const btn = $('export-oplogs');
  const target = await selectSaveFile('导出排障报告', [{ name: 'Markdown 报告', extensions: ['md'] }]);
  if (!target) return;
  const path = target.toLowerCase().endsWith('.md') ? target : `${target}.md`;
  setBusy(btn, true, '导出中…');
  try {
    const written = await invoke('export_op_logs', { target: path });
    showResult($('logs-result'), `报告已导出：${written}`, false);
  } catch (e) {
    showResult($('logs-result'), `导出失败：${e.message || e}`, true);
  } finally {
    setBusy(btn, false);
  }
}

// ---------- MCP 接入配置 ----------
function renderMcpConfig() {
  const el = $("mcp-config-text");
  if (!el) return;
  // 用当前 exe 路径生成 MCP 配置
  const exePath = "dsh-vault"; // 安装后在 PATH；否则用绝对路径
  const config = {
    mcpServers: {
      "dsh-vault": {
        command: "dsh-vault",
        args: ["--mcp"]
      }
    }
  };
  el.textContent = JSON.stringify(config, null, 2);
}

// ---------- 错位检测与一键归位 ----------
async function checkMismatch() {
  const banner = $("mismatch-banner");
  const repo = state.defaultRepo;
  let totalBroken = 0;
  const brokenHomes = [];
  const jobs = state.homes
    .filter(h => h.kind === "dsh-home" && state.adoptStatus[h.path]?.adopted)
    .map(async (h) => {
      try {
        const preview = await invoke("preview_repair", { repo, homePath: h.path });
        if (preview.brokenCount > 0) {
          totalBroken += preview.brokenCount;
          brokenHomes.push({ home: h, preview });
        }
      } catch (e) { /* 未接管或无记录，忽略 */ }
    });
  await Promise.all(jobs);
  state.brokenHomes = brokenHomes;
  if (totalBroken > 0) {
    $("mismatch-title").textContent = `检测到 ${brokenHomes.length} 个环境内容错位（共 ${totalBroken} 项）`;
    banner.hidden = false;
  } else {
    banner.hidden = true;
  }
}

async function repairAll() {
  const broken = state.brokenHomes || [];
  if (!broken.length) return;
  // 检测 DSH 运行
  let running = [];
  try { running = await invoke("check_dsh_running"); } catch (e) {}
  if (running.length) {
    await confirmDialog({
      title: "请先关闭 DSH",
      message: `检测到 DSH 正在运行（${running.join("、")}）。归位需要改动链接，请先关闭。`,
      items: ["请先完全关闭所有 DSH 窗口。"],
      confirmText: "我知道了",
      danger: false,
    });
    return;
  }
  const names = broken.map(b => b.home.label).join("、");
  const accepted = await confirmDialog({
    title: "一键归位",
    message: `将让 ${broken.length} 个错位环境（${names}）的每个目录都读回自己的内容。`,
    items: [
      "归位后，每个环境打开看到的都是自己的对话和技能，不再串到别家。",
      "别家的内容仍在各自的存档里，不会被删除。",
      "操作前会自动给这些环境做一次完整备份（保险）。",
    ],
  });
  if (!accepted) return;

  const btn = $("repair-all");
  setBusy(btn, true, "正在备份…");
  try {
    // 先完整备份（保险）
    await withOpProgress("归位前自动备份", () =>
      invoke("backup", { repo: state.defaultRepo, note: "归位前自动备份", onlyConfig: false }));
    // 逐个修复
    const summaries = [];
    for (const b of broken) {
      setBusy(btn, true, `归位 ${b.home.label}…`);
      const r = await withOpProgress(`正在归位「${b.home.label}」`, () =>
        invoke("repair_links", { repo: state.defaultRepo, homePath: b.home.path }));
      const parts = [];
      if (r.repointed.length) parts.push(`归位 ${r.repointed.length} 项`);
      if (r.removed.length) parts.push(`移除多余链接 ${r.removed.length} 项`);
      if (r.restoredFromBackup.length) parts.push(`从保险找回 ${r.restoredFromBackup.length} 项`);
      summaries.push(`${b.home.label}：${parts.join("，") || "无需改动"}`);
    }
    showResult($("env-result"), `归位完成：\n${summaries.join("\n")}`, false);
    await refreshAdoptStatus();
    renderEnvironment();
    await checkMismatch();
  } catch (e) {
    showResult($("env-result"), `归位失败：${e.message}`, true);
  } finally {
    setBusy(btn, false);
  }
}

// ---------- 切换页 ----------
async function renderSwitch() {
  await refreshAdoptStatus();
  const adopted = state.homes.filter(h => h.kind === 'dsh-home' && state.adoptStatus[h.path]?.adopted);
  const empty = $('switch-empty');
  const stage = $('switch-stage');
  if (adopted.length < 2) {
    empty.hidden = false;
    stage.style.display = 'none';
    $('switch-config').hidden = true;
    return;
  }
  empty.hidden = true;
  stage.style.display = '';

  const card = (h, role) => `
    <button class="switch-card" data-role="${role}" data-path="${escapeHtml(h.path)}" type="button">
      <strong>${escapeHtml(h.label)}</strong>
      <span class="muted">${h.sessions.total} 个对话 · ${formatBytes(h.backupSize)}</span>
      <code>${escapeHtml(h.path)}</code>
    </button>`;
  $('switch-source-list').innerHTML = adopted.map(h => card(h, 'source')).join('');
  $('switch-target-list').innerHTML = adopted.map(h => card(h, 'target')).join('');
}

function updateSwitchRoute() {
  const cfg = $('switch-config');
  if (state.switchSource && state.switchTarget && state.switchSource !== state.switchTarget) {
    const s = state.homes.find(h => h.path === state.switchSource);
    const t = state.homes.find(h => h.path === state.switchTarget);
    $('switch-route-source').textContent = s?.label || '';
    $('switch-route-target').textContent = t?.label || '';
    cfg.hidden = false;
  } else {
    cfg.hidden = true;
  }
  // 卡片选中态
  document.querySelectorAll('.switch-card').forEach(c => {
    const isSel = (c.dataset.role === 'source' && c.dataset.path === state.switchSource) ||
                  (c.dataset.role === 'target' && c.dataset.path === state.switchTarget);
    c.classList.toggle('is-selected', isSel);
    // 同源同标禁止
    if (c.dataset.role === 'target' && c.dataset.path === state.switchSource) {
      c.classList.add('is-disabled');
    } else if (c.dataset.role === 'source' && c.dataset.path === state.switchTarget) {
      c.classList.add('is-disabled');
    } else {
      c.classList.remove('is-disabled');
    }
  });
}

async function doSwitch() {
  const s = state.homes.find(h => h.path === state.switchSource);
  const t = state.homes.find(h => h.path === state.switchTarget);
  let running = [];
  try { running = await invoke('check_dsh_running'); } catch (e) {}
  if (running.length) {
    await confirmDialog({
      title: '请先关闭 DSH',
      message: `检测到 DSH 正在运行（${running.join('、')}）。切换需要改动链接，请先关闭。`,
      items: ['请先完全关闭所有 DSH 窗口。'],
      danger: false,
      confirmText: '我知道了',
    });
    return;
  }
  const types = [];
  if ($('sw-sessions').checked) types.push('对话记录');
  if ($('sw-skills').checked) types.push('技能');
  if ($('sw-config').checked) types.push('配置');
  if ($('sw-memories').checked) types.push('记忆');
  if (!types.length) {
    showResult($('switch-result'), '请至少勾选一种要切换的内容类型。', true);
    return;
  }
  const accepted = await confirmDialog({
    title: '确认切换',
    message: `将把「${s?.label}」的${types.join('、')}切换到「${t?.label}」。`,
    items: [
      `切换后，打开「${t?.label}」会看到「${s?.label}」的${types.join('、')}。`,
      `会先给「${t?.label}」保存一份保险快照，万一不对可以切回来。`,
      '请确认所有 DSH 窗口已关闭。',
    ],
  });
  if (!accepted) return;
  const btn = $('switch-execute');
  setBusy(btn, true, '切换中…');
  try {
    const result = await withOpProgress(`正在切换到「${t?.label}」`, () => invoke('switch_links', {
      repo: state.defaultRepo,
      sourceHome: state.switchSource,
      targetHome: state.switchTarget,
      includeSessions: $('sw-sessions').checked,
      includeSkills: $('sw-skills').checked,
      includeConfig: $('sw-config').checked,
      includeMemories: $('sw-memories').checked,
    }));
    showResult($('switch-result'), `切换完成：${result.switchedLinks} 类内容已从「${s?.label}」切到「${t?.label}」。目标环境的原内容已存入保险快照「${result.backupSnapshot}」。`, false);
    await renderSwitch();
    updateSwitchRoute();
    backgroundRescan(); // 切换后后台刷新缓存
  } catch (e) {
    showResult($('switch-result'), `切换失败：${e.message}`, true);
  } finally {
    setBusy(btn, false);
  }
}

function showView(name) {
  document.querySelectorAll('.nav-item').forEach(v => v.classList.remove('is-active'));
  document.querySelectorAll('.view').forEach(v => v.classList.remove('is-active'));
  const nav = document.querySelector(`.nav-item[data-view="${name}"]`);
  if (nav) nav.classList.add('is-active');
  const view = $(`view-${name}`);
  if (view) view.classList.add('is-active');
}

function bindNavigation() {
  document.querySelectorAll('.nav-item').forEach((item) => {
    item.addEventListener('click', () => showView(item.dataset.view));
  });
  // 存档页子标签
  document.querySelectorAll('.subtab').forEach((tab) => {
    tab.addEventListener('click', () => {
      document.querySelectorAll('.subtab').forEach(t => t.classList.remove('is-active'));
      document.querySelectorAll('.subpanel').forEach(p => p.classList.remove('is-active'));
      tab.classList.add('is-active');
      $(`subpanel-${tab.dataset.subtab}`).classList.add('is-active');
    });
  });
}

document.addEventListener('DOMContentLoaded', async () => {
  bindNavigation();
  setupProgressListeners();
  // 新手引导「知道了」：关闭并记住（原先这段代码误落在 undo() 函数体内，永远执行不到）。
  const dismissOnb = document.getElementById('dismiss-onboarding');
  if (dismissOnb) dismissOnb.addEventListener('click', () => {
    const onb = document.getElementById('onboarding');
    if (onb) onb.hidden = true;
    try { localStorage.setItem('dsh-vault-onboarding-dismissed', '1'); } catch (e) {}
  });
  try {
    if (localStorage.getItem('dsh-vault-onboarding-dismissed') === '1') {
      const onb = document.getElementById('onboarding');
      if (onb) onb.hidden = true;
    }
  } catch (e) {}
  // 先秒读缓存显示上次结果，再触发 scan() 后台刷新（见文件末尾）
  await loadScanCache();
  setupOpProgressListener();
  $('rescan').addEventListener('click', scan);
  $('pick-backup-repo').addEventListener('click', async () => {
    const path = await selectDirectory('选择备份仓库');
    if (path) $('backup-repo').value = path;
  });
  $('pick-restore-repo').addEventListener('click', async () => {
    const path = await selectDirectory('选择要恢复的备份仓库');
    if (path) $('restore-repo').value = path;
  });
  $('pick-repository-repo').addEventListener('click', async () => {
    const path = await selectDirectory('选择当前仓库');
    if (path) $('repository-repo').value = path;
  });
  $('pick-export-file').addEventListener('click', async () => {
    const path = await selectSaveFile('导出备份包', [{ name: 'DSH Vault 备份包', extensions: ['zip'] }]);
    if (path) $('export-file').value = path;
  });
  $('pick-import-archive').addEventListener('click', async () => {
    const path = await selectOpenFile('选择备份包', [{ name: 'DSH Vault 备份包', extensions: ['zip'] }]);
    if (path) $('import-archive').value = path;
  });
  $('pick-import-repo').addEventListener('click', async () => {
    const path = await selectDirectory('选择导入目标目录（必须为空）');
    if (path) $('import-repo').value = path;
  });
  $('start-backup').addEventListener('click', startBackup);
  $('goto-timemachine').addEventListener('click', () => {
    showView('archive');
    document.querySelector('.subtab[data-subtab="timemachine"]')?.click();
  });
  $('backup-again').addEventListener('click', () => {
    $('backup-success').hidden = true;
    $('backup-note').focus();
  });
  $('verify-repo').addEventListener('click', () => verifyRepo($('backup-repo').value, $('backup-result')));
  $('repo-verify').addEventListener('click', () => verifyRepo($('repository-repo').value, $('repo-result')));
  $('load-catalog').addEventListener('click', loadCatalog);
  $('preview-restore').addEventListener('click', previewRestore);
  $('execute-restore').addEventListener('click', executeRestore);
  $('export-backup').addEventListener('click', exportBackup);
  $('import-backup').addEventListener('click', importBackup);
  $('load-rollback-ledgers').addEventListener('click', loadRollbackLedgers);
  // 时光机
  $('refresh-snapshots').addEventListener('click', refreshSnapshots);
  $('tm-backup-now').addEventListener('click', () => {
    showView('archive');
    document.querySelector('.subtab[data-subtab="backup"]')?.click();
    setTimeout(() => $('start-backup')?.click(), 300);
  });
  $('tm-detail-close').addEventListener('click', () => { $('tm-detail').hidden = true; tmSelectedSnapshot = null; document.querySelectorAll('.tm-snapshot-card').forEach(c => c.classList.remove('is-selected')); });
  $('tm-restore-btn').addEventListener('click', tmRestoreSnapshot);
  $('tm-export-btn').addEventListener('click', () => {
    showView('repository');
  });

  document.getElementById("rollback-ledger-list").addEventListener("click", (event) => {
    const button = event.target.closest("button[data-ledger]");
    if (button) previewCompensate(button.dataset.ledger);
  });
  $('undo').addEventListener('click', undo);

  // v3：环境页
  $('deep-scan').addEventListener('click', deepScan);
  // 日志页
  $('refresh-oplogs')?.addEventListener('click', refreshOpLogs);
  $('export-oplogs')?.addEventListener('click', exportOpLogs);
  $('clear-oplogs')?.addEventListener('click', async () => {
    const ok = await confirmDialog({
      title: '清空操作日志',
      message: '将删除本机保存的全部操作记录。此操作不影响任何备份与环境文件，只是清空日志。',
      items: ['日志清空后无法再导出历史排障报告。'],
      confirmText: '确认清空',
    });
    if (!ok) return;
    try { await invoke('clear_op_logs'); await refreshOpLogs(); } catch (e) {}
  });
  // 进入日志页时刷新
  document.querySelector('.nav-item[data-view="logs"]')?.addEventListener('click', () => {
    setTimeout(refreshOpLogs, 0);
  });
  $('cancel-scan')?.addEventListener('click', async () => {
    try { await invoke('cancel_operation'); } catch (e) {}
  });
  $('repair-all').addEventListener('click', repairAll);
  $('copy-mcp-config')?.addEventListener('click', async () => {
    const text = $('mcp-config-text')?.textContent || '';
    try {
      await navigator.clipboard.writeText(text);
      $('copy-mcp-config').textContent = '已复制';
      setTimeout(() => { $('copy-mcp-config').textContent = '复制配置'; }, 1500);
    } catch (e) {
      showResult($('repo-result'), '复制失败：' + e.message, true);
    }
  });
  renderMcpConfig();
  $('home-list').addEventListener('click', (e) => {
    const adoptBtn = e.target.closest('[data-adopt]');
    const unadoptBtn = e.target.closest('[data-unadopt]');
    if (adoptBtn) doAdopt(adoptBtn.dataset.adopt);
    if (unadoptBtn) doUnadopt(unadoptBtn.dataset.unadopt);
  });
  // 高级操作 → 恢复视图
  $('open-advanced')?.addEventListener('click', () => showView('restore'));
  // v3：切换页
  $('switch-stage').addEventListener('click', (e) => {
    const card = e.target.closest('.switch-card');
    if (!card || card.classList.contains('is-disabled')) return;
    if (card.dataset.role === 'source') state.switchSource = card.dataset.path;
    if (card.dataset.role === 'target') state.switchTarget = card.dataset.path;
    updateSwitchRoute();
  });
  $('switch-reset').addEventListener('click', () => {
    state.switchSource = null; state.switchTarget = null;
    updateSwitchRoute();
  });
  $('switch-execute').addEventListener('click', doSwitch);
  // 进入切换页时刷新
  document.querySelector('.nav-item[data-view="switch"]')?.addEventListener('click', () => {
    setTimeout(renderSwitch, 0);
  });

  scan();
});


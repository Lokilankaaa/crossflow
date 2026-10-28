'use strict';
(() => {
  const $ = id => document.getElementById(id);
  const NS = 'http://www.w3.org/2000/svg';
  const STORAGE = 'crossflow.layout-draft.v1';
  const COLORS = [['#8074ce', '#f0edf9'], ['#6e9fbc', '#edf4f8'], ['#b78d73', '#f8f0eb'], ['#7fa598', '#eef5f2'], ['#b485ab', '#f7eef5']];
  const initial = { screens: [
    { id: 'desktop-main', device_id: 'windows-desktop', x: 0, y: 0, width: 1920, height: 1080 },
    { id: 'macbook-display', device_id: 'macbook-air', x: 1920, y: 124, width: 1470, height: 956 }
  ] };
  let model = structuredClone(initial);
  let selected = model.screens[0].id;
  let drag = null;
  let view = { x: -300, y: -350, w: 4000, h: 1800 };
  const copy = value => structuredClone(value);
  const devices = () => [...new Set(model.screens.map(s => s.device_id))];
  const color = device => COLORS[devices().indexOf(device) % COLORS.length];
  const overlap = (a, b) => a.x < b.x + b.width && a.x + a.width > b.x && a.y < b.y + b.height && a.y + a.height > b.y;
  function validate(data, allowEmpty = false) {
    if (!data || typeof data !== 'object' || Array.isArray(data) || !Array.isArray(data.screens)) throw new Error('布局必须包含 screens 数组。');
    if (Object.keys(data).some(key => key !== 'screens')) throw new Error('布局只支持 screens 字段。');
    if (!allowEmpty && !data.screens.length) throw new Error('布局至少需要一块屏幕。');
    if (data.screens.length > 256) throw new Error('最多支持 256 块屏幕。');
    const seen = new Set();
    for (const s of data.screens) {
      if (!s || typeof s !== 'object' || Array.isArray(s)) throw new Error('屏幕数据必须是对象。');
      const keys = ['id', 'device_id', 'x', 'y', 'width', 'height'];
      if (Object.keys(s).length !== keys.length || !keys.every(k => Object.hasOwn(s, k))) throw new Error('每块屏幕必须且只能包含 id、device_id、x、y、width、height。');
      for (const key of ['id', 'device_id']) {
        if (typeof s[key] !== 'string' || !s[key].trim() || new TextEncoder().encode(s[key]).length > 128) throw new Error('屏幕和设备标识不能为空，且 UTF-8 长度不能超过 128 字节。');
      }
      if (seen.has(s.id)) throw new Error(`屏幕标识重复：${s.id}`);
      seen.add(s.id);
      for (const key of ['x', 'y', 'width', 'height']) if (typeof s[key] !== 'number' || !Number.isFinite(s[key])) throw new Error('位置和尺寸必须是有限数字。');
      if (Math.abs(s.x) > 1000000 || Math.abs(s.y) > 1000000) throw new Error('坐标范围为 -1,000,000 至 1,000,000。');
      if (s.width <= 0 || s.height <= 0 || s.width > 32768 || s.height > 32768) throw new Error('宽高必须大于 0，且不超过 32,768 个逻辑像素。');
      if (!Number.isFinite(s.x + s.width) || !Number.isFinite(s.y + s.height) || s.x + s.width <= s.x || s.y + s.height <= s.y) throw new Error('屏幕尺寸相对于坐标过小，无法表示有效边缘。');
    }
    for (let i = 0; i < data.screens.length; i++) for (let j = i + 1; j < data.screens.length; j++) {
      if (overlap(data.screens[i], data.screens[j])) throw new Error(`${data.screens[i].id} 与 ${data.screens[j].id} 重叠。请调整位置。`);
    }
    return data;
  }
  function announce(message, error = false) { $('notice').textContent = message; $('notice').classList.toggle('error', error); }
  function persist() {
    try { localStorage.setItem(STORAGE, JSON.stringify(model)); $('save-status').textContent = '已保存至本机'; return true; }
    catch { $('save-status').textContent = '未保存'; announce('浏览器无法保存草稿。请导出 JSON 留存当前布局。', true); return false; }
  }
  function setModel(next, message, fit = false) {
    try { validate(next, true); } catch (error) { announce(error.message, true); return false; }
    model = next;
    if (!model.screens.some(s => s.id === selected)) selected = model.screens[0]?.id ?? null;
    const saved = persist(); render(); if (fit) fitCanvas(); if (message && saved) announce(message); return true;
  }
  function svgElement(tag, attrs, text) {
    const element = document.createElementNS(NS, tag);
    for (const [key, value] of Object.entries(attrs)) element.setAttribute(key, String(value));
    if (text !== undefined) element.textContent = text;
    return element;
  }
  function renderScreens(invalid = false) {
    const container = $('screens'); container.replaceChildren();
    for (const s of model.screens) {
      const [fill] = color(s.device_id);
      const font = Math.max(15, Math.min(s.width / 11, s.height / 9, 60));
      const label = s.id.length > 22 ? s.id.slice(0, 20) + '…' : s.id;
      const group = svgElement('g', { transform: `translate(${s.x} ${s.y})`, class: `screen-node${s.id === selected ? ' selected' : ''}${invalid && s.id === selected ? ' invalid' : ''}`, tabindex: '0', role: 'button', 'aria-pressed': s.id === selected, 'aria-label': `${s.id}，设备 ${s.device_id}，位置 ${s.x}, ${s.y}，尺寸 ${s.width} × ${s.height}`, 'data-id': s.id });
      group.append(svgElement('rect', { x: 0, y: 0, width: s.width, height: s.height, rx: Math.min(28, s.height / 12), fill, class: 'screen-border' }));
      group.append(svgElement('rect', { x: s.width / 2 - s.width / 12, y: s.height + 16, width: s.width / 6, height: 12, rx: 6, class: 'screen-base' }));
      group.append(svgElement('text', { x: s.width / 2, y: s.height / 2 - font * .7, 'text-anchor': 'middle', 'font-size': font, class: 'screen-label' }, label));
      group.append(svgElement('text', { x: s.width / 2, y: s.height / 2 + font * .65, 'text-anchor': 'middle', 'font-size': font * .65, class: 'screen-meta' }, `${s.width} × ${s.height}`));
      group.append(svgElement('text', { x: s.width / 2, y: s.height - font * 1.2, 'text-anchor': 'middle', 'font-size': font * .48, class: 'screen-device' }, s.device_id.length > 25 ? s.device_id.slice(0, 23) + '…' : s.device_id));
      container.append(group);
    }
    $('empty-state').hidden = model.screens.length !== 0;
  }
  function renderProperties() {
    const s = model.screens.find(s => s.id === selected);
    $('properties').hidden = !s; $('no-selection').hidden = !!s;
    if (!s) return;
    $('selected-id').textContent = s.id; $('selected-device').textContent = s.device_id;
    for (const key of ['device', 'x', 'y', 'width', 'height']) $('prop-' + key).value = s[key === 'device' ? 'device_id' : key];
  }
  function renderDevices() {
    $('device-list').replaceChildren(); $('device-options').replaceChildren();
    for (const device of devices()) {
      const [main, light] = color(device);
      const card = document.createElement('article'); card.className = 'device-card'; card.style.setProperty('--device-color', main); card.style.setProperty('--device-light', light);
      const icon = document.createElement('span'); icon.className = 'device-icon'; icon.textContent = '▣'; icon.setAttribute('aria-hidden', 'true');
      const text = document.createElement('div'); text.className = 'device-copy';
      const title = document.createElement('strong'); title.textContent = device;
      const detail = document.createElement('small'); detail.textContent = `${model.screens.filter(s => s.device_id === device).length} 块屏幕 · 布局设备`;
      text.append(title, detail); const tag = document.createElement('span'); tag.className = 'draft-tag'; tag.textContent = '草稿'; card.append(icon, text, tag); $('device-list').append(card);
      const option = document.createElement('option'); option.value = device; $('device-options').append(option);
    }
    if (!devices().length) { const empty = document.createElement('p'); empty.className = 'field-note'; empty.textContent = '添加屏幕后，所属设备会显示在这里。'; $('device-list').append(empty); }
    $('device-count').textContent = `${devices().length} 台`;
    $('screen-count').textContent = `${model.screens.length} 块屏幕`;
  }
  function render() { renderScreens(); renderProperties(); renderDevices(); $('export-button').disabled = model.screens.length === 0; }
  function fitCanvas() {
    const screens = model.screens;
    if (!screens.length) view = { x: -240, y: -240, w: 4000, h: 1700 };
    else {
      const left = Math.min(...screens.map(s => s.x)), top = Math.min(...screens.map(s => s.y));
      const right = Math.max(...screens.map(s => s.x + s.width)), bottom = Math.max(...screens.map(s => s.y + s.height));
      const w = right - left, h = bottom - top, pad = Math.max(w, h) * .12 + 60;
      view = { x: left - pad, y: top - pad, w: w + pad * 2, h: h + pad * 2 };
    }
    $('canvas').setAttribute('viewBox', `${view.x} ${view.y} ${view.w} ${view.h}`);
  }
  function world(event) { const point = new DOMPoint(event.clientX, event.clientY); return point.matrixTransform($('canvas').getScreenCTM().inverse()); }
  function focusScreen(id) { [...$('screens').children].find(node => node.dataset.id === id)?.focus({ preventScroll: true }); }
  function select(id, focus = false) { selected = id; renderScreens(); renderProperties(); if (focus) focusScreen(id); }
  function snapPosition(s) {
    if (!$('snap').checked) return;
    const scale = $('canvas').getScreenCTM()?.a || 1;
    const threshold = 10 / scale;
    let bestX = threshold, bestY = threshold, snapX = s.x, snapY = s.y;
    for (const other of model.screens) if (other.id !== s.id) {
      for (const position of [other.x, other.x + other.width, other.x - s.width, other.x + other.width - s.width]) if (Math.abs(position - s.x) < bestX) { bestX = Math.abs(position - s.x); snapX = position; }
      for (const position of [other.y, other.y + other.height, other.y - s.height, other.y + other.height - s.height]) if (Math.abs(position - s.y) < bestY) { bestY = Math.abs(position - s.y); snapY = position; }
    }
    s.x = snapX; s.y = snapY;
  }
  $('canvas').addEventListener('pointerdown', event => {
    if (event.button !== 0 || drag) return;
    const node = event.target.closest('.screen-node'); if (!node) return;
    event.preventDefault(); selected = node.dataset.id;
    const s = model.screens.find(s => s.id === selected);
    drag = { id: selected, pointer: event.pointerId, start: world(event), original: copy(model), x: s.x, y: s.y };
    $('canvas').setPointerCapture(event.pointerId); renderScreens(); renderProperties();
  });
  $('canvas').addEventListener('pointermove', event => {
    if (!drag || event.pointerId !== drag.pointer) return;
    const point = world(event), s = model.screens.find(s => s.id === drag.id);
    s.x = Math.round(drag.x + point.x - drag.start.x); s.y = Math.round(drag.y + point.y - drag.start.y); snapPosition(s);
    let invalid = false; try { validate(model); } catch { invalid = true; }
    renderScreens(invalid); renderProperties();
  });
  function finishDrag(cancel) {
    if (!drag) return;
    const previous = drag; drag = null;
    if ($('canvas').hasPointerCapture(previous.pointer)) $('canvas').releasePointerCapture(previous.pointer);
    if (cancel) { model = previous.original; render(); announce('已取消移动。'); }
    else {
      try { validate(model); const saved = persist(); render(); if (saved) announce('屏幕位置已更新，仅保存为本地草稿。'); }
      catch (error) { model = previous.original; render(); announce(error.message + ' 已恢复移动前的位置。', true); }
    }
    focusScreen(selected);
  }
  $('canvas').addEventListener('pointerup', event => { if (drag?.pointer === event.pointerId) finishDrag(false); });
  $('canvas').addEventListener('pointercancel', () => finishDrag(true));
  $('canvas').addEventListener('lostpointercapture', () => { if (drag) finishDrag(true); });
  document.addEventListener('keydown', event => { if (event.key === 'Escape' && drag) { event.preventDefault(); finishDrag(true); } });
  $('canvas').addEventListener('keydown', event => {
    const node = event.target.closest('.screen-node'); if (!node || drag) return;
    if (event.key === 'Enter' || event.key === ' ') { event.preventDefault(); select(node.dataset.id, true); return; }
    const direction = { ArrowLeft: [-1, 0], ArrowRight: [1, 0], ArrowUp: [0, -1], ArrowDown: [0, 1] }[event.key];
    if (!direction) return;
    event.preventDefault(); selected = node.dataset.id;
    const next = copy(model), s = next.screens.find(s => s.id === selected), step = event.altKey ? 1 : event.shiftKey ? 50 : 10;
    s.x += direction[0] * step; s.y += direction[1] * step;
    setModel(next, `位置更新为 ${s.x}, ${s.y}。`); focusScreen(selected);
  });
  $('properties').addEventListener('submit', event => {
    event.preventDefault(); const next = copy(model), s = next.screens.find(s => s.id === selected); if (!s) return;
    s.device_id = $('prop-device').value.trim();
    for (const key of ['x', 'y', 'width', 'height']) s[key] = Number($('prop-' + key).value);
    setModel(next, '屏幕属性已更新。', true);
  });
  $('delete-screen').addEventListener('click', () => {
    const removed = selected; setModel({ screens: model.screens.filter(s => s.id !== selected) }, `已从草稿移除 ${removed}。`, true);
  });
  function openAdd() {
    let n = model.screens.length + 1; while (model.screens.some(s => s.id === `screen-${n}`)) n++;
    $('new-id').value = `screen-${n}`; $('new-device').value = devices()[0] ?? 'my-device'; $('dialog-error').textContent = ''; $('add-dialog').showModal(); $('new-id').focus(); $('new-id').select();
  }
  $('add-screen').addEventListener('click', openAdd); $('empty-add').addEventListener('click', openAdd);
  for (const id of ['close-dialog', 'cancel-dialog']) $(id).addEventListener('click', () => $('add-dialog').close());
  $('add-form').addEventListener('submit', event => {
    event.preventDefault(); const next = copy(model);
    const s = { id: $('new-id').value.trim(), device_id: $('new-device').value.trim(), x: next.screens.length ? Math.max(...next.screens.map(s => s.x + s.width)) : 0, y: 0, width: Number($('new-width').value), height: Number($('new-height').value) };
    next.screens.push(s);
    try { validate(next); } catch (error) { $('dialog-error').textContent = error.message; return; }
    selected = s.id; setModel(next, `已添加 ${s.id}。`, true); $('add-dialog').close(); focusScreen(selected);
  });
  $('fit').addEventListener('click', () => { fitCanvas(); announce('画布已调整，显示全部屏幕。'); });
  $('export-button').addEventListener('click', () => {
    try { validate(model); } catch (error) { announce(error.message, true); return; }
    const blob = new Blob([JSON.stringify(model, null, 2) + '\n'], { type: 'application/json' });
    const url = URL.createObjectURL(blob), anchor = document.createElement('a'); anchor.href = url; anchor.download = 'crossflow-layout.json'; anchor.click(); setTimeout(() => URL.revokeObjectURL(url), 1000); announce('已导出布局 JSON；导出不会将布局应用到系统。');
  });
  $('import-button').addEventListener('click', () => $('import-file').click());
  $('import-file').addEventListener('change', async event => {
    const file = event.target.files[0]; if (!file) return;
    try {
      if (file.size > 1024 * 1024) throw new Error('JSON 文件不能超过 1 MB。');
      const data = JSON.parse(await file.text()); validate(data); setModel(copy(data), '布局已导入并保存为本地草稿。', true);
    } catch (error) { announce(`导入失败：${error instanceof SyntaxError ? '文件不是有效 JSON。' : error.message} 原布局保持不变。`, true); }
    event.target.value = '';
  });
  const pageNames = { layout: '屏幕布局', settings: '输入设置', status: '连接状态' };
  function showPage(page) {
    if (!Object.hasOwn(pageNames, page)) page = 'layout';
    for (const key of Object.keys(pageNames)) $('page-' + key).hidden = key !== page;
    for (const button of document.querySelectorAll('[data-page]')) { const active = button.dataset.page === page; button.classList.toggle('active', active); if (active) button.setAttribute('aria-current', 'page'); else button.removeAttribute('aria-current'); }
    $('page-name').textContent = pageNames[page]; document.title = `Crossflow · ${pageNames[page]}`;
  }
  document.querySelectorAll('[data-page]').forEach(button => button.addEventListener('click', () => { location.hash = button.dataset.page; showPage(button.dataset.page); }));
  window.addEventListener('hashchange', () => showPage(location.hash.slice(1)));
  try {
    const saved = localStorage.getItem(STORAGE);
    if (saved !== null) { model = copy(validate(JSON.parse(saved), true)); selected = model.screens[0]?.id ?? null; $('save-status').textContent = '已恢复本地草稿'; announce('已恢复本地草稿。此布局尚未应用到共享内核。'); }
  } catch { announce('无法读取有效的本地草稿，已载入示例布局；旧存储暂未覆盖。', true); $('save-status').textContent = '草稿待保存'; }
  render(); fitCanvas(); showPage(location.hash.slice(1));
})();

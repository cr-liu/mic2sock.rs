const configDiv = document.getElementById('config');
const saveBtn = document.getElementById('save');
const reloadBtn = document.getElementById('reload');
const restartBtn = document.getElementById('restart');

let currentConfig = null;

async function loadConfig() {
  const res = await fetch('/api/config');
  if (!res.ok) { configDiv.textContent = 'Failed to load config'; return; }
  currentConfig = await res.json();
  // The server includes _meta; strip for rendering
  const meta = currentConfig._meta;
  delete currentConfig._meta;
  renderConfig(currentConfig);
}

function renderConfig(cfg) {
  configDiv.innerHTML = '';
  for (const [section, value] of Object.entries(cfg)) {
    const fs = document.createElement('fieldset');
    const lg = document.createElement('legend');
    lg.textContent = section;
    fs.appendChild(lg);

    if (Array.isArray(value)) {
      value.forEach((item, idx) => {
        if (typeof item === 'object' && item !== null) {
          const sub = document.createElement('fieldset');
          const subLg = document.createElement('legend');
          subLg.textContent = `${section}[${idx}]`;
          sub.appendChild(subLg);
          renderFields(sub, item, [section, idx]);
          fs.appendChild(sub);
        } else {
          // shouldn't happen at top level but handle gracefully
          renderField(fs, String(idx), item, [section, idx]);
        }
      });
    } else if (typeof value === 'object' && value !== null) {
      renderFields(fs, value, [section]);
    } else {
      renderField(fs, section, value, [section]);
    }
    configDiv.appendChild(fs);
  }
}

function renderFields(parent, obj, path) {
  for (const [key, val] of Object.entries(obj)) {
    if (Array.isArray(val)) {
      renderStringList(parent, key, val, [...path, key]);
    } else if (typeof val === 'object' && val !== null) {
      const sub = document.createElement('fieldset');
      const lg = document.createElement('legend');
      lg.textContent = key;
      sub.appendChild(lg);
      renderFields(sub, val, [...path, key]);
      parent.appendChild(sub);
    } else {
      renderField(parent, key, val, [...path, key]);
    }
  }
}

function renderField(parent, key, val, path) {
  const row = document.createElement('div');
  const label = document.createElement('label');
  label.textContent = key + ': ';
  const input = document.createElement('input');
  if (typeof val === 'boolean') {
    input.type = 'checkbox';
    input.checked = val;
  } else if (typeof val === 'number') {
    input.type = 'number';
    input.value = val;
  } else {
    input.type = 'text';
    input.value = val;
  }
  input.dataset.path = JSON.stringify(path);
  input.dataset.type = typeof val;
  row.appendChild(label);
  row.appendChild(input);
  parent.appendChild(row);
}

function renderStringList(parent, key, list, path) {
  const row = document.createElement('div');
  const label = document.createElement('div');
  label.textContent = key + ':';
  row.appendChild(label);
  const ul = document.createElement('ul');
  list.forEach((val, idx) => {
    const li = document.createElement('li');
    const input = document.createElement('input');
    input.type = 'text';
    input.value = val;
    input.dataset.path = JSON.stringify([...path, idx]);
    input.dataset.type = 'string';
    li.appendChild(input);
    const rm = document.createElement('button');
    rm.textContent = '×';
    rm.type = 'button';
    rm.onclick = () => { list.splice(idx, 1); renderConfig(currentConfig); };
    li.appendChild(rm);
    ul.appendChild(li);
  });
  const addBtn = document.createElement('button');
  addBtn.textContent = '+ Add';
  addBtn.type = 'button';
  addBtn.onclick = () => { list.push(''); renderConfig(currentConfig); };
  row.appendChild(ul);
  row.appendChild(addBtn);
  parent.appendChild(row);
}

function gatherConfig() {
  document.querySelectorAll('input[data-path]').forEach(inp => {
    const path = JSON.parse(inp.dataset.path);
    const type = inp.dataset.type;
    let val;
    if (type === 'boolean') val = inp.checked;
    else if (type === 'number') val = Number(inp.value);
    else val = inp.value;
    setByPath(currentConfig, path, val);
  });
  return currentConfig;
}

function setByPath(obj, path, val) {
  let cur = obj;
  for (let i = 0; i < path.length - 1; i++) cur = cur[path[i]];
  cur[path[path.length - 1]] = val;
}

saveBtn.onclick = async () => {
  const cfg = gatherConfig();
  const res = await fetch('/api/config', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(cfg),
  });
  const body = await res.json().catch(() => ({}));
  if (res.ok) {
    alert(`Saved: ${body.status}${body.restart_required ? ' (restart required)' : ''}`);
  } else {
    alert(`Error: ${JSON.stringify(body)}`);
  }
};

reloadBtn.onclick = () => loadConfig();

restartBtn.onclick = async () => {
  if (!confirm('Restart the binary?')) return;
  await fetch('/api/restart', { method: 'POST' });
  alert('Restart requested. Reload the page in a few seconds.');
};

loadConfig();

// ── Waveform canvas ──
const canvas = document.getElementById('waveform');
const ctx = canvas.getContext('2d');

const VIEW_SECONDS = 5;
const FRAMES_PER_SEC = 20;
const POINTS_PER_FRAME = 100;
const BUF_LEN = VIEW_SECONDS * FRAMES_PER_SEC * POINTS_PER_FRAME;
const pointBuf = new Int16Array(BUF_LEN);

function connectWaveformWS() {
  const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
  const ws = new WebSocket(`${proto}//${location.host}/ws/waveform`);
  ws.binaryType = 'arraybuffer';

  ws.onmessage = (ev) => {
    const view = new DataView(ev.data);
    const sampleCount = (ev.data.byteLength - 4) / 2;
    pointBuf.copyWithin(0, sampleCount);
    for (let i = 0; i < sampleCount; i++) {
      pointBuf[BUF_LEN - sampleCount + i] = view.getInt16(4 + i * 2, true);
    }
  };

  ws.onclose = () => setTimeout(connectWaveformWS, 3000);
  ws.onerror = () => ws.close();
}

function drawWaveform() {
  const w = canvas.width;
  const h = canvas.height;
  ctx.clearRect(0, 0, w, h);
  ctx.strokeStyle = '#2a9';
  ctx.beginPath();
  const step = w / BUF_LEN;
  for (let i = 0; i < BUF_LEN; i++) {
    const x = i * step;
    const y = h / 2 - (pointBuf[i] / 32768) * (h / 2);
    if (i === 0) ctx.moveTo(x, y); else ctx.lineTo(x, y);
  }
  ctx.stroke();
  requestAnimationFrame(drawWaveform);
}

connectWaveformWS();
requestAnimationFrame(drawWaveform);

// Webview side of the rift chat: renders server events streamed from the
// extension host and sends user input back. No frameworks — the protocol is
// small enough that direct DOM updates stay clear.
(function () {
  const vscode = acquireVsCodeApi();
  const messages = document.getElementById('messages');
  const input = document.getElementById('input');
  const statusEl = document.getElementById('status');
  const btnSend = document.getElementById('btn-send');
  const btnStop = document.getElementById('btn-stop');

  /** Open assistant streaming block (accumulates content deltas). */
  let assistantEl = null;
  let assistantRaw = '';
  /** Open thinking block for the current turn. */
  let thinkingEl = null;
  /** The plan checklist element, updated in place across the session. */
  let planEl = null;

  function esc(s) {
    return s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
  }

  // Minimal markdown: fenced code, inline code, bold, headings, bullets.
  function md(text) {
    const out = [];
    const lines = text.split('\n');
    let inFence = false;
    let para = [];
    const flush = () => {
      if (para.length) {
        out.push('<p>' + para.join('<br>') + '</p>');
        para = [];
      }
    };
    for (const raw of lines) {
      if (raw.trimStart().startsWith('```')) {
        flush();
        out.push(inFence ? '</code></pre>' : '<pre><code>');
        inFence = !inFence;
        continue;
      }
      if (inFence) {
        out.push(esc(raw) + '\n');
        continue;
      }
      let line = esc(raw);
      line = line.replace(/`([^`]+)`/g, '<code>$1</code>');
      line = line.replace(/\*\*([^*]+)\*\*/g, '<strong>$1</strong>');
      const h = line.match(/^(#{1,3})\s+(.*)$/);
      if (h) {
        flush();
        out.push(`<h${h[1].length + 1}>${h[2]}</h${h[1].length + 1}>`);
      } else if (/^\s*[-*]\s+/.test(line)) {
        flush();
        out.push('<ul><li>' + line.replace(/^\s*[-*]\s+/, '') + '</li></ul>');
      } else if (line.trim() === '') {
        flush();
      } else {
        para.push(line);
      }
    }
    flush();
    if (inFence) out.push('</code></pre>');
    return out.join('').replace(/<\/ul><ul>/g, '');
  }

  function atBottom() {
    return messages.scrollHeight - messages.scrollTop - messages.clientHeight < 40;
  }
  function add(el) {
    const stick = atBottom();
    messages.appendChild(el);
    if (stick) messages.scrollTop = messages.scrollHeight;
    return el;
  }
  function line(cls, text) {
    const el = document.createElement('div');
    el.className = cls;
    el.textContent = text;
    return add(el);
  }

  function closeTurnBlocks() {
    assistantEl = null;
    assistantRaw = '';
    thinkingEl = null;
  }

  function userBubble(text) {
    closeTurnBlocks();
    const el = document.createElement('div');
    el.className = 'msg user';
    el.textContent = text;
    add(el);
  }

  function assistantBlock(text, streaming) {
    if (streaming && assistantEl) {
      assistantRaw += text;
      const stick = atBottom();
      assistantEl.innerHTML = md(assistantRaw);
      if (stick) messages.scrollTop = messages.scrollHeight;
      return;
    }
    const el = document.createElement('div');
    el.className = 'msg assistant';
    el.innerHTML = md(text);
    add(el);
    if (streaming) {
      assistantEl = el;
      assistantRaw = text;
    }
  }

  function askCard(ev) {
    const card = document.createElement('div');
    card.className = 'ask';
    const q = document.createElement('div');
    q.className = 'question';
    q.textContent = ev.question;
    card.appendChild(q);
    if (ev.detail && ev.detail.length) {
      const pre = document.createElement('pre');
      pre.className = 'detail';
      for (const l of ev.detail) {
        const span = document.createElement('span');
        span.textContent = l + '\n';
        if (l.startsWith('+')) span.className = 'diff-add';
        else if (l.startsWith('-')) span.className = 'diff-del';
        pre.appendChild(span);
      }
      card.appendChild(pre);
    }
    const answer = (text) => {
      vscode.postMessage({ type: 'answer', id: ev.id, text });
      card.classList.add('answered');
      const done = document.createElement('div');
      done.className = 'line-info';
      done.textContent = text ? `→ ${text}` : '(dismissed)';
      card.appendChild(done);
    };
    if (ev.choices && ev.choices.length) {
      const row = document.createElement('div');
      row.className = 'choices';
      ev.choices.forEach((c, i) => {
        const b = document.createElement('button');
        b.textContent = c;
        if (i > 0) b.className = 'secondary';
        b.addEventListener('click', () => answer(c));
        row.appendChild(b);
      });
      card.appendChild(row);
    } else {
      const row = document.createElement('div');
      row.className = 'choices free';
      const field = document.createElement('input');
      field.placeholder = 'type your answer…';
      const b = document.createElement('button');
      b.textContent = 'send';
      b.addEventListener('click', () => answer(field.value));
      field.addEventListener('keydown', (e) => {
        if (e.key === 'Enter') answer(field.value);
      });
      row.appendChild(field);
      row.appendChild(b);
      card.appendChild(row);
      field.focus();
    }
    add(card);
  }

  function renderPlan(items) {
    if (!planEl) {
      planEl = document.createElement('div');
      planEl.className = 'plan';
      add(planEl);
    }
    planEl.innerHTML = '';
    for (const it of items) {
      const row = document.createElement('div');
      if (it.done) row.className = 'done';
      row.textContent = `${it.done ? '☑' : '☐'} ${it.text}`;
      planEl.appendChild(row);
    }
  }

  function onRiftEvent(ev) {
    switch (ev.event) {
      case 'ready':
        line('line-info', `session ready · ${ev.model}`);
        break;
      case 'history':
        for (const m of ev.messages) {
          if (m.role === 'user') userBubble(m.text);
          else assistantBlock(m.text, false);
        }
        line('line-info', '── session resumed ──');
        break;
      case 'content':
        if (thinkingEl) thinkingEl = null;
        assistantBlock(ev.text, true);
        break;
      case 'thinking': {
        if (!thinkingEl) {
          thinkingEl = document.createElement('div');
          thinkingEl.className = 'thinking';
          add(thinkingEl);
        }
        const stick = atBottom();
        thinkingEl.textContent += ev.text;
        if (stick) messages.scrollTop = messages.scrollHeight;
        break;
      }
      case 'tool_start': {
        assistantEl = null;
        assistantRaw = '';
        let args = ev.args;
        try {
          const o = JSON.parse(args);
          args = Object.entries(o).map(([k, v]) => `${k}=${String(v).slice(0, 60)}`).join(' ');
        } catch { /* show raw */ }
        line('tool', `→ ${ev.name} ${args}`);
        break;
      }
      case 'tool_result':
        line(ev.ok ? 'tool' : 'tool err', `${ev.ok ? '✓' : '✗'} ${ev.name} ${ev.preview || ''}`);
        break;
      case 'ask':
        askCard(ev);
        break;
      case 'plan':
        renderPlan(ev.items);
        break;
      case 'info':
        line('line-info', ev.text);
        break;
      case 'warning':
        line('line-warn', '! ' + ev.text);
        break;
      case 'task_started':
        line('line-info', `⚙ background #${ev.id} started: ${ev.label}`);
        break;
      case 'task_finished':
        line('line-info', `⚙ background #${ev.id} ${ev.ok ? '✓' : '✗'} ${ev.label}`);
        break;
      case 'done': {
        closeTurnBlocks();
        const s = ev.stats || {};
        if (s.output_tokens) {
          line(
            'stats',
            `${s.output_tokens} tok · ${(s.tokens_per_sec || 0).toFixed(1)} tok/s · ${((s.duration_ms || 0) / 1000).toFixed(1)}s`
          );
        }
        break;
      }
    }
  }

  const modelSelect = document.getElementById('model-select');
  const settingsPanel = document.getElementById('settings');
  const setBin = document.getElementById('set-bin');
  const setHost = document.getElementById('set-host');
  const setEffort = document.getElementById('set-effort');
  const setArgs = document.getElementById('set-args');

  function renderModels(models, current) {
    modelSelect.innerHTML = '';
    const def = document.createElement('option');
    def.value = '';
    def.textContent = 'default model (rift config)';
    modelSelect.appendChild(def);
    const seen = new Set(models);
    // The configured model always appears, even if its server is down.
    if (current && !seen.has(current)) models = [current, ...models];
    for (const name of models) {
      const opt = document.createElement('option');
      opt.value = name;
      opt.textContent = name;
      modelSelect.appendChild(opt);
    }
    modelSelect.value = current || '';
  }

  window.addEventListener('message', (e) => {
    const m = e.data;
    if (m.type === 'rift') onRiftEvent(m.ev);
    else if (m.type === 'userEcho') userBubble(m.text);
    else if (m.type === 'status') {
      statusEl.textContent = m.text;
      btnStop.classList.toggle('hidden', !m.busy);
      btnSend.classList.toggle('hidden', m.busy);
    } else if (m.type === 'insert') {
      input.value += m.text;
      input.focus();
      autosize();
    } else if (m.type === 'reset') {
      messages.innerHTML = '';
      closeTurnBlocks();
      planEl = null;
    } else if (m.type === 'models') {
      renderModels(m.models, m.current);
    } else if (m.type === 'settings') {
      setBin.value = m.executablePath;
      setHost.value = m.host;
      setEffort.value = m.effort || '';
      setArgs.value = m.extraArgs;
      if (m.model !== undefined && modelSelect.options.length) modelSelect.value = m.model;
    }
  });

  function send() {
    const text = input.value.trim();
    if (!text) return;
    vscode.postMessage({ type: 'send', text });
    input.value = '';
    autosize();
  }

  function autosize() {
    input.style.height = 'auto';
    input.style.height = Math.min(input.scrollHeight, window.innerHeight * 0.4) + 'px';
  }

  input.addEventListener('keydown', (e) => {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      send();
    }
  });
  input.addEventListener('input', autosize);
  btnSend.addEventListener('click', send);
  btnStop.addEventListener('click', () => vscode.postMessage({ type: 'cancel' }));
  document.getElementById('btn-new').addEventListener('click', () => vscode.postMessage({ type: 'newSession' }));
  document.getElementById('btn-continue').addEventListener('click', () => vscode.postMessage({ type: 'continueSession' }));

  modelSelect.addEventListener('change', () => {
    vscode.postMessage({ type: 'setModel', model: modelSelect.value });
  });
  document.getElementById('btn-refresh').addEventListener('click', () => {
    vscode.postMessage({ type: 'refreshModels' });
  });
  document.getElementById('btn-settings').addEventListener('click', () => {
    settingsPanel.classList.toggle('hidden');
  });
  document.getElementById('set-close').addEventListener('click', () => {
    settingsPanel.classList.add('hidden');
  });
  document.getElementById('set-save').addEventListener('click', () => {
    vscode.postMessage({
      type: 'saveSettings',
      executablePath: setBin.value.trim(),
      host: setHost.value.trim(),
      effort: setEffort.value,
      extraArgs: setArgs.value.trim(),
    });
    settingsPanel.classList.add('hidden');
  });
  document.getElementById('set-config').addEventListener('click', () => {
    vscode.postMessage({ type: 'openConfig' });
  });

  vscode.postMessage({ type: 'ready' });
})();

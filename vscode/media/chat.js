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
  /** Tool row awaiting its result — tool_result folds into the same row. */
  let pendingTool = null;

  function esc(s) {
    return s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
  }

  // Languages highlightAuto may guess from — keeps unlabeled fences fast.
  const AUTO_LANGS = [
    'javascript', 'typescript', 'python', 'rust', 'go', 'bash', 'json',
    'yaml', 'html', 'xml', 'css', 'c', 'cpp', 'java', 'sql', 'diff',
  ];

  /** One fenced block: highlighted (when hljs knows the language) with a
   *  copy / insert-at-cursor button bar. Buttons are wired by delegation on
   *  #messages — streaming re-renders would drop per-element listeners. */
  function codeBlock(code, lang) {
    const hl = window.hljs;
    let html;
    try {
      if (hl && lang && hl.getLanguage(lang)) {
        html = hl.highlight(code, { language: lang }).value;
      } else if (hl && code.length < 10000) {
        html = hl.highlightAuto(code, AUTO_LANGS).value;
      } else {
        html = esc(code);
      }
    } catch {
      html = esc(code);
    }
    return (
      '<div class="codeblock">' +
      `<div class="cb-bar"><span class="cb-lang">${esc(lang || '')}</span>` +
      '<span class="cb-actions">' +
      '<button class="cb-copy" title="Copy to clipboard">copy</button>' +
      '<button class="cb-insert" title="Insert at cursor (replaces selection)">insert</button>' +
      '</span></div>' +
      `<pre><code class="hljs">${html}</code></pre></div>`
    );
  }

  // Minimal markdown: fenced code, inline code, bold, headings, bullets.
  function md(text) {
    const out = [];
    const lines = text.split('\n');
    let inFence = false;
    let fenceLang = '';
    let fenceBuf = [];
    let para = [];
    const flush = () => {
      if (para.length) {
        out.push('<p>' + para.join('<br>') + '</p>');
        para = [];
      }
    };
    const closeFence = () => {
      out.push(codeBlock(fenceBuf.join('\n'), fenceLang));
      fenceBuf = [];
      fenceLang = '';
    };
    for (const raw of lines) {
      const t = raw.trimStart();
      if (t.startsWith('```')) {
        if (inFence) {
          closeFence();
        } else {
          flush();
          fenceLang = t.slice(3).trim().toLowerCase();
        }
        inFence = !inFence;
        continue;
      }
      if (inFence) {
        fenceBuf.push(raw);
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
    if (inFence) closeFence(); // still streaming — render what's arrived
    return out.join('').replace(/<\/ul><ul>/g, '');
  }

  // Code block buttons, delegated: innerHTML re-renders during streaming
  // would silently drop listeners attached to the buttons themselves.
  messages.addEventListener('click', (e) => {
    const btn = e.target.closest('button');
    if (!btn) return;
    const block = btn.closest('.codeblock');
    if (!block) return;
    // textContent of the highlighted markup is the original code.
    const code = block.querySelector('pre code').textContent;
    if (btn.classList.contains('cb-copy')) {
      navigator.clipboard.writeText(code).then(() => {
        btn.textContent = 'copied ✓';
        setTimeout(() => (btn.textContent = 'copy'), 1200);
      });
    } else if (btn.classList.contains('cb-insert')) {
      vscode.postMessage({ type: 'insertCode', code });
    }
  });

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
    pendingTool = null;
  }

  /** One tool call = one row: an ellipsized head line that folds the result
   *  in when it arrives, and a click-to-expand body with full args/output. */
  function toolRow(name, summary, full) {
    const el = document.createElement('div');
    el.className = 'tool';
    const head = document.createElement('div');
    head.className = 'tool-head';
    head.title = 'click to expand';
    head.textContent = `→ ${name} ${summary}`;
    const body = document.createElement('div');
    body.className = 'tool-body hidden';
    body.textContent = full;
    head.addEventListener('click', () => body.classList.toggle('hidden'));
    el.appendChild(head);
    el.appendChild(body);
    add(el);
    return { el, head, body, name, summary };
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
          thinkingEl.title = 'click to expand';
          thinkingEl.addEventListener('click', function () {
            this.classList.toggle('expanded');
          });
          add(thinkingEl);
        }
        const stick = atBottom();
        thinkingEl.textContent += ev.text;
        // Keep the newest reasoning visible inside the capped block —
        // without this the tail hides below the fold, clipped mid-line.
        thinkingEl.scrollTop = thinkingEl.scrollHeight;
        if (stick) messages.scrollTop = messages.scrollHeight;
        break;
      }
      case 'tool_start': {
        assistantEl = null;
        assistantRaw = '';
        // Thinking that follows a tool call starts a fresh block *below*
        // the tool row instead of appending above it out of order.
        thinkingEl = null;
        let summary = ev.args;
        let full = ev.args;
        try {
          const o = JSON.parse(ev.args);
          summary = Object.entries(o).map(([k, v]) => `${k}=${String(v).slice(0, 60)}`).join(' ');
          full = Object.entries(o).map(([k, v]) => `${k} = ${v}`).join('\n');
        } catch { /* show raw */ }
        pendingTool = toolRow(ev.name, summary, full);
        break;
      }
      case 'tool_result': {
        const preview = (ev.preview || '').replace(/\s+$/, '');
        const first = preview.split('\n')[0].slice(0, 80);
        if (pendingTool && pendingTool.name === ev.name) {
          const t = pendingTool;
          pendingTool = null;
          t.el.classList.toggle('err', !ev.ok);
          t.head.textContent =
            `${ev.ok ? '✓' : '✗'} ${ev.name} ${t.summary}${first ? ' → ' + first : ''}`;
          if (preview) t.body.textContent += '\n── result ──\n' + preview;
        } else {
          // Result with no open start row (e.g. background task): own row.
          const t = toolRow(ev.name, first, preview);
          t.el.classList.toggle('err', !ev.ok);
          t.head.textContent = `${ev.ok ? '✓' : '✗'} ${ev.name} ${first}`;
        }
        break;
      }
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

  // ── @-mention file completion ─────────────────────────────────────────────
  // Typing "@" (at the start or after whitespace) opens a popup of workspace
  // paths from the extension host, filtered live as the token grows.
  const mentionPopup = document.getElementById('mention-popup');
  /** Active token: start = index of the '@' in the input value. */
  let mention = null;
  let mentionItems = [];
  let mentionSel = 0;
  let mentionToken = 0;
  let mentionTimer = null;

  function mentionContext() {
    const pos = input.selectionStart;
    const m = input.value.slice(0, pos).match(/(?:^|\s)@([^\s@]*)$/);
    return m ? { start: pos - m[1].length - 1, query: m[1] } : null;
  }

  function closeMention() {
    mention = null;
    mentionItems = [];
    mentionPopup.innerHTML = '';
    mentionPopup.classList.add('hidden');
  }

  function updateMention() {
    const ctx = mentionContext();
    if (!ctx) return closeMention();
    mention = ctx;
    const token = ++mentionToken;
    clearTimeout(mentionTimer);
    mentionTimer = setTimeout(
      () => vscode.postMessage({ type: 'queryFiles', query: ctx.query, token }),
      60
    );
  }

  function renderMention() {
    mentionPopup.innerHTML = '';
    if (!mention || !mentionItems.length) {
      mentionPopup.classList.add('hidden');
      return;
    }
    mentionItems.forEach((it, i) => {
      const row = document.createElement('div');
      row.className = 'mention-item' + (i === mentionSel ? ' selected' : '');
      const slash = it.path.lastIndexOf('/');
      const name = document.createElement('span');
      name.className = 'mi-name';
      name.textContent = it.path.slice(slash + 1) + (it.dir ? '/' : '');
      row.appendChild(name);
      if (slash > 0) {
        const dir = document.createElement('span');
        dir.className = 'mi-dir';
        dir.textContent = it.path.slice(0, slash);
        row.appendChild(dir);
      }
      // mousedown (not click) so the textarea never loses focus.
      row.addEventListener('mousedown', (e) => {
        e.preventDefault();
        pickMention(i);
      });
      row.addEventListener('mousemove', () => {
        if (mentionSel !== i) {
          mentionSel = i;
          renderMention();
        }
      });
      mentionPopup.appendChild(row);
    });
    mentionPopup.classList.remove('hidden');
    const sel = mentionPopup.children[mentionSel];
    if (sel) sel.scrollIntoView({ block: 'nearest' });
  }

  function pickMention(i) {
    const it = mentionItems[i];
    if (!it || !mention) return;
    const end = input.selectionStart;
    const text = '@' + it.path + (it.dir ? '/' : ' ');
    input.value = input.value.slice(0, mention.start) + text + input.value.slice(end);
    const caret = mention.start + text.length;
    input.setSelectionRange(caret, caret);
    input.focus();
    autosize();
    // Picking a folder keeps the popup open to drill into it; a file closes it.
    if (it.dir) updateMention();
    else closeMention();
  }

  mentionPopup.addEventListener('mousedown', (e) => e.preventDefault());
  input.addEventListener('blur', closeMention);
  input.addEventListener('click', updateMention);

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
      updateMention();
    } else if (m.type === 'reset') {
      messages.innerHTML = '';
      closeTurnBlocks();
      planEl = null;
    } else if (m.type === 'files') {
      // Stale responses (an older keystroke's query) are dropped by token.
      if (m.token === mentionToken && mention) {
        mentionItems = m.results;
        mentionSel = 0;
        renderMention();
      }
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
    closeMention();
    vscode.postMessage({ type: 'send', text });
    input.value = '';
    autosize();
  }

  function autosize() {
    input.style.height = 'auto';
    input.style.height = Math.min(input.scrollHeight, window.innerHeight * 0.4) + 'px';
  }

  input.addEventListener('keydown', (e) => {
    if (mention && mentionItems.length && !mentionPopup.classList.contains('hidden')) {
      if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
        e.preventDefault();
        const n = mentionItems.length;
        mentionSel = (mentionSel + (e.key === 'ArrowDown' ? 1 : n - 1)) % n;
        renderMention();
        return;
      }
      if (e.key === 'Enter' || e.key === 'Tab') {
        e.preventDefault();
        pickMention(mentionSel);
        return;
      }
      if (e.key === 'Escape') {
        e.preventDefault();
        closeMention();
        return;
      }
    }
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      send();
    }
  });
  input.addEventListener('input', () => {
    autosize();
    updateMention();
  });
  btnSend.addEventListener('click', send);
  btnStop.addEventListener('click', () => vscode.postMessage({ type: 'cancel' }));
  document.getElementById('btn-undo').addEventListener('click', () => vscode.postMessage({ type: 'undo' }));
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

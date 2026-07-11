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
  /** Current run of consecutive tool calls, collected into one capped,
   *  auto-scrolling box so a long burst doesn't take over the transcript.
   *  Broken (reset to null) as soon as any non-tool element is added. */
  let toolGroup = null;
  /** Sub-agent lanes by tag ("agent 1", "task #3") — one card per agent. */
  const agentLanes = new Map();
  /** Last status text from the extension; agent count is appended locally. */
  let baseStatus = 'rift';

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
      '<button class="cb-copy" data-tip="Copy this code to the clipboard">copy</button>' +
      '<button class="cb-insert" data-tip="Insert this code at the cursor in the active editor (replaces the selection, if any)">insert</button>' +
      '</span></div>' +
      `<pre><code class="hljs">${html}</code></pre></div>`
    );
  }

  // Inline spans shared by paragraphs, list items, and table cells.
  function inline(s) {
    return esc(s)
      .replace(/`([^`]+)`/g, '<code>$1</code>')
      .replace(/\*\*([^*]+)\*\*/g, '<strong>$1</strong>');
  }

  // A GFM pipe table: a header row, a |---|:--:| delimiter, then body rows.
  const TABLE_DELIM = /^\s*\|?\s*:?-{1,}:?\s*(\|\s*:?-{1,}:?\s*)+\|?\s*$/;
  function tableCells(row) {
    let s = row.trim();
    if (s.startsWith('|')) s = s.slice(1);
    if (s.endsWith('|')) s = s.slice(0, -1);
    return s.split('|').map((c) => c.trim());
  }
  function tableAligns(delim) {
    return tableCells(delim).map((c) => {
      const l = c.startsWith(':');
      const r = c.endsWith(':');
      return l && r ? 'center' : r ? 'right' : l ? 'left' : '';
    });
  }
  function renderTable(header, delim, rows) {
    const aligns = tableAligns(delim);
    const cell = (tag, text, i) => {
      const a = aligns[i] ? ` style="text-align:${aligns[i]}"` : '';
      return `<${tag}${a}>${inline(text)}</${tag}>`;
    };
    const th = tableCells(header).map((c, i) => cell('th', c, i)).join('');
    const body = rows
      .map((r) => `<tr>${tableCells(r).map((c, i) => cell('td', c, i)).join('')}</tr>`)
      .join('');
    return `<table class="md-table"><thead><tr>${th}</tr></thead><tbody>${body}</tbody></table>`;
  }

  // Minimal markdown: fenced code, inline code, bold, headings, bullets, tables.
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
      // Stray/empty fences (a model tic around tool calls) would render as
      // bare code-block bars — thin phantom lines in the transcript. Skip
      // them; a fence still streaming renders once content arrives.
      if (fenceBuf.join('\n').trim()) {
        out.push(codeBlock(fenceBuf.join('\n'), fenceLang));
      }
      fenceBuf = [];
      fenceLang = '';
    };
    for (let i = 0; i < lines.length; i++) {
      const raw = lines[i];
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
      // Table: this row has a pipe and the next line is the |---| delimiter.
      if (raw.includes('|') && i + 1 < lines.length && TABLE_DELIM.test(lines[i + 1])) {
        flush();
        const rows = [];
        let j = i + 2;
        while (j < lines.length && lines[j].includes('|') && lines[j].trim() !== '') {
          rows.push(lines[j]);
          j++;
        }
        out.push(renderTable(raw, lines[i + 1], rows));
        i = j - 1;
        continue;
      }
      let line = inline(raw);
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
    // Anything added straight to the transcript ends the current tool run,
    // so the next tool call opens a fresh box below the intervening content.
    toolGroup = null;
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

  function renderStatus() {
    let active = 0;
    for (const lane of agentLanes.values()) if (!lane.done) active++;
    statusEl.textContent = baseStatus + (active ? ` · ⧉ ${active} running` : '');
  }

  // ── Context gauge ─────────────────────────────────────────────────────────
  // A header pill showing how full the model's context window is (estimated
  // conversation tokens vs the working num_ctx, from rift's context events).
  const ctxEl = document.getElementById('ctx');

  function fmtTok(n) {
    return n >= 10000 ? `${Math.round(n / 1000)}k` : n >= 1000 ? `${(n / 1000).toFixed(1)}k` : String(n);
  }

  function renderCtxGauge(used, limit) {
    if (!limit) {
      ctxEl.className = 'hidden';
      return;
    }
    const pct = Math.min(999, Math.round((used * 100) / limit));
    ctxEl.textContent = `ctx ${pct}%`;
    ctxEl.className = pct >= 85 ? 'ctx-high' : pct >= 60 ? 'ctx-mid' : 'ctx-low';
    ctxEl.setAttribute(
      'data-tip',
      `Context window: ~${fmtTok(used)} of ${fmtTok(limit)} tokens in use` +
        (pct >= 85 ? ' — rift will compact older history soon' : '')
    );
  }

  /** One card per sub-agent/background task: a status head plus its own
   *  scrolling activity feed, so parallel agents don't interleave. */
  function agentLane(tag, icon, model, label) {
    const el = document.createElement('div');
    el.className = 'agent-lane';
    const head = document.createElement('div');
    head.className = 'lane-head';
    head.setAttribute(
      'data-tip',
      'rift delegated this work to a separate agent running in parallel — click to show/hide its activity'
    );
    const status = document.createElement('span');
    status.className = 'lane-status running';
    status.textContent = '◐';
    const title = document.createElement('span');
    title.className = 'lane-title';
    title.textContent = `${icon} ${tag}${model ? ' · ' + model : ''}${label ? ' — ' + label : ''}`;
    head.appendChild(status);
    head.appendChild(title);
    const body = document.createElement('div');
    body.className = 'lane-body';
    head.addEventListener('click', () => body.classList.toggle('hidden'));
    el.appendChild(head);
    el.appendChild(body);
    add(el);
    const lane = { el, body, status, tag, done: false };
    agentLanes.set(tag, lane);
    renderStatus();
    return lane;
  }

  function laneLine(lane, text, warn) {
    const stick = atBottom();
    const l = document.createElement('div');
    if (warn) l.className = 'warn';
    l.textContent = text;
    lane.body.appendChild(l);
    lane.body.scrollTop = lane.body.scrollHeight;
    if (stick) messages.scrollTop = messages.scrollHeight;
  }

  function finishLane(lane, mark, ok) {
    lane.done = true;
    lane.status.textContent = mark;
    lane.status.classList.remove('running');
    lane.status.classList.add(ok ? 'ok' : 'err');
    renderStatus();
  }

  /** The box that collects the current run of tool calls. Created lazily on
   *  the first tool row and reused until add() breaks the run. */
  function ensureToolGroup() {
    if (toolGroup) return toolGroup;
    const el = document.createElement('div');
    el.className = 'tool-group';
    const head = document.createElement('div');
    head.className = 'tool-group-head';
    head.setAttribute('data-tip', 'Tool activity for this step — scrolls within its box; click to collapse');
    const body = document.createElement('div');
    body.className = 'tool-group-body';
    head.addEventListener('click', () => el.classList.toggle('collapsed'));
    el.appendChild(head);
    el.appendChild(body);
    add(el); // resets toolGroup to null …
    toolGroup = { el, head, body, count: 0 }; // … so claim it afterwards
    return toolGroup;
  }

  /** One tool call = one row: an ellipsized head line that folds the result
   *  in when it arrives, and a click-to-expand body with full args/output.
   *  Rows live inside the current tool-group box, which caps its height and
   *  auto-scrolls so a long burst of calls can't blow up the transcript. */
  function toolRow(name, summary, full) {
    const el = document.createElement('div');
    el.className = 'tool';
    const head = document.createElement('div');
    head.className = 'tool-head';
    head.setAttribute('data-tip', 'Tool call — click to show the full command and output');
    head.textContent = `→ ${name} ${summary}`;
    const body = document.createElement('div');
    body.className = 'tool-body hidden';
    body.textContent = full;
    head.addEventListener('click', (e) => {
      e.stopPropagation(); // don't also toggle the group's collapse
      body.classList.toggle('hidden');
    });
    el.appendChild(head);
    el.appendChild(body);
    const g = ensureToolGroup();
    const stick = atBottom();
    g.body.appendChild(el);
    g.count += 1;
    g.head.textContent = `⚙ tool activity · ${g.count} call${g.count === 1 ? '' : 's'}`;
    g.body.scrollTop = g.body.scrollHeight; // keep the newest row in view
    if (stick) messages.scrollTop = messages.scrollHeight;
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
    if (streaming) {
      assistantRaw += text;
      // Whitespace-only deltas between tool calls must not open a block:
      // each empty block eats a flex-gap slot (a phantom blank row) and
      // needlessly breaks the current tool-activity box.
      if (!assistantEl) {
        if (!assistantRaw.trim()) return;
        assistantEl = add(Object.assign(document.createElement('div'), { className: 'msg assistant' }));
      }
      const stick = atBottom();
      assistantEl.innerHTML = md(assistantRaw);
      if (stick) messages.scrollTop = messages.scrollHeight;
      return;
    }
    if (!text.trim()) return;
    const el = document.createElement('div');
    el.className = 'msg assistant';
    el.innerHTML = md(text);
    add(el);
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

  /** Red/green summary of a change rift applied to a file. Head shows the
   *  path and ±counts; click toggles the diff body. */
  function diffCard(ev) {
    const card = document.createElement('div');
    card.className = 'diff-card';
    const head = document.createElement('div');
    head.className = 'diff-head';
    head.textContent = `✎ ${ev.path} · +${ev.added} −${ev.removed}`;
    head.setAttribute('data-tip', 'Change rift applied to this file — click to show/hide the diff');
    const body = document.createElement('pre');
    body.className = 'diff-body';
    for (const l of ev.diff || []) {
      const span = document.createElement('span');
      span.textContent = l + '\n';
      if (l.startsWith('+')) span.className = 'diff-add';
      else if (l.startsWith('-')) span.className = 'diff-del';
      else span.className = 'diff-meta';
      body.appendChild(span);
    }
    head.addEventListener('click', () => body.classList.toggle('hidden'));
    card.appendChild(head);
    card.appendChild(body);
    add(card);
  }

  /** Pending edit-review cards by id — resolved by editReviewDone. */
  const reviewCards = new Map();

  /** One proposed write/edit under review: the diff opens in the editor;
   *  this card summarizes it and offers the same apply/reject actions. */
  function reviewCard(m) {
    const card = document.createElement('div');
    card.className = 'ask';
    const q = document.createElement('div');
    q.className = 'question';
    q.textContent = `✎ review ${m.tool}: ${m.path}`;
    card.appendChild(q);
    const meta = document.createElement('div');
    meta.className = 'line-info';
    meta.textContent = `${m.hunks} hunk${m.hunks === 1 ? '' : 's'} · +${m.added} −${m.removed} — review the diff in the editor`;
    card.appendChild(meta);
    const row = document.createElement('div');
    row.className = 'choices';
    const mk = (label, action, secondary, tip) => {
      const b = document.createElement('button');
      b.textContent = label;
      if (secondary) b.className = 'secondary';
      if (tip) b.setAttribute('data-tip', tip);
      b.addEventListener('click', () => vscode.postMessage({ type: 'reviewAction', id: m.id, action }));
      row.appendChild(b);
    };
    mk('Apply', 'apply', false, 'Write the accepted hunks to the file (all of them, unless you rejected some in the diff view)');
    mk('Open diff', 'open', true, 'Open the proposal as a native diff — accept or reject individual hunks there');
    mk('Reject', 'reject', true, 'Apply nothing — the agent is told the edit was denied');
    card.appendChild(row);
    add(card);
    reviewCards.set(m.id, { card, row });
  }

  function reviewDone(m) {
    const rc = reviewCards.get(m.id);
    reviewCards.delete(m.id);
    if (!rc) return;
    rc.row.remove();
    rc.card.classList.add('answered');
    const done = document.createElement('div');
    done.className = 'line-info';
    done.textContent = `→ ${m.verdict}`;
    rc.card.appendChild(done);
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
        // Don't open a reasoning block for whitespace-only deltas — an
        // empty .thinking div is another phantom row in the transcript.
        if (!thinkingEl && !ev.text.trim()) break;
        if (!thinkingEl) {
          thinkingEl = document.createElement('div');
          thinkingEl.className = 'thinking';
          thinkingEl.setAttribute('data-tip', "The model's reasoning — click to expand or collapse");
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
      case 'edit_diff':
        diffCard(ev);
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
      case 'subagent_started':
        agentLane(ev.tag, '⧉', ev.model, ev.label);
        break;
      case 'subagent': {
        // Activity for an unknown tag (e.g. session resumed mid-task)
        // still gets a lane so nothing is silently dropped.
        const lane = agentLanes.get(ev.tag) || agentLane(ev.tag, '⧉', '', '');
        laneLine(lane, ev.text, ev.warn);
        break;
      }
      case 'subagent_finished': {
        const lane = agentLanes.get(ev.tag);
        if (lane) {
          laneLine(lane, `finished — ${ev.steps} step(s)`);
          finishLane(lane, '✓', true);
        }
        break;
      }
      case 'task_started':
        agentLane(`task #${ev.id}`, '⚙', '', ev.label);
        break;
      case 'task_finished': {
        const lane = agentLanes.get(`task #${ev.id}`);
        if (lane) {
          if (ev.preview) laneLine(lane, ev.preview, !ev.ok);
          finishLane(lane, ev.ok ? '✓' : '✗', ev.ok);
        } else {
          line('line-info', `⚙ background #${ev.id} ${ev.ok ? '✓' : '✗'} ${ev.label}`);
        }
        break;
      }
      case 'done': {
        closeTurnBlocks();
        // A cancelled turn drops its foreground agents without a finished
        // event — close their lanes so nothing spins forever. Background
        // task lanes live across turns and are left alone.
        for (const lane of agentLanes.values()) {
          if (!lane.done && !lane.tag.startsWith('task #')) finishLane(lane, '◼', false);
        }
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
  // Skills + plugin commands from the server's ready event (via status),
  // completable as /skill:<name> — same popup as @file mentions.
  let skills = [];

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

  /// "/", "/sk", "/skill:stand" at the start of the input → skill completion.
  function skillContext() {
    const pos = input.selectionStart;
    const m = input.value.slice(0, pos).match(/^\/(?:s(?:k(?:i(?:l(?:l:?)?)?)?)?)?([^\s/]*)$/);
    if (!m) return null;
    return { start: 0, query: m[1] || '', kind: 'skill' };
  }

  function updateMention() {
    const sctx = skillContext();
    if (sctx && skills.length) {
      mention = sctx;
      mentionSel = 0;
      mentionItems = skills
        .filter((s) => s.name.toLowerCase().startsWith(sctx.query.toLowerCase()))
        .slice(0, 12)
        .map((s) => ({ path: 'skill:' + s.name, skill: true, desc: s.description }));
      renderMention();
      return;
    }
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
      if (it.skill && it.desc) {
        // Skill rows: the description rides in the dir slot.
        const desc = document.createElement('span');
        desc.className = 'mi-dir';
        desc.textContent = it.desc;
        row.appendChild(desc);
      } else if (slash > 0) {
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
    const text = it.skill ? '/' + it.path + ' ' : '@' + it.path + (it.dir ? '/' : ' ');
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
  const setNumCtx = document.getElementById('set-numctx');
  const setTemp = document.getElementById('set-temp');
  const setIters = document.getElementById('set-iters');

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
    else if (m.type === 'editReview') reviewCard(m);
    else if (m.type === 'editReviewDone') reviewDone(m);
    else if (m.type === 'userEcho') userBubble(m.text);
    else if (m.type === 'status') {
      baseStatus = m.text;
      renderStatus();
      renderCtxGauge(m.ctxUsed, m.ctxLimit);
      btnStop.classList.toggle('hidden', !m.busy);
      btnSend.classList.toggle('hidden', m.busy);
      if (m.skills) skills = m.skills;
    } else if (m.type === 'insert') {
      input.value += m.text;
      input.focus();
      autosize();
      updateMention();
    } else if (m.type === 'reset') {
      messages.innerHTML = '';
      closeTurnBlocks();
      planEl = null;
      agentLanes.clear();
      reviewCards.clear();
      renderStatus();
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
      setNumCtx.value = m.numCtx;
      setTemp.value = m.temperature;
      setIters.value = m.maxIterations;
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
  document.getElementById('btn-history').addEventListener('click', () => vscode.postMessage({ type: 'history' }));
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
      numCtx: setNumCtx.value.trim(),
      temperature: setTemp.value.trim(),
      maxIterations: setIters.value.trim(),
    });
    settingsPanel.classList.add('hidden');
  });
  document.getElementById('set-config').addEventListener('click', () => {
    vscode.postMessage({ type: 'openConfig' });
  });

  // ── Tooltips ───────────────────────────────────────────────────────────────
  // One floating box for every [data-tip] element, shown instantly on hover
  // (native title tooltips are delayed and easy to miss on small buttons).
  const tip = document.createElement('div');
  tip.id = 'tooltip';
  tip.className = 'hidden';
  document.body.appendChild(tip);
  let tipTarget = null;

  function hideTip() {
    tipTarget = null;
    tip.classList.add('hidden');
  }

  document.addEventListener('mouseover', (e) => {
    const t = e.target.closest ? e.target.closest('[data-tip]') : null;
    if (t === tipTarget) return;
    if (!t) return hideTip();
    tipTarget = t;
    tip.textContent = t.getAttribute('data-tip');
    tip.classList.remove('hidden');
    // Position after content is set so measurements are real: centered
    // above the element, clamped to the viewport, flipped below if needed.
    const r = t.getBoundingClientRect();
    const x = Math.max(4, Math.min(r.left + r.width / 2 - tip.offsetWidth / 2, window.innerWidth - tip.offsetWidth - 4));
    let y = r.top - tip.offsetHeight - 6;
    if (y < 4) y = r.bottom + 6;
    tip.style.left = x + 'px';
    tip.style.top = y + 'px';
  });
  // A click usually changes state (send, expand, undo) — stale tips linger
  // over the new state, so drop them; scrolling moves the anchor away.
  document.addEventListener('mousedown', hideTip);
  messages.addEventListener('scroll', hideTip);

  vscode.postMessage({ type: 'ready' });
})();

// Rift desktop frontend. The transcript renderer is ported from the VS Code
// webview (vscode/media/chat.js) — same event handling, same markdown rules —
// wrapped in a Tab class so every tab owns an independent `rift --serve`
// conversation. The shell (tabs, sidebar, modals) is desktop-only. No
// frameworks: the serve protocol is small enough that direct DOM updates
// stay clear.
(function () {
  'use strict';
  const inv = window.__TAURI__.core.invoke;
  const listen = window.__TAURI__.event.listen;

  // ── Markdown (ported verbatim) ──────────────────────────────────────────
  function esc(s) {
    return s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
  }

  const AUTO_LANGS = [
    'javascript', 'typescript', 'python', 'rust', 'go', 'bash', 'json',
    'yaml', 'html', 'xml', 'css', 'c', 'cpp', 'java', 'sql', 'diff',
  ];

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
      '</span></div>' +
      `<pre><code class="hljs">${html}</code></pre></div>`
    );
  }

  function inline(s) {
    return esc(s)
      .replace(/`([^`]+)`/g, '<code>$1</code>')
      .replace(/\*\*([^*]+)\*\*/g, '<strong>$1</strong>');
  }

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

  const HR = '<hr class="md-rule">';
  const RULE_LINE = /^\s*[-─—━═_=*]{3,}\s*$/;

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
      if (RULE_LINE.test(raw)) {
        flush();
        if (out[out.length - 1] !== HR) out.push(HR);
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
    if (inFence) closeFence();
    while (out.length && out[0] === HR) out.shift();
    while (out.length && out[out.length - 1] === HR) out.pop();
    return out.join('').replace(/<\/ul><ul>/g, '');
  }

  // ── Settings (persisted whole by the backend) ───────────────────────────
  let settings = {
    riftBin: '',
    host: '',
    model: '',
    effort: '',
    numCtx: '',
    temperature: '',
    maxIterations: '',
    theme: 'dark',
    recentProjects: [],
  };

  async function loadSettings() {
    try {
      const s = await inv('load_settings');
      if (s && typeof s === 'object') settings = Object.assign(settings, s);
    } catch (e) {
      console.error('load_settings', e);
    }
    applyTheme();
  }
  function saveSettings() {
    inv('save_settings', { settings }).catch((e) => console.error('save_settings', e));
  }
  function applyTheme() {
    document.body.className = settings.theme === 'light' ? 'theme-light' : 'theme-dark';
  }

  function riftArgs() {
    const args = [];
    if (settings.host) args.push('--host', settings.host);
    if (settings.model) args.push('--model', settings.model);
    if (settings.effort) args.push('--effort', settings.effort);
    if (settings.numCtx) args.push('--num-ctx', String(settings.numCtx));
    if (settings.temperature !== '' && settings.temperature != null) {
      args.push('--temp', String(settings.temperature));
    }
    if (settings.maxIterations) args.push('--max-iterations', String(settings.maxIterations));
    return args;
  }

  function touchRecent(dir) {
    settings.recentProjects = [
      { path: dir, at: Date.now() },
      ...(settings.recentProjects || []).filter((r) => r.path !== dir),
    ].slice(0, 12);
    saveSettings();
    renderRecent();
  }

  // ── Tabs ────────────────────────────────────────────────────────────────
  const tabs = new Map();
  let activeTabId = null;
  let nextTabNum = 1;

  const tabsEl = document.getElementById('tabs');
  const panelsEl = document.getElementById('panels');
  const welcomeEl = document.getElementById('welcome');
  const sessionListEl = document.getElementById('session-list');

  function basename(p) {
    const s = p.replace(/[\\/]+$/, '');
    const i = Math.max(s.lastIndexOf('/'), s.lastIndexOf('\\'));
    return i >= 0 ? s.slice(i + 1) : s;
  }

  class Tab {
    constructor(dir) {
      this.id = 't' + nextTabNum++;
      this.dir = dir;
      this.running = false;
      this.busy = false;
      this.model = '';
      this.commands = [];
      this.skills = [];
      this.sessionPath = null;
      this.ctxUsed = 0;
      this.ctxLimit = 0;
      /** Pending edit reviews by id: {ev, segments, accepted[], card, row}. */
      this.reviews = new Map();

      // Streaming state (ported from chat.js).
      this.assistantEl = null;
      this.assistantRaw = '';
      this.thinkingEl = null;
      this.planEl = null;
      this.pendingTool = null;
      this.toolGroup = null;
      this.agentLanes = new Map();

      // Mention/skill completion state.
      this.mention = null;
      this.mentionItems = [];
      this.mentionSel = 0;
      this.mentionToken = 0;
      this.mentionTimer = null;

      this.buildDom();
      tabs.set(this.id, this);
      this.activate();
      updateWelcome();
    }

    buildDom() {
      // Tab strip button.
      this.tabEl = document.createElement('div');
      this.tabEl.className = 'tab';
      this.titleEl = document.createElement('span');
      this.titleEl.className = 'tab-title';
      this.titleEl.textContent = basename(this.dir);
      this.busyEl = document.createElement('span');
      this.busyEl.className = 'tab-busy hidden';
      this.busyEl.textContent = '●';
      const close = document.createElement('button');
      close.className = 'tab-close';
      close.textContent = '✕';
      close.addEventListener('click', (e) => {
        e.stopPropagation();
        this.close();
      });
      this.tabEl.appendChild(this.busyEl);
      this.tabEl.appendChild(this.titleEl);
      this.tabEl.appendChild(close);
      this.tabEl.addEventListener('click', () => this.activate());
      this.tabEl.addEventListener('auxclick', (e) => {
        if (e.button === 1) this.close();
      });
      tabsEl.appendChild(this.tabEl);

      // Panel from the template.
      const tpl = document.getElementById('tab-template');
      this.panel = tpl.content.firstElementChild.cloneNode(true);
      panelsEl.appendChild(this.panel);
      const $ = (sel) => this.panel.querySelector(sel);
      this.statusEl = $('.status');
      this.ctxEl = $('.ctx');
      this.messages = $('.messages');
      this.input = $('.input');
      this.btnSend = $('.btn-send');
      this.btnStop = $('.btn-stop');
      this.mentionPopup = $('.mention-popup');
      this.modelSelect = $('.model-select');

      $('.btn-undo').addEventListener('click', () => {
        if (this.running) this.send({ cmd: 'undo' });
        else this.line('line-warn', '! nothing to undo — rift is not running');
      });
      $('.btn-new-session').addEventListener('click', () => this.restart([]));
      $('.btn-continue').addEventListener('click', () =>
        this.restart(this.sessionPath ? ['--resume', this.sessionPath] : ['--continue'])
      );
      $('.btn-models-refresh').addEventListener('click', () => {
        if (this.running && this.commands.includes('list_models')) {
          this.send({ cmd: 'list_models' });
        }
      });
      this.modelSelect.addEventListener('change', () => {
        const m = this.modelSelect.value;
        if (!m) return;
        if (this.running && this.commands.includes('set_model')) {
          this.send({ cmd: 'set_model', model: m });
        }
      });
      this.btnSend.addEventListener('click', () => this.submit());
      this.btnStop.addEventListener('click', () => this.send({ cmd: 'cancel' }));

      this.input.addEventListener('keydown', (e) => this.onInputKey(e));
      this.input.addEventListener('input', () => {
        this.autosize();
        this.updateMention();
      });
      this.input.addEventListener('click', () => this.updateMention());
      this.input.addEventListener('blur', () => this.closeMention());
      this.mentionPopup.addEventListener('mousedown', (e) => e.preventDefault());

      // Code block copy buttons, delegated (streaming re-renders drop
      // per-element listeners).
      this.messages.addEventListener('click', (e) => {
        const btn = e.target.closest('button');
        if (!btn || !btn.classList.contains('cb-copy')) return;
        const block = btn.closest('.codeblock');
        if (!block) return;
        const code = block.querySelector('pre code').textContent;
        navigator.clipboard.writeText(code).then(() => {
          btn.textContent = 'copied ✓';
          setTimeout(() => (btn.textContent = 'copy'), 1200);
        });
      });
      this.messages.addEventListener('scroll', hideTip);
    }

    activate() {
      activeTabId = this.id;
      for (const t of tabs.values()) {
        t.tabEl.classList.toggle('active', t === this);
        t.panel.classList.toggle('active', t === this);
      }
      updateWelcome();
      renderSessions(this.lastSessions || []);
      this.input.focus();
    }

    close() {
      inv('stop_rift', { tab: this.id }).catch(() => {});
      this.tabEl.remove();
      this.panel.remove();
      tabs.delete(this.id);
      if (activeTabId === this.id) {
        const rest = [...tabs.values()];
        activeTabId = null;
        if (rest.length) rest[rest.length - 1].activate();
        else renderSessions([]);
      }
      updateWelcome();
    }

    async start(extraArgs = []) {
      this.running = true;
      this.statusEl.textContent = 'starting…';
      try {
        await inv('start_rift', {
          tab: this.id,
          dir: this.dir,
          bin: settings.riftBin || '',
          args: [...riftArgs(), ...extraArgs],
        });
        this.send({ cmd: 'hello', edit_review: true });
      } catch (e) {
        this.running = false;
        this.line('line-warn', '! ' + e);
        this.statusEl.textContent = 'not running';
      }
      this.renderStatus();
    }

    restart(extraArgs) {
      for (const id of [...this.reviews.keys()]) this.reviewDone(id, 'cancelled — session restarted');
      this.messages.innerHTML = '';
      this.assistantEl = null;
      this.assistantRaw = '';
      this.thinkingEl = null;
      this.planEl = null;
      this.pendingTool = null;
      this.toolGroup = null;
      this.agentLanes.clear();
      this.busy = false;
      this.ctxUsed = 0;
      this.ctxLimit = 0;
      if (!extraArgs.length) this.sessionPath = null;
      this.start(extraArgs);
    }

    /** Sending when the process died (or was never started) revives it on
     *  the same session — the desktop equivalent of the TUI just being open. */
    ensureRunning() {
      if (this.running) return;
      this.start(this.sessionPath ? ['--resume', this.sessionPath] : []);
    }

    send(obj) {
      inv('send_rift', { tab: this.id, line: JSON.stringify(obj) }).catch((e) =>
        this.line('line-warn', '! ' + e)
      );
    }

    submit() {
      const text = this.input.value.trim();
      if (!text) return;
      this.closeMention();
      this.ensureRunning();
      this.userBubble(text);
      this.busy = true;
      this.send({ cmd: 'prompt', text });
      this.input.value = '';
      this.autosize();
      this.renderStatus();
    }

    // ── Transcript rendering (ported) ─────────────────────────────────────
    atBottom() {
      return this.messages.scrollHeight - this.messages.scrollTop - this.messages.clientHeight < 40;
    }
    add(el) {
      this.toolGroup = null;
      const stick = this.atBottom();
      this.messages.appendChild(el);
      if (stick) this.messages.scrollTop = this.messages.scrollHeight;
      return el;
    }
    line(cls, text) {
      const el = document.createElement('div');
      el.className = cls;
      el.textContent = text;
      return this.add(el);
    }
    closeTurnBlocks() {
      this.assistantEl = null;
      this.assistantRaw = '';
      this.thinkingEl = null;
      this.pendingTool = null;
    }
    userBubble(text) {
      this.closeTurnBlocks();
      const el = document.createElement('div');
      el.className = 'msg user';
      el.textContent = text;
      this.add(el);
    }
    assistantBlock(text, streaming) {
      if (streaming) {
        this.assistantRaw += text;
        const html = md(this.assistantRaw);
        if (!this.assistantEl) {
          if (!html) return;
          this.assistantEl = this.add(
            Object.assign(document.createElement('div'), { className: 'msg assistant' })
          );
        }
        const stick = this.atBottom();
        this.assistantEl.innerHTML = html;
        if (stick) this.messages.scrollTop = this.messages.scrollHeight;
        return;
      }
      const html = md(text);
      if (!html) return;
      const el = document.createElement('div');
      el.className = 'msg assistant';
      el.innerHTML = html;
      this.add(el);
    }

    ensureToolGroup() {
      if (this.toolGroup) return this.toolGroup;
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
      this.add(el); // resets toolGroup …
      this.toolGroup = { el, head, body, count: 0 }; // … so claim it afterwards
      return this.toolGroup;
    }
    toolRow(name, summary, full) {
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
        e.stopPropagation();
        body.classList.toggle('hidden');
      });
      el.appendChild(head);
      el.appendChild(body);
      const g = this.ensureToolGroup();
      const stick = this.atBottom();
      g.body.appendChild(el);
      g.count += 1;
      g.head.textContent = `⚙ tool activity · ${g.count} call${g.count === 1 ? '' : 's'}`;
      g.body.scrollTop = g.body.scrollHeight;
      if (stick) this.messages.scrollTop = this.messages.scrollHeight;
      return { el, head, body, name, summary };
    }

    agentLane(tag, icon, model, label) {
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
      this.add(el);
      const lane = { el, body, status, tag, done: false };
      this.agentLanes.set(tag, lane);
      this.renderStatus();
      return lane;
    }
    laneLine(lane, text, warn) {
      const stick = this.atBottom();
      const l = document.createElement('div');
      if (warn) l.className = 'warn';
      l.textContent = text;
      lane.body.appendChild(l);
      lane.body.scrollTop = lane.body.scrollHeight;
      if (stick) this.messages.scrollTop = this.messages.scrollHeight;
    }
    finishLane(lane, mark, ok) {
      lane.done = true;
      lane.status.textContent = mark;
      lane.status.classList.remove('running');
      lane.status.classList.add(ok ? 'ok' : 'err');
      this.renderStatus();
    }

    askCard(ev) {
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
        this.send({ cmd: 'answer', id: ev.id, text });
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
      this.add(card);
    }

    diffCard(ev) {
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
      this.add(card);
    }

    renderPlan(items) {
      if (!this.planEl) {
        this.planEl = document.createElement('div');
        this.planEl.className = 'plan';
        this.add(this.planEl);
      }
      this.planEl.innerHTML = '';
      for (const it of items) {
        const row = document.createElement('div');
        if (it.done) row.className = 'done';
        row.textContent = `${it.done ? '☑' : '☐'} ${it.text}`;
        this.planEl.appendChild(row);
      }
    }

    // ── Edit review (in-app: card + modal, per-hunk) ──────────────────────
    reviewCard(ev) {
      const segments = Array.isArray(ev.segments)
        ? ev.segments.map((s) =>
            s.same
              ? { same: true, lines: s.lines || [] }
              : { same: false, oldLines: s.old || [], newLines: s.new || [] }
          )
        : [{ same: false, oldLines: (ev.old || '').split('\n'), newLines: (ev.new || '').split('\n') }];
      const accepted = segments.filter((s) => !s.same).map(() => true);
      const added = segments.reduce((t, s) => t + (s.same ? 0 : s.newLines.length), 0);
      const removed = segments.reduce((t, s) => t + (s.same ? 0 : s.oldLines.length), 0);

      const card = document.createElement('div');
      card.className = 'ask';
      const q = document.createElement('div');
      q.className = 'question';
      q.textContent = `✎ review ${ev.tool}: ${ev.path}`;
      card.appendChild(q);
      const meta = document.createElement('div');
      meta.className = 'line-info';
      meta.textContent = `${accepted.length} hunk${accepted.length === 1 ? '' : 's'} · +${added} −${removed}`;
      card.appendChild(meta);
      const row = document.createElement('div');
      row.className = 'choices';
      const mk = (label, fn, secondary, tip) => {
        const b = document.createElement('button');
        b.textContent = label;
        if (secondary) b.className = 'secondary';
        if (tip) b.setAttribute('data-tip', tip);
        b.addEventListener('click', fn);
        row.appendChild(b);
      };
      mk('Review…', () => openReviewModal(this, ev.id), false,
        'Open the diff — accept or reject individual hunks');
      mk('Apply all', () => this.reviewApply(ev.id), false,
        'Write the whole proposal to the file');
      mk('Reject', () => this.reviewReject(ev.id), true,
        'Apply nothing — the agent is told the edit was denied');
      card.appendChild(row);
      this.add(card);
      this.reviews.set(ev.id, { ev, segments, accepted, card, row });
    }

    reviewContent(r) {
      const lines = [];
      let h = 0;
      for (const seg of r.segments) {
        if (seg.same) lines.push(...seg.lines);
        else lines.push(...(r.accepted[h++] ? seg.newLines : seg.oldLines));
      }
      return lines.join('\n');
    }

    reviewApply(id) {
      const r = this.reviews.get(id);
      if (!r) return;
      const kept = r.accepted.filter(Boolean).length;
      if (!kept) return this.reviewReject(id);
      this.send({ cmd: 'edit_decision', id, apply: true, content: this.reviewContent(r) });
      this.reviewDone(id, `applied ${kept}/${r.accepted.length} hunk${r.accepted.length === 1 ? '' : 's'}`);
    }
    reviewReject(id) {
      const r = this.reviews.get(id);
      if (!r) return;
      this.send({ cmd: 'edit_decision', id, apply: false });
      this.reviewDone(id, 'rejected');
    }
    reviewDone(id, verdict) {
      const r = this.reviews.get(id);
      this.reviews.delete(id);
      closeReviewModal(this, id);
      if (!r) return;
      r.row.remove();
      r.card.classList.add('answered');
      const done = document.createElement('div');
      done.className = 'line-info';
      done.textContent = `→ ${verdict}`;
      r.card.appendChild(done);
    }

    // ── Status / gauges ───────────────────────────────────────────────────
    renderStatus() {
      let active = 0;
      for (const lane of this.agentLanes.values()) if (!lane.done) active++;
      const base = this.running
        ? `${this.model || 'starting…'}${this.busy ? ' · working' : ''}`
        : 'not running — send a message to start';
      this.statusEl.textContent = base + (active ? ` · ⧉ ${active} running` : '');
      this.btnStop.classList.toggle('hidden', !this.busy);
      this.btnSend.classList.toggle('hidden', this.busy);
      this.busyEl.classList.toggle('hidden', !this.busy);
      this.renderCtx();
    }
    renderCtx() {
      const { ctxUsed: used, ctxLimit: limit } = this;
      if (!limit || !this.running) {
        this.ctxEl.className = 'ctx hidden';
        return;
      }
      const fmt = (n) =>
        n >= 10000 ? `${Math.round(n / 1000)}k` : n >= 1000 ? `${(n / 1000).toFixed(1)}k` : String(n);
      const pct = Math.min(999, Math.round((used * 100) / limit));
      this.ctxEl.textContent = `ctx ${pct}%`;
      this.ctxEl.className = 'ctx ' + (pct >= 85 ? 'ctx-high' : pct >= 60 ? 'ctx-mid' : 'ctx-low');
      this.ctxEl.setAttribute(
        'data-tip',
        `Context window: ~${fmt(used)} of ${fmt(limit)} tokens in use` +
          (pct >= 85 ? ' — rift will compact older history soon' : '')
      );
    }
    renderModels(models, current) {
      this.modelSelect.innerHTML = '';
      const seen = new Set(models);
      if (current && !seen.has(current)) models = [current, ...models];
      for (const name of models) {
        const opt = document.createElement('option');
        opt.value = name;
        opt.textContent = name;
        this.modelSelect.appendChild(opt);
      }
      this.modelSelect.value = current || '';
    }

    // ── Serve events ──────────────────────────────────────────────────────
    onEvent(ev) {
      switch (ev.event) {
        case 'ready':
          this.model = ev.model;
          if (ev.num_ctx) this.ctxLimit = ev.num_ctx;
          if (ev.session) this.sessionPath = ev.session;
          this.skills = ev.skills || [];
          this.commands = ev.commands || [];
          if (this.commands.includes('list_models')) this.send({ cmd: 'list_models' });
          this.send({ cmd: 'list_sessions' });
          this.line('line-info', `session ready · ${ev.model}`);
          if (ev.protocol_version && ev.protocol_version > 1) {
            this.line('line-warn', `! rift speaks serve protocol v${ev.protocol_version}; this app expects v1 — update the app`);
          }
          break;
        case 'history':
          for (const m of ev.messages) {
            if (m.role === 'user') this.userBubble(m.text);
            else this.assistantBlock(m.text, false);
          }
          this.line('line-info', '── session resumed ──');
          break;
        case 'content':
          if (this.thinkingEl) this.thinkingEl = null;
          this.assistantBlock(ev.text, true);
          break;
        case 'thinking': {
          if (!this.thinkingEl && !ev.text.trim()) break;
          if (!this.thinkingEl) {
            this.thinkingEl = document.createElement('div');
            this.thinkingEl.className = 'thinking';
            this.thinkingEl.setAttribute('data-tip', "The model's reasoning — click to expand or collapse");
            this.thinkingEl.addEventListener('click', function () {
              this.classList.toggle('expanded');
            });
            this.add(this.thinkingEl);
          }
          const stick = this.atBottom();
          this.thinkingEl.textContent += ev.text;
          this.thinkingEl.scrollTop = this.thinkingEl.scrollHeight;
          if (stick) this.messages.scrollTop = this.messages.scrollHeight;
          break;
        }
        case 'tool_start': {
          this.assistantEl = null;
          this.assistantRaw = '';
          this.thinkingEl = null;
          let summary = ev.args;
          let full = ev.args;
          try {
            const o = typeof ev.args === 'string' ? JSON.parse(ev.args) : ev.args;
            summary = Object.entries(o).map(([k, v]) => `${k}=${String(v).slice(0, 60)}`).join(' ');
            full = Object.entries(o).map(([k, v]) => `${k} = ${v}`).join('\n');
          } catch { /* show raw */ }
          this.pendingTool = this.toolRow(ev.name, summary, full);
          break;
        }
        case 'tool_result': {
          const preview = (ev.preview || '').replace(/\s+$/, '');
          const first = preview.split('\n')[0].slice(0, 80);
          if (this.pendingTool && this.pendingTool.name === ev.name) {
            const t = this.pendingTool;
            this.pendingTool = null;
            t.el.classList.toggle('err', !ev.ok);
            t.head.textContent =
              `${ev.ok ? '✓' : '✗'} ${ev.name} ${t.summary}${first ? ' → ' + first : ''}`;
            if (preview) t.body.textContent += '\n── result ──\n' + preview;
          } else {
            const t = this.toolRow(ev.name, first, preview);
            t.el.classList.toggle('err', !ev.ok);
            t.head.textContent = `${ev.ok ? '✓' : '✗'} ${ev.name} ${first}`;
          }
          break;
        }
        case 'edit_diff':
          this.diffCard(ev);
          break;
        case 'edit_review':
          this.reviewCard(ev);
          openReviewModal(this, ev.id);
          break;
        case 'edit_review_closed':
          this.reviewDone(ev.id, 'cancelled — the turn ended before a decision');
          break;
        case 'ask':
          this.askCard(ev);
          break;
        case 'plan':
          this.renderPlan(ev.items);
          break;
        case 'context':
          this.ctxUsed = ev.used;
          this.ctxLimit = ev.limit;
          this.renderCtx();
          break;
        case 'sessions':
          this.lastSessions = ev.items || [];
          if (activeTabId === this.id) renderSessions(this.lastSessions);
          break;
        case 'models':
          this.renderModels(ev.models || [], ev.current || this.model);
          break;
        case 'model_changed':
          this.model = ev.model;
          this.line('line-info', `switched to ${ev.model}${ev.note ? ' ' + ev.note : ''}`);
          break;
        case 'info':
          this.line('line-info', ev.text);
          break;
        case 'warning':
          this.line('line-warn', '! ' + ev.text);
          break;
        case 'subagent_started':
          this.agentLane(ev.tag, '⧉', ev.model, ev.label);
          break;
        case 'subagent': {
          const lane = this.agentLanes.get(ev.tag) || this.agentLane(ev.tag, '⧉', '', '');
          this.laneLine(lane, ev.text, ev.warn);
          break;
        }
        case 'subagent_finished': {
          const lane = this.agentLanes.get(ev.tag);
          if (lane) {
            this.laneLine(lane, `finished — ${ev.steps} step(s)`);
            this.finishLane(lane, '✓', true);
          }
          break;
        }
        case 'task_started':
          this.agentLane(`task #${ev.id}`, '⚙', '', ev.label);
          break;
        case 'task_finished': {
          const lane = this.agentLanes.get(`task #${ev.id}`);
          if (lane) {
            if (ev.preview) this.laneLine(lane, ev.preview, !ev.ok);
            this.finishLane(lane, ev.ok ? '✓' : '✗', ev.ok);
          } else {
            this.line('line-info', `⚙ background #${ev.id} ${ev.ok ? '✓' : '✗'} ${ev.label}`);
          }
          break;
        }
        case 'done': {
          this.busy = false;
          this.closeTurnBlocks();
          for (const lane of this.agentLanes.values()) {
            if (!lane.done && !lane.tag.startsWith('task #')) this.finishLane(lane, '◼', false);
          }
          const s = ev.stats || {};
          if (s.output_tokens) {
            this.line(
              'stats',
              `${s.output_tokens} tok · ${(s.tokens_per_sec || 0).toFixed(1)} tok/s · ${((s.duration_ms || 0) / 1000).toFixed(1)}s`
            );
          }
          break;
        }
      }
      this.renderStatus();
    }

    onExit(code, stderrTail) {
      this.running = false;
      this.busy = false;
      this.commands = [];
      for (const id of [...this.reviews.keys()]) this.reviewDone(id, 'cancelled — rift exited');
      if (code !== 0 && code != null) {
        this.line('line-warn', `! rift exited (code ${code})${stderrTail ? '\n' + stderrTail : ''}`);
      }
      this.renderStatus();
    }

    // ── @-mention and /skill completion (ported) ──────────────────────────
    mentionContext() {
      const pos = this.input.selectionStart;
      const m = this.input.value.slice(0, pos).match(/(?:^|\s)@([^\s@]*)$/);
      return m ? { start: pos - m[1].length - 1, query: m[1] } : null;
    }
    skillContext() {
      const pos = this.input.selectionStart;
      const m = this.input.value.slice(0, pos).match(/^\/(?:s(?:k(?:i(?:l(?:l:?)?)?)?)?)?([^\s/]*)$/);
      if (!m) return null;
      return { start: 0, query: m[1] || '', kind: 'skill' };
    }
    closeMention() {
      this.mention = null;
      this.mentionItems = [];
      this.mentionPopup.innerHTML = '';
      this.mentionPopup.classList.add('hidden');
    }
    updateMention() {
      const sctx = this.skillContext();
      if (sctx && this.skills.length) {
        this.mention = sctx;
        this.mentionSel = 0;
        this.mentionItems = this.skills
          .filter((s) => s.name.toLowerCase().startsWith(sctx.query.toLowerCase()))
          .slice(0, 12)
          .map((s) => ({ path: 'skill:' + s.name, skill: true, desc: s.description }));
        this.renderMention();
        return;
      }
      const ctx = this.mentionContext();
      if (!ctx) return this.closeMention();
      this.mention = ctx;
      const token = ++this.mentionToken;
      clearTimeout(this.mentionTimer);
      this.mentionTimer = setTimeout(() => {
        inv('query_files', { dir: this.dir, query: ctx.query })
          .then((results) => {
            if (token === this.mentionToken && this.mention) {
              this.mentionItems = results;
              this.mentionSel = 0;
              this.renderMention();
            }
          })
          .catch(() => {});
      }, 60);
    }
    renderMention() {
      this.mentionPopup.innerHTML = '';
      if (!this.mention || !this.mentionItems.length) {
        this.mentionPopup.classList.add('hidden');
        return;
      }
      this.mentionItems.forEach((it, i) => {
        const row = document.createElement('div');
        row.className = 'mention-item' + (i === this.mentionSel ? ' selected' : '');
        const slash = it.path.lastIndexOf('/');
        const name = document.createElement('span');
        name.className = 'mi-name';
        name.textContent = it.path.slice(slash + 1) + (it.dir ? '/' : '');
        row.appendChild(name);
        if (it.skill && it.desc) {
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
        row.addEventListener('mousedown', (e) => {
          e.preventDefault();
          this.pickMention(i);
        });
        row.addEventListener('mousemove', () => {
          if (this.mentionSel !== i) {
            this.mentionSel = i;
            this.renderMention();
          }
        });
        this.mentionPopup.appendChild(row);
      });
      this.mentionPopup.classList.remove('hidden');
      const sel = this.mentionPopup.children[this.mentionSel];
      if (sel) sel.scrollIntoView({ block: 'nearest' });
    }
    pickMention(i) {
      const it = this.mentionItems[i];
      if (!it || !this.mention) return;
      const end = this.input.selectionStart;
      const text = it.skill ? '/' + it.path + ' ' : '@' + it.path + (it.dir ? '/' : ' ');
      this.input.value =
        this.input.value.slice(0, this.mention.start) + text + this.input.value.slice(end);
      const caret = this.mention.start + text.length;
      this.input.setSelectionRange(caret, caret);
      this.input.focus();
      this.autosize();
      if (it.dir) this.updateMention();
      else this.closeMention();
    }
    onInputKey(e) {
      if (this.mention && this.mentionItems.length && !this.mentionPopup.classList.contains('hidden')) {
        if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
          e.preventDefault();
          const n = this.mentionItems.length;
          this.mentionSel = (this.mentionSel + (e.key === 'ArrowDown' ? 1 : n - 1)) % n;
          this.renderMention();
          return;
        }
        if (e.key === 'Enter' || e.key === 'Tab') {
          e.preventDefault();
          this.pickMention(this.mentionSel);
          return;
        }
        if (e.key === 'Escape') {
          e.preventDefault();
          this.closeMention();
          return;
        }
      }
      if (e.key === 'Enter' && !e.shiftKey) {
        e.preventDefault();
        this.submit();
      }
    }
    autosize() {
      this.input.style.height = 'auto';
      this.input.style.height = Math.min(this.input.scrollHeight, window.innerHeight * 0.4) + 'px';
    }
  }

  // ── Review modal (one at a time, per-hunk toggles) ──────────────────────
  const reviewModal = document.getElementById('review-modal');
  const reviewTitle = document.getElementById('review-title');
  const reviewHunks = document.getElementById('review-hunks');
  const reviewApplyBtn = document.getElementById('review-apply');
  const reviewRejectBtn = document.getElementById('review-reject');
  /** {tab, id} of the review currently in the modal. */
  let modalReview = null;

  function openReviewModal(tab, id) {
    const r = tab.reviews.get(id);
    if (!r) return;
    modalReview = { tab, id };
    reviewTitle.textContent = `✎ ${r.ev.tool}: ${r.ev.path}`;
    renderReviewModal();
    reviewModal.classList.remove('hidden');
  }
  function renderReviewModal() {
    if (!modalReview) return;
    const r = modalReview.tab.reviews.get(modalReview.id);
    if (!r) return;
    reviewHunks.innerHTML = '';
    let h = 0;
    for (const seg of r.segments) {
      if (seg.same) {
        // Context: a few lines either side keep the hunks anchored without
        // dumping the whole file into the modal.
        const pre = document.createElement('pre');
        pre.className = 'hunk-context';
        const lines = seg.lines;
        pre.textContent =
          lines.length > 7
            ? [...lines.slice(0, 3), `… ${lines.length - 6} unchanged lines …`, ...lines.slice(-3)].join('\n')
            : lines.join('\n');
        reviewHunks.appendChild(pre);
        continue;
      }
      const idx = h++;
      const box = document.createElement('div');
      box.className = 'hunk' + (r.accepted[idx] ? '' : ' rejected');
      const head = document.createElement('div');
      head.className = 'hunk-head';
      const check = document.createElement('input');
      check.type = 'checkbox';
      check.checked = r.accepted[idx];
      const label = document.createElement('span');
      label.textContent = `hunk ${idx + 1} · −${seg.oldLines.length} +${seg.newLines.length}`;
      head.appendChild(check);
      head.appendChild(label);
      head.addEventListener('click', (e) => {
        if (e.target !== check) check.checked = !check.checked;
        r.accepted[idx] = check.checked;
        renderReviewModal();
      });
      const pre = document.createElement('pre');
      for (const l of seg.oldLines) {
        const span = document.createElement('span');
        span.className = 'diff-del';
        span.textContent = '- ' + l + '\n';
        pre.appendChild(span);
      }
      for (const l of seg.newLines) {
        const span = document.createElement('span');
        span.className = 'diff-add';
        span.textContent = '+ ' + l + '\n';
        pre.appendChild(span);
      }
      box.appendChild(head);
      box.appendChild(pre);
      reviewHunks.appendChild(box);
    }
    const kept = r.accepted.filter(Boolean).length;
    reviewApplyBtn.textContent = kept
      ? `✔ Apply ${kept}/${r.accepted.length} hunk${r.accepted.length === 1 ? '' : 's'}`
      : 'nothing accepted — Apply rejects the edit';
  }
  function closeReviewModal(tab, id) {
    if (modalReview && modalReview.tab === tab && modalReview.id === id) {
      modalReview = null;
      reviewModal.classList.add('hidden');
    }
  }
  reviewApplyBtn.addEventListener('click', () => {
    if (modalReview) modalReview.tab.reviewApply(modalReview.id);
  });
  reviewRejectBtn.addEventListener('click', () => {
    if (modalReview) modalReview.tab.reviewReject(modalReview.id);
  });
  document.getElementById('review-close').addEventListener('click', () => {
    // Close the modal only — the card in the chat still resolves it.
    modalReview = null;
    reviewModal.classList.add('hidden');
  });

  // ── Sidebar: recent projects + sessions ─────────────────────────────────
  const recentList = document.getElementById('recent-list');
  const welcomeRecent = document.getElementById('welcome-recent');

  function renderRecent() {
    const items = settings.recentProjects || [];
    for (const el of [recentList, welcomeRecent]) {
      el.innerHTML = '';
      for (const r of items.slice(0, el === welcomeRecent ? 5 : 12)) {
        const row = document.createElement('div');
        row.className = 'side-item';
        row.textContent = basename(r.path);
        const sub = document.createElement('span');
        sub.className = 'sub';
        sub.textContent = r.path;
        row.appendChild(sub);
        row.addEventListener('click', () => openProject(r.path));
        el.appendChild(row);
      }
    }
  }

  function renderSessions(items) {
    sessionListEl.innerHTML = '';
    const tab = tabs.get(activeTabId);
    const when = (s) => {
      if (!s) return '';
      const diff = Date.now() / 1000 - s;
      if (diff < 3600) return `${Math.max(1, Math.round(diff / 60))}m ago`;
      if (diff < 86400) return `${Math.round(diff / 3600)}h ago`;
      if (diff < 86400 * 7) return `${Math.round(diff / 86400)}d ago`;
      return new Date(s * 1000).toLocaleDateString();
    };
    for (const it of items) {
      const row = document.createElement('div');
      row.className = 'side-item' + (tab && it.path === tab.sessionPath ? ' active' : '');
      row.textContent = it.title || '(untitled chat)';
      const sub = document.createElement('span');
      sub.className = 'sub';
      sub.textContent = `${when(it.saved_at)} · ${it.turns} turn${it.turns === 1 ? '' : 's'}${it.model ? ' · ' + it.model : ''}`;
      row.appendChild(sub);
      row.setAttribute('data-tip', it.cwd || '');
      row.addEventListener('click', () => {
        const t = tabs.get(activeTabId);
        if (!t || it.path === t.sessionPath) return;
        t.sessionPath = it.path;
        t.restart(['--resume', it.path]);
      });
      sessionListEl.appendChild(row);
    }
  }

  document.getElementById('btn-sessions-refresh').addEventListener('click', () => {
    const t = tabs.get(activeTabId);
    if (t && t.running) t.send({ cmd: 'list_sessions' });
  });

  // ── Shell actions ───────────────────────────────────────────────────────
  function updateWelcome() {
    welcomeEl.classList.toggle('hidden', tabs.size > 0);
  }

  async function openProject(dir) {
    if (!dir) {
      dir = await inv('pick_folder').catch(() => null);
      if (!dir) return;
    }
    touchRecent(dir);
    const tab = new Tab(dir);
    tab.start([]);
  }

  document.getElementById('btn-open-project').addEventListener('click', () => openProject());
  document.getElementById('welcome-open').addEventListener('click', () => openProject());
  document.getElementById('btn-new-tab').addEventListener('click', () => {
    const t = tabs.get(activeTabId);
    if (t) {
      const tab = new Tab(t.dir);
      tab.start([]);
    } else {
      openProject();
    }
  });
  document.getElementById('btn-collapse').addEventListener('click', () => {
    document.getElementById('app').classList.toggle('sidebar-collapsed');
  });
  document.getElementById('btn-theme').addEventListener('click', () => {
    settings.theme = settings.theme === 'light' ? 'dark' : 'light';
    applyTheme();
    saveSettings();
  });

  // ── Settings modal ──────────────────────────────────────────────────────
  const settingsModal = document.getElementById('settings-modal');
  const fields = {
    riftBin: document.getElementById('set-bin'),
    host: document.getElementById('set-host'),
    model: document.getElementById('set-model'),
    numCtx: document.getElementById('set-numctx'),
    temperature: document.getElementById('set-temp'),
    maxIterations: document.getElementById('set-iters'),
    effort: document.getElementById('set-effort'),
  };
  document.getElementById('btn-settings').addEventListener('click', () => {
    for (const [k, el] of Object.entries(fields)) el.value = settings[k] ?? '';
    settingsModal.classList.remove('hidden');
  });
  document.getElementById('set-close').addEventListener('click', () =>
    settingsModal.classList.add('hidden')
  );
  document.getElementById('set-save').addEventListener('click', () => {
    for (const [k, el] of Object.entries(fields)) settings[k] = el.value.trim();
    saveSettings();
    settingsModal.classList.add('hidden');
  });

  // ── Keyboard shortcuts ──────────────────────────────────────────────────
  document.addEventListener('keydown', (e) => {
    const mod = e.ctrlKey || e.metaKey;
    if (mod && e.key === 't') {
      e.preventDefault();
      document.getElementById('btn-new-tab').click();
    } else if (mod && e.key === 'w') {
      e.preventDefault();
      const t = tabs.get(activeTabId);
      if (t) t.close();
    } else if (mod && e.key >= '1' && e.key <= '9') {
      const list = [...tabs.values()];
      const t = list[Number(e.key) - 1];
      if (t) {
        e.preventDefault();
        t.activate();
      }
    } else if (e.key === 'Escape') {
      if (!settingsModal.classList.contains('hidden')) settingsModal.classList.add('hidden');
      else if (!reviewModal.classList.contains('hidden')) {
        modalReview = null;
        reviewModal.classList.add('hidden');
      }
    }
  });

  // ── Tooltips (ported) ───────────────────────────────────────────────────
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
    const r = t.getBoundingClientRect();
    const x = Math.max(4, Math.min(r.left + r.width / 2 - tip.offsetWidth / 2, window.innerWidth - tip.offsetWidth - 4));
    let y = r.top - tip.offsetHeight - 6;
    if (y < 4) y = r.bottom + 6;
    tip.style.left = x + 'px';
    tip.style.top = y + 'px';
  });
  document.addEventListener('mousedown', hideTip);

  // ── Backend event wiring ────────────────────────────────────────────────
  listen('rift-event', (e) => {
    const { tab: id, line } = e.payload;
    const tab = tabs.get(id);
    if (!tab) return;
    let ev;
    try {
      ev = JSON.parse(line);
    } catch {
      return;
    }
    tab.onEvent(ev);
  });
  listen('rift-exit', (e) => {
    const { tab: id, code, stderr } = e.payload;
    const tab = tabs.get(id);
    if (tab) tab.onExit(code, (stderr || '').split('\n').slice(-4).join('\n'));
  });

  // ── Boot ────────────────────────────────────────────────────────────────
  loadSettings().then(() => {
    renderRecent();
    updateWelcome();
  });
})();

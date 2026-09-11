/** An attachment-local immutable snapshot. Live xterm never receives history bytes. */
export type HistoryPage = {
  attachment_id: string;
  capture_id: string;
  total_lines: number;
  start: number;
  columns: number;
  lines: string[];
};
const palette = ['#000000', '#cd0000', '#00cd00', '#cdcd00', '#0000ee', '#cd00cd', '#00cdcd', '#e5e5e5', '#7f7f7f', '#ff0000', '#00ff00', '#ffff00', '#5c5cff', '#ff00ff', '#00ffff', '#ffffff'];
function color(n: number) {
  if (n < 16)
    return palette[n];
  if (n < 232) {
    const v = n - 16, c = [Math.floor(v / 36), Math.floor(v / 6) % 6, v % 6].map(x => x ? 55 + x * 40 : 0);
    return `rgb(${c.join(',')})`;
  }
  return `rgb(${Array(3).fill(8 + (n - 232) * 10).join(',')})`;
}
function extendedColor(parts: string[]): string {
  const mode = Number(parts[0]);
  if (mode === 5 && parts.length === 2 && /^\d+$/.test(parts[1]) && Number(parts[1]) <= 255)
    return color(Number(parts[1]));
  // Colon truecolor permits an omitted or explicit color-space slot.
  const channels = parts.length === 5 ? parts.slice(2) : parts.slice(1);
  if (mode === 2 && channels.length === 3 && channels.every(x => /^\d+$/.test(x) && Number(x) <= 255))
    return `rgb(${channels.map(Number).join(',')})`;
  return '';
}
/** Only SGR is interpreted; OSC/DCS/CSI and all other controls are inert. */
export function renderHistoryLine(line: string): DocumentFragment {
  const out = document.createDocumentFragment();
  let fg = '', bg = '', underlineColor = '', bold = false, dim = false, italic = false, strike = false, conceal = false, inverse = false, underline = 0;
  const tokens = line.match(/\x1b\][\s\S]*?(?:\x07|\x1b\\|$)|\x1b[P^_X][\s\S]*?(?:\x1b\\|$)|\x1b\[[0-?]*[ -/]*[@-~]|\x1b[ -/]*[@-~]|[^\x1b]+/g) || [];
  for (const token of tokens) {
    if (token.startsWith('\x1b')) {
      if (!/^\x1b\[[\d;:]*m$/.test(token))
        continue;
      const codes = token.slice(2, -1).split(';');
      for (let i = 0; i < codes.length; i++) {
        const parts = codes[i].split(':'), n = Number(parts[0]);
        if (n === 0) {
          fg = bg = underlineColor = '';
          bold = dim = italic = strike = conceal = inverse = false;
          underline = 0;
        }
        else if (n === 1)
          bold = true;
        else if (n === 2)
          dim = true;
        else if (n === 3)
          italic = true;
        else if (n === 4)
          underline = parts.length === 1 ? 1 : Number(parts[1]);
        else if (n === 7)
          inverse = true;
        else if (n === 8)
          conceal = true;
        else if (n === 9)
          strike = true;
        else if (n === 21)
          underline = 2;
        else if (n === 22)
          bold = dim = false;
        else if (n === 23)
          italic = false;
        else if (n === 24)
          underline = 0;
        else if (n === 27)
          inverse = false;
        else if (n === 28)
          conceal = false;
        else if (n === 29)
          strike = false;
        else if (n === 39)
          fg = '';
        else if (n === 49)
          bg = '';
        else if (n === 59)
          underlineColor = '';
        else if (n >= 30 && n <= 37)
          fg = color(n - 30);
        else if (n >= 90 && n <= 97)
          fg = color(n - 90 + 8);
        else if (n >= 40 && n <= 47)
          bg = color(n - 40);
        else if (n >= 100 && n <= 107)
          bg = color(n - 100 + 8);
        else if (n === 38 || n === 48 || n === 58) {
          let c = '';
          if (parts.length > 1)
            c = extendedColor(parts.slice(1));
          else {
            const count = codes[i + 1] === '5' ? 2 : codes[i + 1] === '2' ? 4 : 0;
            if (count) {
              c = extendedColor(codes.slice(i + 1, i + 1 + count));
              i += count;
            }
          }
          if (c) {
            if (n === 38)
              fg = c;
            else if (n === 48)
              bg = c;
            else
              underlineColor = c;
          }
        }
      }
    }
    else {
      const span = document.createElement('span');
      span.textContent = token.replace(/[\u0000-\u001f\u007f-\u009f]/g, '');
      span.style.color = conceal ? 'transparent' : inverse ? bg || '#101416' : fg;
      span.style.backgroundColor = inverse ? fg || '#e4e9e7' : bg;
      if (bold)
        span.style.fontWeight = 'bold';
      if (dim)
        span.style.opacity = '0.5';
      if (italic)
        span.style.fontStyle = 'italic';
      const decoration = [underline > 0 ? 'underline' : '', strike ? 'line-through' : ''].filter(Boolean).join(' ');
      if (decoration)
        span.style.textDecoration = decoration;
      if (underlineColor)
        span.style.textDecorationColor = underlineColor;
      if (underline > 1)
        span.style.textDecorationStyle = underline === 2 ? 'double' : underline === 3 ? 'wavy' : underline === 4 ? 'dotted' : 'dashed';
      out.append(span);
    }
  }
  return out;
}
export class HistoryView {
  readonly viewport = document.createElement('div');
  readonly content = document.createElement('div');
  readonly bottom = document.createElement('button');
  readonly retry = document.createElement('button');
  readonly loading = document.createElement('span');
  browsing = false;
  pending = false;
  private dirty = true;
  private capture = '';
  private lines: string[] = [];
  private start = 0;
  private total = 0;
  private ready = false;
  private before: number | null = null;
  private columns = 80;
  private row = 17;
  private cell = 8.4;
  private delta = 0;
  private touch: number | null = null;
  private touchFromLive = false;
  private abort = new AbortController();
  constructor(private pane: HTMLElement, private attachment: () => string, private request: (op: Record<string, unknown>) => void, private liveFocus: () => void, private metrics?: () => {
    rows: number;
    cols: number;
    font?: string;
    size?: number;
  }) {
    this.viewport.className = 'history-viewport';
    this.viewport.tabIndex = 0;
    this.viewport.setAttribute('aria-label', '终端历史');
    this.content.className = 'history-content';
    this.viewport.append(this.content);
    this.bottom.className = 'history-bottom';
    this.bottom.textContent = '↓';
    this.bottom.setAttribute('aria-label', '回到底部');
    this.bottom.onclick = () => this.live();
    this.retry.className = 'history-retry';
    this.retry.onclick = () => { this.retry.hidden = true; this.load(this.lines.length ? this.start : null); };
    this.loading.className = 'history-loading';
    this.loading.textContent = '正在加载历史…';
    this.loading.setAttribute('role', 'status');
    pane.append(this.viewport, this.bottom, this.retry, this.loading);
    this.hide();
    const options = { signal: this.abort.signal };
    pane.addEventListener('wheel', e => {
      if (!this.browsing) {
        e.preventDefault();
        e.stopImmediatePropagation();
        if (e.deltaY < 0 && this.attachment())
          this.enter(Math.abs(e.deltaY) * (e.deltaMode === 1 ? this.row : e.deltaMode === 2 ? this.height() : 1));
      }
      else if (e.target !== this.viewport && !this.viewport.contains(e.target as Node)) {
        e.preventDefault();
        e.stopImmediatePropagation();
        this.viewport.scrollTop += e.deltaY;
        this.scrolled();
      }
    }, { ...options, capture: true, passive: false });
    pane.addEventListener('touchstart', e => { this.touch = e.touches.length === 1 ? e.touches[0].clientY : null; this.touchFromLive = !this.browsing; }, { ...options, capture: true, passive: true });
    pane.addEventListener('touchmove', e => { if (this.touchFromLive && this.touch !== null && e.touches.length === 1) {
      const y = e.touches[0].clientY, delta = y - this.touch;
      if (this.browsing) {
        e.preventDefault();
        e.stopImmediatePropagation();
        this.viewport.scrollTop -= delta;
        this.touch = y;
        if (!this.lines.length)
          this.delta += delta;
        else
          this.scrolled();
      }
      else if (delta > 8 && this.attachment()) {
        e.preventDefault();
        e.stopImmediatePropagation();
        this.enter(delta);
        this.touch = y;
      }
      else if (Math.abs(delta) > 8) {
        e.preventDefault();
        e.stopImmediatePropagation();
      }
    } }, { ...options, capture: true, passive: false });
    pane.addEventListener('touchend', () => { this.touch = null; this.touchFromLive = false; }, options);
    pane.addEventListener('keydown', e => { if (e.shiftKey && ['PageUp', 'PageDown', 'Home', 'End'].includes(e.key)) {
      e.preventDefault();
      e.stopImmediatePropagation();
      if (e.key === 'End') {
        this.live();
        return;
      }
      if (!this.browsing) {
        if (e.key === 'PageUp' || e.key === 'Home')
          this.enter(this.height());
        return;
      }
      this.viewport.scrollTop = e.key === 'Home' ? 0 : this.viewport.scrollTop + (e.key === 'PageUp' ? -1 : 1) * this.height();
      this.scrolled();
    }
    else if (this.browsing) {
      e.stopPropagation();
    } }, { ...options, capture: true });
    this.viewport.addEventListener('scroll', () => this.scrolled(), options);
  }
  private height() { return this.viewport.clientHeight || this.pane.clientHeight || 400; }
  private hide() { this.viewport.hidden = this.bottom.hidden = this.retry.hidden = this.loading.hidden = true; }
  focus() { if (this.browsing)
    this.viewport.focus({ preventScroll: true });
  else
    this.liveFocus(); }
  output() { this.dirty = true; }
  enter(delta: number) {
    if (!this.attachment() || this.browsing)
      return;
    this.browsing = true;
    this.delta = Math.max(1, delta);
    this.viewport.hidden = this.bottom.hidden = false;
    this.retry.hidden = true;
    this.measure();
    this.viewport.focus({ preventScroll: true });
    if (this.dirty && !this.pending) {
      this.lines = [];
      this.start = 0;
      this.ready = false;
      this.capture = crypto.randomUUID();
      this.dirty = false;
      this.paint();
      this.load(null);
    }
    else if (!this.ready) {
      this.loading.hidden = !this.pending;
      this.load(null);
    }
    else {
      this.viewport.scrollTop = Math.max(0, this.lines.length * this.row - this.height() - this.delta);
      this.paint();
      if (this.viewport.scrollTop < this.row * 8)
        this.load(this.start);
    }
  }
  private load(before: number | null) {
    if (this.pending || !this.capture || !this.attachment() || before === 0)
      return;
    this.pending = true;
    this.before = before;
    this.loading.hidden = !this.browsing;
    this.request({ op: 'history', attachment_id: this.attachment(), capture_id: this.capture, before });
  }
  accept(p: HistoryPage) {
    if (p.attachment_id !== this.attachment() || p.capture_id !== this.capture || !this.pending)
      return;
    const first = this.before === null;
    if (!Number.isInteger(p.total_lines) || p.total_lines < 0 || p.total_lines > 50256 ||
      !Number.isInteger(p.start) || p.start < 0 || p.start > p.total_lines ||
      !Number.isInteger(p.columns) || p.columns < 1 || p.columns > 512 ||
      !Array.isArray(p.lines) || p.lines.some(l => typeof l !== 'string') || p.lines.length > 256 ||
      p.start + p.lines.length !== (first ? p.total_lines : this.before) ||
      (p.lines.length === 0 && (!first || p.total_lines !== 0)) ||
      (!first && (p.total_lines !== this.total || p.columns !== this.columns || p.start >= this.start))) {
      this.fail(this.capture);
      return;
    }
    this.pending = false;
    this.ready = true;
    this.loading.hidden = this.retry.hidden = true;
    this.columns = p.columns;
    this.total = p.total_lines;
    const oldTop = this.viewport.scrollTop;
    this.lines = first ? p.lines : p.lines.concat(this.lines);
    this.start = p.start;
    this.paint();
    this.viewport.scrollTop = first ? Math.max(0, this.lines.length * this.row - this.height() - this.delta) : oldTop + p.lines.length * this.row;
    this.paint();
    if (this.browsing && this.viewport.scrollTop < this.row * 8 && this.start > 0)
      this.load(this.start);
  }
  fail(capture?: string) { if (capture && capture !== this.capture)
    return; if (!this.pending)
    return; this.pending = false; this.loading.hidden = true; if (this.browsing) {
    this.retry.textContent = '历史加载失败 · 重试';
    this.retry.hidden = false;
  } }
  live() { this.browsing = false; this.hide(); this.liveFocus(); }
  reset() { this.pending = false; this.ready = false; this.total = 0; this.capture = ''; this.lines = []; this.dirty = true; this.browsing = false; this.content.replaceChildren(); this.hide(); }
  dispose() { this.reset(); this.abort.abort(); this.viewport.remove(); this.bottom.remove(); this.retry.remove(); this.loading.remove(); }
  measure() {
    const old = this.row, screen = this.pane.querySelector<HTMLElement>('.xterm-screen'), dimensions = screen?.getBoundingClientRect(), metrics = this.metrics?.();
    if (metrics?.rows && dimensions?.height)
      this.row = dimensions.height / metrics.rows;
    if (metrics?.cols && dimensions?.width)
      this.cell = dimensions.width / metrics.cols;
    if (metrics?.font)
      this.viewport.style.fontFamily = metrics.font;
    if (metrics?.size)
      this.viewport.style.fontSize = `${metrics.size}px`;
    const anchor = this.viewport.scrollTop / old * this.row;
    this.paint();
    this.viewport.scrollTop = anchor;
    this.paint();
  }
  private scrolled() { if (!this.browsing || !this.lines.length)
    return; const top = Math.max(0, this.viewport.scrollTop); if (top >= Math.max(0, this.lines.length * this.row - this.height()) - 1 && this.lines.length * this.row > this.height()) {
    this.live();
    return;
  } this.paint(); if (top < this.row * 8 && this.retry.hidden)
    this.load(this.start); }
  private paint() { this.content.style.height = `${this.lines.length * this.row}px`; this.content.style.minWidth = `${this.columns * this.cell}px`; const first = Math.max(0, Math.floor(this.viewport.scrollTop / this.row) - 8), last = Math.min(this.lines.length, first + Math.ceil(this.height() / this.row) + 16); const nodes = []; for (let i = first; i < last; i++) {
    const node = document.createElement('div');
    node.className = 'history-row';
    node.style.top = `${i * this.row}px`;
    node.style.height = node.style.lineHeight = `${this.row}px`;
    node.dataset.line = String(this.start + i);
    node.append(renderHistoryLine(this.lines[i]));
    nodes.push(node);
  } this.content.replaceChildren(...nodes); }
}

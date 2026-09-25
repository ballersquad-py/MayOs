// The DOM for MayOS's browser, built on a few natives in `__n` (see
// vendor/quickjs/mayos_js.c). Nodes live in Rust and are known by id.
(function (g) {
  "use strict";
  const N = __n;
  const W = { TEXT: 0, HTML: 1, ATTR: 2, TAG: 3, STYLE: 4, OUTER: 5, TYPE: 6, VALUE: 7, RMATTR: 8 };
  const wrappers = new Map();

  function wrap(id) {
    if (id === undefined || id === null || id < 0) return null;
    let w = wrappers.get(id);
    if (!w) {
      const type = N.get(id, W.TYPE, "");
      w = type === "3" ? new Text(id) : type === "9" ? document : new HTMLElement(id);
      wrappers.set(id, w);
    }
    return w;
  }
  const wrapAll = ids => ids.map(wrap);

  class EventTarget {
    addEventListener(type, fn, opts) {
      if (!fn) return;
      const l = this.__l || (this.__l = {});
      (l[type] || (l[type] = [])).push({ fn, once: !!(opts && opts.once), capture: opts === true || !!(opts && opts.capture) });
    }
    removeEventListener(type, fn) {
      const l = this.__l && this.__l[type];
      if (l) this.__l[type] = l.filter(x => x.fn !== fn);
    }
    dispatchEvent(ev) {
      ev.target = ev.target || this;
      fire(this, ev);
      return !ev.defaultPrevented;
    }
  }

  function fire(target, ev) {
    // Bubble from the target up to window.
    const path = [];
    for (let n = target; n; n = n === document ? g : (n === g ? null : n.parentNode || document)) {
      path.push(n);
      if (n === g) break;
    }
    for (const n of path) {
      ev.currentTarget = n;
      const list = n.__l && n.__l[ev.type];
      if (list) {
        for (const x of list.slice()) {
          try { x.fn.call(n, ev); } catch (e) { console.error(e && e.stack || String(e)); }
          if (x.once) n.removeEventListener(ev.type, x.fn);
          if (ev.__stopNow) break;
        }
      }
      const h = n["on" + ev.type];
      if (typeof h === "function") {
        try { if (h.call(n, ev) === false) ev.preventDefault(); } catch (e) { console.error(e && e.stack || String(e)); }
      }
      if (ev.__stop || !ev.bubbles) break;
    }
  }

  class Event {
    constructor(type, init) {
      init = init || {};
      this.type = type; this.bubbles = !!init.bubbles; this.cancelable = !!init.cancelable;
      this.defaultPrevented = false; this.timeStamp = N.now(); this.detail = init.detail;
    }
    preventDefault() { this.defaultPrevented = true; }
    stopPropagation() { this.__stop = true; }
    stopImmediatePropagation() { this.__stop = true; this.__stopNow = true; }
  }
  class CustomEvent extends Event {}

  class Node extends EventTarget {
    constructor(id) { super(); this.__id = id; }
    get nodeType() { return +N.get(this.__id, W.TYPE, ""); }
    get parentNode() { return wrap(N.rel(this.__id, 0)); }
    get parentElement() { const p = this.parentNode; return p && p.nodeType === 1 ? p : null; }
    get firstChild() { return wrap(N.rel(this.__id, 1)); }
    get nextSibling() { return wrap(N.rel(this.__id, 2)); }
    get previousSibling() { return wrap(N.rel(this.__id, 3)); }
    get lastChild() { return wrap(N.rel(this.__id, 4)); }
    get childNodes() { return wrapAll(N.children(this.__id)); }
    get textContent() { return N.get(this.__id, W.TEXT, ""); }
    set textContent(v) { N.set(this.__id, W.TEXT, "", String(v == null ? "" : v)); }
    get nodeValue() { return this.nodeType === 3 ? this.textContent : null; }
    set nodeValue(v) { if (this.nodeType === 3) this.textContent = v; }
    get ownerDocument() { return document; }
    get isConnected() { let n = this; while (n) { if (n === document.documentElement) return true; n = n.parentNode; } return false; }
    hasChildNodes() { return N.rel(this.__id, 1) >= 0; }
    appendChild(c) {
      if (c instanceof DocumentFragment) { for (const k of c.__kids.splice(0)) this.appendChild(k); return c; }
      N.insert(this.__id, c.__id, -1); return c;
    }
    insertBefore(c, ref) {
      if (c instanceof DocumentFragment) { for (const k of c.__kids.splice(0)) this.insertBefore(k, ref); return c; }
      N.insert(this.__id, c.__id, ref ? ref.__id : -1); return c;
    }
    removeChild(c) { N.remove(c.__id); return c; }
    replaceChild(n, old) { this.insertBefore(n, old); N.remove(old.__id); return old; }
    remove() { N.remove(this.__id); }
    contains(o) { for (let n = o; n; n = n.parentNode) if (n === this) return true; return false; }
    cloneNode(deep) {
      if (this.nodeType === 3) return document.createTextNode(this.textContent);
      const e = document.createElement(this.tagName.toLowerCase());
      for (const a of this.getAttributeNames()) e.setAttribute(a, this.getAttribute(a));
      if (deep) e.innerHTML = this.innerHTML;
      return e;
    }
  }

  class Text extends Node {
    get data() { return this.textContent; }
    set data(v) { this.textContent = v; }
    get nodeName() { return "#text"; }
  }

  class DocumentFragment extends Node {
    constructor() { super(-1); this.__kids = []; }
    appendChild(c) { this.__kids.push(c); return c; }
    get childNodes() { return this.__kids.slice(); }
    get children() { return this.__kids.filter(k => k.nodeType === 1); }
    get firstChild() { return this.__kids[0] || null; }
    querySelector() { return null; }
    querySelectorAll() { return []; }
  }

  function styleProp(name) {
    return name.replace(/[A-Z]/g, m => "-" + m.toLowerCase()).replace(/^webkit-/, "-webkit-");
  }
  function makeStyle(el) {
    const get = (t, k) => {
      if (k === "setProperty") return (p, v) => N.set(el.__id, W.STYLE, p, v == null ? "" : String(v));
      if (k === "getPropertyValue") return p => N.get(el.__id, W.STYLE, p) || "";
      if (k === "removeProperty") return p => N.set(el.__id, W.STYLE, p, "");
      if (k === "cssText") return N.get(el.__id, W.ATTR, "style") || "";
      if (typeof k !== "string") return undefined;
      return N.get(el.__id, W.STYLE, styleProp(k)) || "";
    };
    const set = (t, k, v) => {
      if (k === "cssText") N.set(el.__id, W.ATTR, "style", String(v));
      else N.set(el.__id, W.STYLE, styleProp(k), v == null ? "" : String(v));
      return true;
    };
    return new Proxy({}, { get, set });
  }

  class DOMTokenList {
    constructor(el) { this.el = el; }
    get list() { return (this.el.getAttribute("class") || "").split(/\s+/).filter(Boolean); }
    get length() { return this.list.length; }
    item(i) { return this.list[i] || null; }
    contains(c) { return this.list.includes(c); }
    add(...cs) { const l = this.list; for (const c of cs) if (!l.includes(c)) l.push(c); this.el.setAttribute("class", l.join(" ")); }
    remove(...cs) { this.el.setAttribute("class", this.list.filter(c => !cs.includes(c)).join(" ")); }
    toggle(c, force) {
      const has = this.contains(c);
      const want = force === undefined ? !has : !!force;
      if (want && !has) this.add(c); else if (!want && has) this.remove(c);
      return want;
    }
    replace(a, b) { if (this.contains(a)) { this.remove(a); this.add(b); } }
    forEach(f) { this.list.forEach(f); }
    toString() { return this.list.join(" "); }
    [Symbol.iterator]() { return this.list[Symbol.iterator](); }
  }

  class Element extends Node {
    get tagName() { return (N.get(this.__id, W.TAG, "") || "").toUpperCase(); }
    get nodeName() { return this.tagName; }
    get localName() { return this.tagName.toLowerCase(); }
    get id() { return this.getAttribute("id") || ""; }
    set id(v) { this.setAttribute("id", v); }
    get className() { return this.getAttribute("class") || ""; }
    set className(v) { this.setAttribute("class", v); }
    get classList() { return new DOMTokenList(this); }
    get style() { return this.__style || (this.__style = makeStyle(this)); }
    set style(v) { this.setAttribute("style", v); }
    get dataset() {
      const el = this;
      return new Proxy({}, {
        get: (t, k) => typeof k === "string" ? el.getAttribute("data-" + styleProp(k)) ?? undefined : undefined,
        set: (t, k, v) => { el.setAttribute("data-" + styleProp(k), v); return true; },
      });
    }
    getAttribute(n) { return N.get(this.__id, W.ATTR, String(n).toLowerCase()); }
    setAttribute(n, v) { N.set(this.__id, W.ATTR, String(n).toLowerCase(), String(v)); }
    removeAttribute(n) { N.set(this.__id, W.RMATTR, String(n).toLowerCase(), ""); }
    hasAttribute(n) { return this.getAttribute(n) !== null; }
    toggleAttribute(n, f) { const h = this.hasAttribute(n); const w = f === undefined ? !h : !!f; if (w) this.setAttribute(n, ""); else this.removeAttribute(n); return w; }
    getAttributeNames() { const s = N.get(this.__id, W.ATTR, "\u0000names"); return s ? s.split("\u0000") : []; }
    get attributes() { return this.getAttributeNames().map(name => ({ name, value: this.getAttribute(name) })); }
    get innerHTML() { return N.get(this.__id, W.HTML, ""); }
    set innerHTML(v) { N.set(this.__id, W.HTML, "", String(v == null ? "" : v)); }
    get outerHTML() { return N.get(this.__id, W.OUTER, ""); }
    get innerText() { return this.textContent; }
    set innerText(v) { this.textContent = v; }
    get children() { return this.childNodes.filter(n => n.nodeType === 1); }
    get childElementCount() { return this.children.length; }
    get firstElementChild() { return this.children[0] || null; }
    get lastElementChild() { const c = this.children; return c[c.length - 1] || null; }
    get nextElementSibling() { let n = this.nextSibling; while (n && n.nodeType !== 1) n = n.nextSibling; return n; }
    get previousElementSibling() { let n = this.previousSibling; while (n && n.nodeType !== 1) n = n.previousSibling; return n; }
    querySelector(s) { return wrap(N.query(this.__id, s, false)[0]); }
    querySelectorAll(s) { return wrapAll(N.query(this.__id, s, true)); }
    getElementsByTagName(t) { return this.querySelectorAll(t); }
    getElementsByClassName(c) { return this.querySelectorAll(c.split(/\s+/).filter(Boolean).map(x => "." + x).join("")); }
    matches(s) { const p = this.parentNode || document; return N.query(p.__id, s, true).includes(this.__id) || document.querySelectorAll(s).includes(this); }
    closest(s) { for (let n = this; n && n.nodeType === 1; n = n.parentNode) if (n.matches(s)) return n; return null; }
    append(...ns) { for (const n of ns) this.appendChild(typeof n === "string" ? document.createTextNode(n) : n); }
    prepend(...ns) { const f = this.firstChild; for (const n of ns) this.insertBefore(typeof n === "string" ? document.createTextNode(n) : n, f); }
    before(...ns) { const p = this.parentNode; for (const n of ns) p.insertBefore(typeof n === "string" ? document.createTextNode(n) : n, this); }
    after(...ns) { const p = this.parentNode, nx = this.nextSibling; for (const n of ns) p.insertBefore(typeof n === "string" ? document.createTextNode(n) : n, nx); }
    replaceWith(n) { this.before(n); this.remove(); }
    insertAdjacentHTML(pos, html) {
      const tmp = document.createElement("div"); tmp.innerHTML = html;
      const kids = tmp.childNodes;
      for (const k of kids) {
        if (pos === "beforeend") this.appendChild(k);
        else if (pos === "afterbegin") this.insertBefore(k, this.firstChild);
        else if (pos === "beforebegin") this.parentNode.insertBefore(k, this);
        else this.parentNode.insertBefore(k, this.nextSibling);
      }
    }
    insertAdjacentElement(pos, el) { this.insertAdjacentHTML(pos, ""); if (pos === "beforeend") this.appendChild(el); else if (pos === "afterbegin") this.insertBefore(el, this.firstChild); else if (pos === "beforebegin") this.parentNode.insertBefore(el, this); else this.parentNode.insertBefore(el, this.nextSibling); return el; }
    getBoundingClientRect() {
      const b = N.box(this.__id), y = b[1] - (+N.ask(6, "") || 0);
      return { x: b[0], y, left: b[0], top: y, width: b[2], height: b[3], right: b[0] + b[2], bottom: y + b[3] };
    }
    getClientRects() { return [this.getBoundingClientRect()]; }
    get offsetWidth() { return N.box(this.__id)[2]; }
    get offsetHeight() { return N.box(this.__id)[3]; }
    get offsetLeft() { return N.box(this.__id)[0]; }
    get offsetTop() { return N.box(this.__id)[1]; }
    get clientWidth() { return this.offsetWidth; }
    get clientHeight() { return this.offsetHeight; }
    get scrollHeight() { return this.offsetHeight; }
    get scrollWidth() { return this.offsetWidth; }
    scrollIntoView() { N.call(7, String(N.box(this.__id)[1]), ""); }
    focus() { document.activeElement = this; }
    blur() {}
    click() { dispatch(this.__id, "click", 0, 0, ""); }
    get hidden() { return this.hasAttribute("hidden"); }
    set hidden(v) { this.toggleAttribute("hidden", !!v); }
    get value() { const v = N.get(this.__id, W.VALUE, ""); return v == null ? "" : v; }
    set value(v) { N.set(this.__id, W.VALUE, "", String(v)); }
    get checked() { return this.hasAttribute("checked"); }
    set checked(v) { this.toggleAttribute("checked", !!v); }
    get disabled() { return this.hasAttribute("disabled"); }
    set disabled(v) { this.toggleAttribute("disabled", !!v); }
    get href() { const h = this.getAttribute("href"); return h == null ? "" : N.ask(7, h) || h; }
    set href(v) { this.setAttribute("href", v); }
    get src() { const h = this.getAttribute("src"); return h == null ? "" : N.ask(7, h) || h; }
    set src(v) { this.setAttribute("src", v); if (this.tagName === "IMG") setTimeout(() => fireSimple(this, "load"), 0); }
    get name() { return this.getAttribute("name") || ""; }
    get type() { return this.getAttribute("type") || ""; }
    get title() { return this.getAttribute("title") || ""; }
    set title(v) { this.setAttribute("title", v); }
    get tabIndex() { return +(this.getAttribute("tabindex") || -1); }
    getContext(kind) { return kind === "2d" ? canvasContext(this) : null; }
    get width() { return +(this.getAttribute("width") || (this.tagName === "CANVAS" ? 300 : 0)); }
    set width(v) { this.setAttribute("width", v); }
    get height() { return +(this.getAttribute("height") || (this.tagName === "CANVAS" ? 150 : 0)); }
    set height(v) { this.setAttribute("height", v); }
    get options() { return this.querySelectorAll("option"); }
    submit() {}
    reset() {}
    animate() { return { finished: Promise.resolve(), cancel() {}, play() {}, pause() {} }; }
    attachShadow() { return this; }
  }
  class HTMLElement extends Element {}

  function fireSimple(target, type) { const e = new Event(type); e.target = target; fire(target, e); }

  // ---- canvas (2D) -----------------------------------------------------
  // Drawing commands are collected and sent to the browser once per frame.
  const canvases = new Map();
  function parseColor(c) { return String(c); }
  function canvasContext(el) {
    let c = canvases.get(el.__id);
    if (c) return c;
    const ops = [];
    c = {
      canvas: el, fillStyle: "#000", strokeStyle: "#000", lineWidth: 1, font: "10px sans-serif", globalAlpha: 1,
      textAlign: "start", textBaseline: "alphabetic", lineCap: "butt", lineJoin: "miter", imageSmoothingEnabled: true,
      __ops: ops, __stack: [],
      save() { this.__stack.push([this.fillStyle, this.strokeStyle, this.lineWidth, this.font, this.globalAlpha, this.textAlign]); ops.push(["save"]); },
      restore() { const s = this.__stack.pop(); if (s) [this.fillStyle, this.strokeStyle, this.lineWidth, this.font, this.globalAlpha, this.textAlign] = s; ops.push(["restore"]); },
      fillRect(x, y, w, h) { ops.push(["fr", x, y, w, h, parseColor(this.fillStyle), this.globalAlpha]); },
      strokeRect(x, y, w, h) { ops.push(["sr", x, y, w, h, parseColor(this.strokeStyle), this.lineWidth, this.globalAlpha]); },
      clearRect(x, y, w, h) { ops.push(["cr", x, y, w, h]); },
      beginPath() { ops.push(["bp"]); },
      closePath() { ops.push(["cp"]); },
      moveTo(x, y) { ops.push(["mt", x, y]); },
      lineTo(x, y) { ops.push(["lt", x, y]); },
      rect(x, y, w, h) { ops.push(["mt", x, y], ["lt", x + w, y], ["lt", x + w, y + h], ["lt", x, y + h], ["cp"]); },
      arc(x, y, r, a0, a1, ccw) { ops.push(["arc", x, y, r, a0, a1, ccw ? 1 : 0]); },
      ellipse(x, y, rx, ry, rot, a0, a1) { ops.push(["arc", x, y, Math.max(rx, ry), a0, a1, 0]); },
      quadraticCurveTo(cx, cy, x, y) { ops.push(["lt", x, y]); },
      bezierCurveTo(a, b, c, d, x, y) { ops.push(["lt", x, y]); },
      fill() { ops.push(["fill", parseColor(this.fillStyle), this.globalAlpha]); },
      stroke() { ops.push(["stroke", parseColor(this.strokeStyle), this.lineWidth, this.globalAlpha]); },
      fillText(t, x, y) { ops.push(["ft", String(t), x, y, parseColor(this.fillStyle), this.font, this.textAlign, this.textBaseline]); },
      strokeText(t, x, y) { ops.push(["ft", String(t), x, y, parseColor(this.strokeStyle), this.font, this.textAlign, this.textBaseline]); },
      measureText(t) { const m = /(\d+)px/.exec(this.font); return { width: String(t).length * (m ? +m[1] : 10) * 0.55 }; },
      drawImage(img, a, b, c2, d, e, f, g2, h) {
        const src = img && (img.src || (img.__id !== undefined && N.get(img.__id, W.ATTR, "src")));
        if (!src) return;
        if (e === undefined) ops.push(["di", src, a, b, c2 === undefined ? -1 : c2, d === undefined ? -1 : d, -1, -1, -1, -1]);
        else ops.push(["di", src, e, f, g2, h, a, b, c2, d]);
      },
      translate(x, y) { ops.push(["tr", x, y]); },
      scale(x, y) { ops.push(["sc", x, y]); },
      rotate() {}, setTransform() { ops.push(["rt"]); }, resetTransform() { ops.push(["rt"]); }, transform() {},
      createLinearGradient() { return { addColorStop(o, c) { this.c = this.c || c; }, toString() { return this.c || "#000"; } }; },
      createRadialGradient() { return this.createLinearGradient(); },
      createPattern() { return "#888"; },
      getImageData(x, y, w, h) { return { width: w, height: h, data: new Uint8ClampedArray(w * h * 4) }; },
      putImageData() {}, createImageData(w, h) { return { width: w, height: h, data: new Uint8ClampedArray(w * h * 4) }; },
      setLineDash() {}, clip() {},
    };
    canvases.set(el.__id, c);
    return c;
  }
  function flushCanvases() {
    for (const [id, c] of canvases) {
      if (!c.__ops.length) continue;
      N.call(9, String(id), JSON.stringify(c.__ops));
      c.__ops.length = 0;
    }
  }

  // ---- document ------------------------------------------------------
  const document = Object.create(Node.prototype);
  document.__id = N.special(3);
  Object.defineProperties(document, {
    documentElement: { get: () => wrap(N.special(0)) },
    head: { get: () => wrap(N.special(1)) },
    body: { get: () => wrap(N.special(2)) },
    title: { get: () => N.ask(8, "") || "", set: v => N.call(3, String(v), "") },
    cookie: { get: () => N.ask(3, "") || "", set: v => N.call(8, String(v), "") },
    location: { get: () => g.location, set: v => { g.location.href = v; } },
    URL: { get: () => g.location.href },
    domain: { get: () => g.location.hostname },
    referrer: { value: "" },
    readyState: { value: "complete", writable: true },
    visibilityState: { value: "visible" },
    hidden: { value: false },
    defaultView: { get: () => g },
    nodeType: { value: 9 },
    nodeName: { value: "#document" },
    parentNode: { value: null },
    characterSet: { value: "UTF-8" },
    compatMode: { value: "CSS1Compat" },
    scrollingElement: { get: () => wrap(N.special(0)) },
    forms: { get: () => document.querySelectorAll("form") },
    images: { get: () => document.querySelectorAll("img") },
    links: { get: () => document.querySelectorAll("a[href]") },
  });
  Object.assign(document, {
    activeElement: null,
    getElementById: id => wrap(N.query(N.special(0), "#" + String(id).replace(/([^\w-])/g, "\\$1"), false)[0]),
    querySelector: s => wrap(N.query(N.special(0), s, false)[0]),
    querySelectorAll: s => wrapAll(N.query(N.special(0), s, true)),
    getElementsByTagName: t => wrapAll(N.query(N.special(0), t, true)),
    getElementsByClassName: c => document.querySelectorAll(c.split(/\s+/).filter(Boolean).map(x => "." + x).join("")),
    getElementsByName: n => document.querySelectorAll('[name="' + n + '"]'),
    createElement: t => wrap(N.create(String(t).toLowerCase(), false)),
    createElementNS: (ns, t) => wrap(N.create(String(t).toLowerCase(), false)),
    createTextNode: t => { const n = wrap(N.create("", true)); n.textContent = t; return n; },
    createComment: () => wrap(N.create("", true)),
    createDocumentFragment: () => new DocumentFragment(),
    createEvent: () => new Event(""),
    createRange: () => ({ selectNodeContents() {}, setStart() {}, setEnd() {}, getBoundingClientRect: () => ({ top: 0, left: 0, width: 0, height: 0 }), createContextualFragment(h) { const d = document.createElement("div"); d.innerHTML = h; const f = new DocumentFragment(); for (const k of d.childNodes) f.appendChild(k); return f; } }),
    write: html => { const b = document.body || document.documentElement; b.insertAdjacentHTML("beforeend", String(html)); },
    writeln: html => document.write(html + "\n"),
    open() {}, close() {},
    hasFocus: () => true,
    addEventListener: EventTarget.prototype.addEventListener,
    removeEventListener: EventTarget.prototype.removeEventListener,
    dispatchEvent: EventTarget.prototype.dispatchEvent,
    execCommand: () => false,
    elementFromPoint: () => null,
  });
  wrappers.set(document.__id, document);

  // ---- window --------------------------------------------------------
  const timers = new Map();
  let nextTimer = 1;
  let frames = [];
  function addTimer(fn, ms, args, repeat) {
    const id = nextTimer++;
    if (typeof fn === "string") { const code = fn; fn = () => (0, eval)(code); }
    timers.set(id, { fn, at: N.now() + Math.max(0, +ms || 0), ms: Math.max(repeat ? 4 : 0, +ms || 0), args, repeat });
    return id;
  }
  g.setTimeout = (fn, ms, ...a) => addTimer(fn, ms, a, false);
  g.setInterval = (fn, ms, ...a) => addTimer(fn, ms, a, true);
  g.clearTimeout = g.clearInterval = id => { timers.delete(id); };
  g.setImmediate = fn => addTimer(fn, 0, [], false);
  g.queueMicrotask = fn => Promise.resolve().then(fn);
  g.requestAnimationFrame = fn => { frames.push(fn); return frames.length; };
  g.cancelAnimationFrame = () => {};
  g.requestIdleCallback = fn => addTimer(() => fn({ timeRemaining: () => 10, didTimeout: false }), 1, [], false);

  // Called by the browser about 60 times a second; returns the delay (ms)
  // until the next timer, or -1 when nothing is scheduled.
  g.__tick = function () {
    const now = N.now();
    const due = [...timers.entries()].filter(([, t]) => t.at <= now).sort((a, b) => a[1].at - b[1].at);
    for (const [id, t] of due) {
      if (!timers.has(id)) continue;
      if (t.repeat) t.at = now + t.ms; else timers.delete(id);
      try { t.fn(...(t.args || [])); } catch (e) { console.error(e && e.stack || String(e)); }
    }
    if (frames.length) {
      const f = frames; frames = [];
      for (const fn of f) { try { fn(N.now()); } catch (e) { console.error(e && e.stack || String(e)); } }
    }
    flushCanvases();
    if (frames.length) return 16;
    let next = -1;
    for (const t of timers.values()) { const d = Math.max(0, t.at - N.now()); if (next < 0 || d < next) next = d; }
    return Math.ceil(next);
  };

  // Called by the browser for user input; returns 1 if the page
  // cancelled the default action (e.g. following a link).
  function dispatch(id, type, x, y, key) {
    const target = id >= 0 ? wrap(id) : document;
    const ev = new Event(type, { bubbles: true, cancelable: true });
    ev.target = target; ev.srcElement = target;
    ev.clientX = ev.pageX = ev.x = ev.offsetX = x; ev.clientY = ev.y = ev.offsetY = y; ev.pageY = y + (+N.ask(6, "") || 0);
    ev.button = 0; ev.buttons = type === "mousedown" ? 1 : 0; ev.which = 1;
    if (key) {
      const [k, code, mods = ""] = key.split("\u0001");
      ev.key = k; ev.code = code; ev.keyCode = ev.which = keyCodeOf(k);
      ev.shiftKey = mods.includes("s"); ev.ctrlKey = mods.includes("c"); ev.altKey = mods.includes("a"); ev.metaKey = false;
      ev.repeat = false;
    }
    ev.touches = []; ev.changedTouches = [];
    fire(target, ev);
    flushCanvases();
    return ev.defaultPrevented ? 1 : 0;
  }
  function keyCodeOf(k) {
    const m = { Enter: 13, Escape: 27, " ": 32, ArrowLeft: 37, ArrowUp: 38, ArrowRight: 39, ArrowDown: 40, Backspace: 8, Tab: 9, Delete: 46, Shift: 16, Control: 17, Alt: 18 };
    if (k in m) return m[k];
    return k.length === 1 ? k.toUpperCase().charCodeAt(0) : 0;
  }
  g.__dispatch = (id, type, x, y, key) => dispatch(id, type, x, y, key);

  // ---- storage, location, console, misc --------------------------------
  function storage(op) {
    return new Proxy({
      getItem: k => N.ask(1, op + String(k)),
      setItem: (k, v) => N.call(4, op + String(k), String(v)),
      removeItem: k => N.call(5, op + String(k), ""),
      clear: () => N.call(6, op, ""),
      key: i => (N.ask(2, op) || "").split("\n").filter(Boolean)[i] ?? null,
      get length() { return (N.ask(2, op) || "").split("\n").filter(Boolean).length; },
    }, {
      get: (t, k) => k in t ? t[k] : (typeof k === "string" ? t.getItem(k) ?? undefined : undefined),
      set: (t, k, v) => { t.setItem(k, v); return true; },
      deleteProperty: (t, k) => { t.removeItem(k); return true; },
    });
  }
  g.localStorage = storage("L");
  g.sessionStorage = storage("S");

  function parseUrl(href) {
    const m = /^([a-z]+:)\/\/([^/:?#]*)(:\d+)?([^?#]*)(\?[^#]*)?(#.*)?$/i.exec(href) || [];
    return { href, protocol: m[1] || "", hostname: m[2] || "", host: (m[2] || "") + (m[3] || ""), port: (m[3] || "").slice(1), pathname: m[4] || "/", search: m[5] || "", hash: m[6] || "", origin: (m[1] || "") + "//" + (m[2] || "") + (m[3] || "") };
  }
  g.location = new Proxy({}, {
    get: (t, k) => {
      const u = parseUrl(N.ask(0, "") || "");
      if (k === "assign" || k === "replace") return v => N.call(1, String(v), "");
      if (k === "reload") return () => N.call(1, u.href, "");
      if (k === "toString") return () => u.href;
      return u[k];
    },
    set: (t, k, v) => { if (k === "href") N.call(1, String(v), ""); else if (k === "hash") {} else if (k === "search") N.call(1, parseUrl(N.ask(0, "")).origin + parseUrl(N.ask(0, "")).pathname + v, ""); return true; },
  });
  g.URL = class URL {
    constructor(u, base) { const s = base ? N.ask(9, String(base) + "\u0000" + String(u)) || String(u) : String(u); Object.assign(this, parseUrl(s)); this.searchParams = new URLSearchParams(this.search); }
    toString() { return this.href; }
  };
  g.URLSearchParams = class URLSearchParams {
    constructor(s) { this.p = []; s = String(s || "").replace(/^\?/, ""); for (const kv of s.split("&").filter(Boolean)) { const [k, v = ""] = kv.split("="); this.p.push([decodeURIComponent(k.replace(/\+/g, " ")), decodeURIComponent(v.replace(/\+/g, " "))]); } }
    get(k) { const e = this.p.find(x => x[0] === k); return e ? e[1] : null; }
    getAll(k) { return this.p.filter(x => x[0] === k).map(x => x[1]); }
    has(k) { return this.p.some(x => x[0] === k); }
    set(k, v) { this.delete(k); this.p.push([k, String(v)]); }
    append(k, v) { this.p.push([k, String(v)]); }
    delete(k) { this.p = this.p.filter(x => x[0] !== k); }
    forEach(f) { this.p.forEach(([k, v]) => f(v, k)); }
    toString() { return this.p.map(([k, v]) => encodeURIComponent(k) + "=" + encodeURIComponent(v)).join("&"); }
    [Symbol.iterator]() { return this.p[Symbol.iterator](); }
  };
  g.history = { length: 1, state: null, back: () => N.call(10, "", ""), forward() {}, go() {}, pushState() {}, replaceState() {} };
  g.navigator = { userAgent: N.ask(4, ""), language: "en-US", languages: ["en-US", "en"], platform: "MayOS", onLine: true, cookieEnabled: true, hardwareConcurrency: 1, maxTouchPoints: 0, vendor: "MayOS", clipboard: { writeText: () => Promise.resolve() }, sendBeacon: () => true, serviceWorker: undefined };
  const view = () => (N.ask(5, "") || "1000,700").split(",").map(Number);
  Object.defineProperties(g, {
    innerWidth: { get: () => view()[0] }, innerHeight: { get: () => view()[1] },
    outerWidth: { get: () => view()[0] }, outerHeight: { get: () => view()[1] },
    scrollY: { get: () => +N.ask(6, "") || 0 }, pageYOffset: { get: () => +N.ask(6, "") || 0 },
    scrollX: { value: 0 }, pageXOffset: { value: 0 }, devicePixelRatio: { value: 1 },
  });
  g.screen = { width: 1280, height: 800, availWidth: 1280, availHeight: 800, colorDepth: 24 };
  g.scrollTo = g.scroll = (x, y) => N.call(7, String(typeof x === "object" ? x.top || 0 : y || 0), "");
  g.scrollBy = (x, y) => N.call(7, String((+N.ask(6, "") || 0) + (typeof x === "object" ? x.top || 0 : y || 0)), "");
  g.alert = m => N.call(2, String(m), "");
  g.confirm = m => { N.call(2, String(m), ""); return true; };
  g.prompt = () => null;
  g.open = u => { if (u) N.call(1, String(u), ""); return null; };
  g.close = () => {};
  g.focus = () => {}; g.blur = () => {};
  g.getComputedStyle = el => el && el.style ? new Proxy({}, { get: (t, k) => k === "getPropertyValue" ? p => el.style.getPropertyValue(p) : el.style[k] }) : {};
  g.matchMedia = q => ({ matches: /min-width:\s*(\d+)/.test(q) ? view()[0] >= +RegExp.$1 : /max-width:\s*(\d+)/.test(q) ? view()[0] <= +RegExp.$1 : false, media: q, addListener() {}, removeListener() {}, addEventListener() {}, removeEventListener() {} });
  g.console = {};
  for (const [name, lvl] of [["log", "log"], ["info", "log"], ["debug", "log"], ["warn", "warn"], ["error", "error"], ["trace", "log"], ["dir", "log"], ["table", "log"]]) {
    g.console[name] = (...a) => N.call(0, a.map(x => { try { return typeof x === "string" ? x : x instanceof Error ? x.stack || String(x) : JSON.stringify(x); } catch (e) { return String(x); } }).join(" "), lvl);
  }
  g.console.group = g.console.groupEnd = g.console.time = g.console.timeEnd = g.console.assert = g.console.count = () => {};
  g.performance = { now: () => N.now(), timing: {}, mark() {}, measure() {}, getEntriesByType: () => [], getEntriesByName: () => [] };
  g.Event = Event; g.CustomEvent = CustomEvent; g.EventTarget = EventTarget;
  g.KeyboardEvent = g.MouseEvent = g.PointerEvent = g.UIEvent = g.FocusEvent = g.InputEvent = g.TouchEvent = g.PopStateEvent = g.MessageEvent = Event;
  g.Node = Node; g.Element = Element; g.HTMLElement = HTMLElement; g.Text = Text; g.DocumentFragment = DocumentFragment; g.Document = Node;
  for (const n of ["HTMLDivElement", "HTMLSpanElement", "HTMLAnchorElement", "HTMLImageElement", "HTMLInputElement", "HTMLButtonElement", "HTMLCanvasElement", "HTMLFormElement", "HTMLScriptElement", "HTMLStyleElement", "HTMLIFrameElement", "HTMLVideoElement", "HTMLMediaElement", "HTMLTemplateElement", "HTMLSelectElement", "HTMLTextAreaElement", "SVGElement"]) g[n] = HTMLElement;
  g.Image = class { constructor(w, h) { const e = document.createElement("img"); if (w) e.width = w; if (h) e.height = h; return e; } };
  g.MutationObserver = g.ResizeObserver = g.IntersectionObserver = g.PerformanceObserver = class { constructor(cb) { this.cb = cb; } observe() {} unobserve() {} disconnect() {} takeRecords() { return []; } };
  g.fetch = () => Promise.reject(new TypeError("fetch is not supported yet"));
  g.XMLHttpRequest = class { open() {} send() { setTimeout(() => { this.readyState = 4; this.status = 0; this.onerror && this.onerror(new Event("error")); }, 0); } setRequestHeader() {} addEventListener(t, f) { this["on" + t] = f; } abort() {} getAllResponseHeaders() { return ""; } };
  g.WebSocket = class { constructor() { throw new Error("WebSocket is not supported"); } };
  g.Worker = class { constructor() { throw new Error("Worker is not supported"); } };
  g.atob = s => { const c = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"; let o = "", b = 0, n = 0; for (const ch of String(s).replace(/[^A-Za-z0-9+/]/g, "")) { b = (b << 6) | c.indexOf(ch); n += 6; if (n >= 8) { n -= 8; o += String.fromCharCode((b >> n) & 255); } } return o; };
  g.btoa = s => { const c = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"; let o = ""; s = String(s); for (let i = 0; i < s.length; i += 3) { const n = (s.charCodeAt(i) << 16) | ((s.charCodeAt(i + 1) || 0) << 8) | (s.charCodeAt(i + 2) || 0); o += c[(n >> 18) & 63] + c[(n >> 12) & 63] + (i + 1 < s.length ? c[(n >> 6) & 63] : "=") + (i + 2 < s.length ? c[n & 63] : "="); } return o; };
  g.TextEncoder = class { encode(s) { const u = unescape(encodeURIComponent(String(s))); const a = new Uint8Array(u.length); for (let i = 0; i < u.length; i++) a[i] = u.charCodeAt(i); return a; } };
  g.TextDecoder = class { decode(a) { let s = ""; for (const b of a || []) s += String.fromCharCode(b); try { return decodeURIComponent(escape(s)); } catch (e) { return s; } } };
  g.crypto = { getRandomValues: a => { for (let i = 0; i < a.length; i++) a[i] = Math.floor(Math.random() * 256); return a; }, randomUUID: () => "10000000-1000-4000-8000-100000000000".replace(/[018]/g, c => (c ^ Math.random() * 16 >> c / 4).toString(16)) };
  g.customElements = { define() {}, get() {}, whenDefined: () => Promise.resolve() };
  g.CSS = { supports: () => false, escape: s => String(s) };
  g.addEventListener = EventTarget.prototype.addEventListener;
  g.removeEventListener = EventTarget.prototype.removeEventListener;
  g.dispatchEvent = EventTarget.prototype.dispatchEvent;
  g.window = g.self = g.top = g.parent = g.globalThis = g;
  g.document = document;
  g.frames = [];
  g.__loaded = () => { fireSimple(document, "DOMContentLoaded"); document.readyState = "complete"; const e = new Event("load"); e.target = document; fire(g, e); };
})(globalThis);

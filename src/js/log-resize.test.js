const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const source = fs.readFileSync(`${__dirname}/app.js`, 'utf8');
const start = source.indexOf('    function setupBottomPanel() {');
const implementation = source.slice(start, source.indexOf('\n    let lastLogMessage', start));

function fixture(saved, { storageUnavailable = false, initiallyOpen = false } = {}) {
  function element() {
    const classes = new Set(), listeners = {};
    return {
      style: {}, attrs: {}, hidden: false,
      classList: { contains: v => classes.has(v), add: v => classes.add(v), remove: v => classes.delete(v),
        toggle(v) { classes.has(v) ? classes.delete(v) : classes.add(v); } },
      addEventListener(event, fn) { (listeners[event] ||= []).push(fn); },
      emit(event, data = {}) { for (const fn of listeners[event] || []) fn({ button: 0, pointerId: 1, clientY: 500, preventDefault() {}, ...data }); },
      setAttribute(key, value) { this.attrs[key] = value; },
      setPointerCapture(id) { this.capture = id; },
      hasPointerCapture(id) { return this.capture === id; },
      releasePointerCapture() { this.capture = null; this.emit('lostpointercapture'); },
    };
  }
  const body = element(), handle = element(), toggle = element(), arrow = element(), app = element();
  if (initiallyOpen) body.classList.add('open');
  const panel = element(), win = element(), doc = { body: element() };
  let height = 800, stored = saved, observed;
  body.getBoundingClientRect = () => ({ height: body.classList.contains('open') ? parseFloat(body.style.height) || 210 : 0 });
  app.getBoundingClientRect = () => ({ height });
  panel.getBoundingClientRect = () => ({ height: body.getBoundingClientRect().height + (handle.hidden ? 46 : 58) });
  const controls = () => ({ getBoundingClientRect: () => ({ height: 50 }) });
  const elements = { '#log-resize-handle': handle, '#bottom-panel': panel, '#app': app,
    '#status-strip': controls(), '#left-panel .panel-header': controls(), '#toolbar': controls(), '#destination-bar': controls() };
  vm.runInNewContext(`${implementation}; setupBottomPanel();`, {
    $: selector => elements[selector], bottomPanelBody: body, bottomPanelToggle: toggle, bottomPanelArrow: arrow,
    document: doc, window: win, scheduleConfigSave() {},
    localStorage: {
      getItem() { if (storageUnavailable) throw new Error('Blocked'); return stored; },
      setItem(_, value) { if (storageUnavailable) throw new Error('Blocked'); stored = value; },
    },
    ResizeObserver: class { constructor(fn) { observed = fn; } observe() {} },
  });
  return { body, handle, toggle, win, doc, observed,
    open: () => toggle.emit('click'), size: () => parseFloat(body.style.height),
    resize(value) { height = value; win.emit('resize'); }, stored: () => stored,
    drag(to) { handle.emit('pointerdown'); handle.emit('pointermove', { clientY: to }); handle.emit('pointerup'); },
  };
}

test('dragging up expands logs and dragging down shrinks them within safe bounds', () => {
  const f = fixture(); f.open();
  f.drag(400); assert.equal(f.size(), 310);
  f.drag(550); assert.equal(f.size(), 260);
  f.drag(-5000); assert.equal(f.size(), 360);
  f.drag(5000); assert.equal(f.size(), 80);
  assert.equal(f.handle.attrs['aria-valuenow'], '80');
  assert.equal(f.doc.body.classList.contains('resizing-log'), false);
});

test('collapse/reopen and rerender keep selected size; saved size restores', () => {
  const f = fixture(); f.open(); f.drag(420);
  f.open(); assert.equal(f.handle.hidden, true);
  f.open(); f.observed(); assert.equal(f.size(), 290);
  assert.equal(f.toggle.attrs['aria-expanded'], 'true');
  const restored = fixture(f.stored()); restored.open(); assert.equal(restored.size(), 290);
});

test('window resize preserves upper workspace and restores preferred size later', () => {
  const f = fixture(340); f.open();
  f.resize(500); assert.equal(f.size(), 182);
  f.resize(300); assert.equal(f.size(), 0);
  f.resize(800); assert.equal(f.size(), 340);
});

test('keyboard controls resize accessibly and cancel/blur terminate pointer drag', () => {
  const f = fixture(); f.open();
  f.handle.emit('keydown', { key: 'ArrowUp' }); assert.equal(f.size(), 230);
  f.handle.emit('keydown', { key: 'ArrowDown' }); assert.equal(f.size(), 210);
  f.handle.emit('keydown', { key: 'End' }); assert.equal(f.size(), 360);
  f.handle.emit('keydown', { key: 'Home' }); assert.equal(f.size(), 80);
  for (const event of ['pointercancel', 'lostpointercapture', 'blur']) {
    f.handle.emit('pointerdown');
    (event === 'blur' ? f.win : f.handle).emit(event);
    f.handle.emit('pointermove', { clientY: 0 });
    assert.equal(f.size(), 80);
    assert.equal(f.doc.body.classList.contains('resizing-log'), false);
  }
  f.handle.emit('pointerdown', { button: 2 });
  f.handle.emit('pointermove', { clientY: 0 }); assert.equal(f.size(), 80);
});

test('unavailable local storage does not prevent initialization', () => {
  const f = fixture(undefined, { storageUnavailable: true });
  f.open(); assert.equal(f.size(), 210);
  f.drag(450); assert.equal(f.size(), 260);
});

test('an already open drawer initializes its handle and accessibility state', () => {
  const f = fixture('invalid', { initiallyOpen: true });
  assert.equal(f.handle.hidden, false);
  assert.equal(f.size(), 210);
  assert.equal(f.toggle.attrs['aria-expanded'], 'true');
});

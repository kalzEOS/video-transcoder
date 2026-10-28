const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');

const app = fs.readFileSync(`${__dirname}/app.js`, 'utf8');
const start = app.indexOf('    function setupSplitter() {');
const implementation = app.slice(start, app.indexOf('\n    // ═', start));

function fixture() {
  function element() {
    const listeners = {}, classes = new Set();
    return {
      style: { removeProperty(name) { delete this[name === 'flex-basis' ? 'flexBasis' : name]; } },
      attrs: {}, listeners,
      classList: {
        add: name => classes.add(name), remove: name => classes.delete(name),
        contains: name => classes.has(name),
        toggle(name, enabled) { enabled ? classes.add(name) : classes.delete(name); },
      },
      addEventListener(name, fn) { (listeners[name] ||= []).push(fn); },
      emit(name, values = {}) {
        const event = { button: 0, clientX: 0, preventDefault() {}, stopPropagation() {}, ...values };
        for (const fn of listeners[name] || []) fn(event);
      },
      setAttribute(name, value) { this.attrs[name] = value; },
    };
  }
  const rail = element(), workspace = element(), splitter = element(), toggle = element();
  const main = element(), document = element(), window = element(), timers = [];
  document.body = element();
  let horizontal = true;
  rail.getBoundingClientRect = () => ({ width: rail.classList.contains('collapsed') ? 0 : parseFloat(rail.style.flexBasis) || 320 });
  main.getBoundingClientRect = () => ({ left: 100, width: 1400 });
  splitter.getBoundingClientRect = () => ({ width: 9 });
  const elements = { '#right-panel': rail, '#left-panel': workspace, '#splitter': splitter, '#splitter-toggle': toggle, '#main-area': main };
  vm.runInNewContext(`let settingsPanelCollapsed = false; ${implementation}; setupSplitter();`, {
    $: id => elements[id], document, window,
    setTimeout: fn => timers.push(fn),
    getComputedStyle: el => el === main ? { flexDirection: horizontal ? 'row-reverse' : 'column-reverse' }
      : { minWidth: el === rail ? '258px' : '340px', maxWidth: '460px', getPropertyValue: () => '320px' },
  });
  return { rail, workspace, splitter, toggle, document, window,
    narrow(value) { horizontal = !value; window.emit('resize'); },
    drag(from, to) { splitter.emit('mousedown', { clientX: from, target: toggle }); document.emit('mousemove', { clientX: to }); document.emit('mouseup'); },
    flush() { timers.splice(0).forEach(fn => fn()); },
  };
}

test('resizes the settings rail in both directions without assigning queue widths', () => {
  const f = fixture();
  f.drag(425, 475); assert.equal(f.rail.style.flexBasis, '370px');
  f.drag(475, 395); assert.equal(f.rail.style.flexBasis, '290px');
  f.drag(395, -1000); assert.equal(f.rail.style.flexBasis, '258px');
  f.drag(395, 2000); assert.equal(f.rail.style.flexBasis, '460px');
  assert.equal(f.workspace.style.width, undefined);
  assert.equal(f.workspace.style.flex, undefined);
});

test('dragging the toggle does not collapse; clicking restores the selected width', () => {
  const f = fixture();
  f.drag(320, 370); f.toggle.emit('click');
  assert.equal(f.rail.classList.contains('collapsed'), false);
  f.flush(); f.toggle.emit('click');
  assert.equal(f.rail.classList.contains('collapsed'), true);
  assert.equal(f.rail.style.flexBasis, undefined);
  assert.equal(f.toggle.attrs['aria-expanded'], 'false');
  f.toggle.emit('click');
  assert.equal(f.rail.style.flexBasis, '370px');
  assert.equal(f.toggle.attrs['aria-expanded'], 'true');
});

test('tiny pointer movement remains a click and window blur ends a drag', () => {
  const f = fixture();
  f.drag(320, 322); f.toggle.emit('click');
  assert.equal(f.rail.classList.contains('collapsed'), true);
  f.flush(); f.toggle.emit('click');
  f.splitter.emit('mousedown', { clientX: 320 }); f.document.emit('mousemove', { clientX: 350 });
  assert.equal(f.document.body.classList.contains('dragging'), true);
  f.window.emit('blur'); f.document.emit('mousemove', { clientX: 450 });
  assert.equal(f.document.body.classList.contains('dragging'), false);
  assert.equal(f.rail.style.flexBasis, '350px');
  f.toggle.emit('click');
  assert.equal(f.rail.classList.contains('collapsed'), true);
});

test('narrow layout clears pixel flex-basis and restores the width on return', () => {
  const f = fixture();
  f.drag(320, 400); f.narrow(true);
  assert.equal(f.rail.style.flexBasis, undefined);
  f.narrow(false); assert.equal(f.rail.style.flexBasis, '400px');
  f.flush(); f.toggle.emit('click'); f.narrow(true); f.narrow(false);
  assert.equal(f.rail.style.flexBasis, undefined);
  f.toggle.emit('click'); assert.equal(f.rail.style.flexBasis, '400px');
});

test('non-primary mouse buttons do not start resizing', () => {
  const f = fixture();
  f.splitter.emit('mousedown', { button: 2, clientX: 320 });
  f.document.emit('mousemove', { clientX: 400 });
  assert.equal(f.rail.style.flexBasis, '320px');
});

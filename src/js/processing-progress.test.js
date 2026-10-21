const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const { formatDuration, formatEta } = require('./lib.js');

// Exercise the actual event handlers without starting Tauri or the full app.
function harness() {
  const source = fs.readFileSync(__dirname + '/app.js', 'utf8');
  const listeners = {};
  function element() {
    const classes = new Set();
    return { style: {}, attrs: {}, textContent: '',
      classList: { toggle(name, on) { on ? classes.add(name) : classes.delete(name); }, add(name) { classes.add(name); }, remove(name) { classes.delete(name); }, contains(name) { return classes.has(name); } },
      setAttribute(name, value) { this.attrs[name] = value; }, removeAttribute(name) { delete this.attrs[name]; } };
  }
  const context = vm.createContext({ listen(name, fn) { listeners[name] = fn; },
    progressBar: element(), progressBarContainer: element(), jobStatus: element(), jobProgressText: element(),
    batchProgressBar: element(), batchProgressBarContainer: element(),
    performance: { now: () => context.now }, now: 1000, fileEncodeStart: 0,
    document: { title: '' }, defaultTitle: 'HISTV', announceStatus() {},
    batchTotalFiles: 1, batchCurrentFile: 1, currentEncodingIndex: -1,
    formatDuration, formatEta });
  const start = source.indexOf('// Stage percentages describe');
  const end = source.indexOf('// Queue item updated (status change)', start);
  assert.ok(start >= 0 && end > start);
  vm.runInContext(source.slice(start, end), context);
  return { context, emit(name, payload) { listeners[name]({ payload }); } };
}

test('unmeasured preprocessing animates without percentage or ETA', () => {
  const { context:c, emit } = harness();
  emit('processing-stage', { stage:'preparing', message:'Extracting video bitstream...', percent:null });
  assert.equal(c.jobStatus.textContent, 'Preparing video');
  assert.equal(c.jobProgressText.textContent, 'Extracting video bitstream...');
  assert.ok(c.progressBarContainer.classList.contains('is-indeterminate'));
  assert.equal(c.progressBarContainer.attrs['aria-valuenow'], undefined);
  assert.equal(c.fileEncodeStart, 0);
});

test('measured stages stay separate from encoding and its timer', () => {
  const { context:c, emit } = harness();
  emit('processing-stage', { stage:'preparing', message:'Preparing HDR10+ metadata...', percent:25 });
  assert.equal(c.progressBar.style.width, '25.0%');
  assert.equal(c.progressBarContainer.attrs['aria-valuenow'], 25);
  assert.ok(!c.progressBarContainer.classList.contains('is-indeterminate'));
  assert.equal(c.batchProgressBar.style.width, undefined);
  c.now = 301000; // Five minutes of preparation must not enter the encode ETA.
  emit('processing-stage', { stage:'encoding', message:'Starting hardware encoding...', percent:null });
  assert.equal(c.fileEncodeStart, 301000);
  c.now = 302000;
  emit('file-progress', { percent:10, timeSecs:10, totalSecs:100, pass:null });
  assert.equal(c.jobStatus.textContent, 'Encoding video');
  assert.ok(!c.progressBarContainer.classList.contains('is-indeterminate'));
  assert.match(c.jobProgressText.textContent, /ETA 9s/);
  emit('processing-stage', { stage:'reinserting', message:'Reinserting HDR10+ metadata...', percent:null });
  assert.equal(c.jobStatus.textContent, 'Reinserting HDR metadata');
  assert.ok(!c.jobProgressText.textContent.includes('ETA'));
  emit('processing-stage', { stage:'finalizing', message:'Validating output...', percent:null });
  assert.equal(c.jobStatus.textContent, 'Finalizing output');
});

test('failure, cancellation and completion stop the animation', () => {
  for (const stage of ['failed', 'cancelled', 'complete']) {
    const { context:c, emit } = harness();
    emit('processing-stage', { stage:'preparing', message:'Scanning metadata...', percent:null });
    emit('processing-stage', { stage, message:stage, percent:stage === 'complete' ? 100 : null });
    assert.ok(!c.progressBarContainer.classList.contains('is-indeterminate'));
    assert.equal(c.jobProgressText.textContent, stage === 'complete' ? 'complete (100%)' : stage);
  }
});

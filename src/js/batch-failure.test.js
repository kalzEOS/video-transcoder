const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');

function harness() {
  const source = fs.readFileSync(__dirname + '/app.js', 'utf8');
  const listeners = {};
  const element = () => ({ textContent: '', style: {},
    classList: { add() {}, remove() {} }, setAttribute() {} });
  const context = vm.createContext({
    listen(name, handler) { listeners[name] = handler; },
    batchHadFailures: false, batchWasCancelled: false, batchRunning: true,
    queueData: [{ status: 'Pending' }], starts: 0,
    startBatch() { context.starts++; }, fetchQueue: async () => {}, renderQueue() {},
    document: { title: '', querySelector: () => null }, defaultTitle: 'HISTV',
    selectedRows: new Set(), chkAutoClear: { checked: false },
    announceStatus() {}, updateBatchButtons() {},
    invoke: async () => {}, console,
    btnPause: element(), jobStatus: element(), statusCmd: element(), jobFile: element(),
    jobProgressText: element(), progressBar: element(), batchProgressBar: element(),
    progressBarContainer: element(), batchProgressBarContainer: element(),
  });
  for (const [begin, end] of [
    ['// Batch started', '// Batch progress (file count)'],
    ['// Batch finished', '// Overwrite prompt from backend'],
  ]) {
    const start = source.indexOf(begin);
    const stop = source.indexOf(end, start);
    assert.ok(start >= 0 && stop > start);
    vm.runInContext(source.slice(start, stop), context);
  }
  return { context, async finish(failed) {
    await listeners['batch-finished']({ payload: { failed } });
    await listeners['queue-sync-complete']();
  }, start() { listeners['batch-started'](); } };
}

test('failed batch never automatically restarts pending files', async () => {
  const h = harness();
  await h.finish(1);
  assert.equal(h.context.starts, 0);
  // Duplicate queue synchronization must not retry a stopped batch either.
  await h.finish(1);
  assert.equal(h.context.starts, 0);
});

test('successful batches still continue new files; manual restart clears failure state', async () => {
  const h = harness();
  await h.finish(1);
  h.start();
  assert.equal(h.context.batchHadFailures, false);
  await h.finish(0);
  assert.equal(h.context.starts, 1);
});

test('user cancellation continues to block automatic continuation', async () => {
  const h = harness();
  h.context.batchWasCancelled = true;
  await h.finish(0);
  assert.equal(h.context.starts, 0);
});

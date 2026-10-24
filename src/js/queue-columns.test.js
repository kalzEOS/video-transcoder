const { test } = require('node:test');
const assert = require('node:assert/strict');
const { MINIMUMS, normalizeWidths, totalWidth } = require('./queue-columns.js');

test('column widths never collapse below readable minimums', () => {
  const widths = normalizeWidths(MINIMUMS.map(() => 1));
  assert.deepEqual(widths, MINIMUMS);
});

test('table width is the sum of the persisted column widths', () => {
  const widths = normalizeWidths([36, 400, 100, 140, 160, 90, 120, 130, 150]);
  assert.equal(totalWidth(widths), 1326);
});

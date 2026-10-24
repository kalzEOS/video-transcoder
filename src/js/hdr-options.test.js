const { test } = require('node:test');
const assert = require('node:assert/strict');
const { profileHdrSelection } = require('./hdr-options.js');

test('quality presets preserve an HDR10+ selection, including one loaded from config', () => {
  assert.deepEqual(profileHdrSelection(false, true, false), {
    preserveHdr: false, preserveHdr10plus: true,
  });
});

test('quality presets preserve HDR10 or explicit SDR selection', () => {
  assert.deepEqual(profileHdrSelection(true, false, false), {
    preserveHdr: true, preserveHdr10plus: false,
  });
  assert.deepEqual(profileHdrSelection(false, false, false), {
    preserveHdr: false, preserveHdr10plus: false,
  });
});

test('Compatibility mode deliberately selects SDR', () => {
  assert.deepEqual(profileHdrSelection(false, true, true), {
    preserveHdr: false, preserveHdr10plus: false,
  });
});

const { test } = require('node:test');
const assert = require('node:assert/strict');
const { computeEstimatedSize, computeTargetBitrateLabel } = require('./lib.js');
test('below-target Compatibility conversion has a variable size instead of source-copy size', () => {
  const settings = { target: 10, targetCodecFamily: 'h264', compatibilityMode: true, rateControlMode: 'CQP', qpI: 18, qpP: 20 };
  const source = { videoCodec: 'hevc', videoBitrateMbps: 2, sourceBytes: 1234, durationSecs: 60 };
  assert.equal(computeEstimatedSize(source, settings).variable, true);
  assert.doesNotMatch(computeTargetBitrateLabel(source, settings), /copy/i);
  assert.equal(computeEstimatedSize({ ...source, videoCodec: 'h264' }, settings).bytes, 1234);
  assert.equal(computeEstimatedSize(source, { ...settings, compatibilityMode: false }).bytes, 1234);
});

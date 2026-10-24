const { test } = require('node:test');
const assert = require('node:assert/strict');
const { profileHdrSelection, hdrModeAvailability } = require('./hdr-options.js');

test('quality presets preserve an HDR10+ selection, including one loaded from config', () => {
  assert.equal(profileHdrSelection('hdr10plus', false), 'hdr10plus');
});

test('quality presets preserve Auto, HDR10 or explicit SDR selection', () => {
  for (const mode of ['auto', 'hdr10', 'sdr']) {
    assert.equal(profileHdrSelection(mode, false), mode);
  }
});

test('Compatibility mode deliberately selects SDR', () => {
  assert.equal(profileHdrSelection('hdr10plus', true), 'sdr');
});

test('only compatible queued sources enable HDR10+ while early metadata absence remains inconclusive', () => {
  const file = { status: 'Pending', videoCodec: 'hevc', videoFormat: { bitDepth: 10 },
    isHdr: true, colorTransfer: 'smpte2084', fullPath: '/tmp/movie.mkv', hasHdr10plus: false };
  assert.deepEqual(hdrModeAvailability([file], false), { hdr10Reason: '', hdr10plusReason: '' });
  assert.match(hdrModeAvailability([file, { ...file, videoCodec: 'av1' }], false).hdr10plusReason, /HEVC/);
  assert.match(hdrModeAvailability([file], true).hdr10Reason, /Compatibility/);
  assert.match(hdrModeAvailability([{ ...file, videoFormat: { bitDepth: 8 } }], false).hdr10Reason, /10-bit/);
  assert.match(hdrModeAvailability([{ ...file, isHdr: false, colorTransfer: 'bt709' }], false).hdr10plusReason, /HDR10\/PQ/);
  assert.match(hdrModeAvailability([{ ...file, videoCodec: 'h264' }], false).hdr10Reason, /supported HDR encoder/);
  assert.equal(hdrModeAvailability([{ ...file, fullPath: '/tmp/movie.m2ts' }], false).hdr10plusReason, '');
});

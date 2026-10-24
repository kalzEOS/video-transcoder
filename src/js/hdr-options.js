// Quality presets change encoding quality; only Compatibility mode changes HDR.
function profileHdrSelection(mode, compatibilityMode) {
  return compatibilityMode ? 'sdr' : mode;
}

function hdrModeAvailability(items, compatibilityMode) {
  const files = items.filter(item => item.videoCodec &&
    ['pending', 'probing'].includes((item.status || 'pending').toLowerCase()));
  const hdr10Reason = compatibilityMode
    ? 'Compatibility mode outputs H.264/SDR.'
    : files.some(item => !item.isHdr || item.colorTransfer !== 'smpte2084' ||
        item.videoFormat?.bitDepth !== 10 || item.videoCodec === 'h264')
      ? 'Every pending file must be 10-bit HDR10/PQ with a supported HDR encoder.' : '';
  const hdr10plusReason = compatibilityMode
    ? 'Compatibility mode outputs H.264/SDR.'
    : files.some(item => !item.isHdr || item.colorTransfer !== 'smpte2084' ||
        item.videoCodec !== 'hevc' || item.videoFormat?.bitDepth !== 10)
      ? 'Every pending file must be 10-bit HDR10/PQ HEVC.' : '';
  return { hdr10Reason, hdr10plusReason };
}

if (typeof module !== 'undefined') module.exports = { profileHdrSelection, hdrModeAvailability };

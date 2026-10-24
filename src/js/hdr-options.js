// Quality presets change encoding quality; only Compatibility mode changes HDR.
function profileHdrSelection(preserveHdr, preserveHdr10plus, compatibilityMode) {
  return compatibilityMode
    ? { preserveHdr: false, preserveHdr10plus: false }
    : { preserveHdr, preserveHdr10plus };
}

if (typeof module !== 'undefined') module.exports = { profileHdrSelection };

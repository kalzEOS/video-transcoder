// Shared column sizing for the queue header and every body row.
// The colgroup is the single source of truth; rerendering tbody cannot reset it.
(() => {
  const STORAGE_KEY = 'histv.queueColumnWidths.v1';
  const DEFAULTS = [36, 280, 110, 130, 130, 110, 125, 125, 130];
  const MINIMUMS = [36, 120, 70, 90, 90, 70, 90, 90, 90];

  function normalizeWidths(widths) {
    return widths.map((width, i) => Math.max(MINIMUMS[i], Math.round(width)));
  }

  function totalWidth(widths) {
    return widths.reduce((sum, width) => sum + width, 0);
  }

  function readWidths(count) {
    try {
      const saved = JSON.parse(localStorage.getItem(STORAGE_KEY) || 'null');
      if (Array.isArray(saved) && saved.length === count && saved.every(Number.isFinite)) {
        return normalizeWidths(saved);
      }
    } catch (_) { /* invalid saved layout: use defaults */ }
    return DEFAULTS.slice(0, count);
  }

  function setupQueueColumnResize(table, container) {
    if (!table || !container) return;
    const cols = [...table.querySelectorAll('colgroup col')];
    const headers = [...table.querySelectorAll('thead th')];
    if (!cols.length || cols.length !== headers.length) return;
    const widths = readWidths(cols.length);

    const apply = () => {
      widths.forEach((width, i) => { cols[i].style.width = `${width}px`; });
      table.style.width = `${totalWidth(widths)}px`;
      try { localStorage.setItem(STORAGE_KEY, JSON.stringify(widths)); } catch (_) { /* optional */ }
    };
    apply();

    headers.forEach((header, index) => {
      if (index === 0) return; // selection column remains fixed
      const handle = document.createElement('div');
      handle.className = 'col-resize-handle';
      handle.setAttribute('role', 'separator');
      handle.setAttribute('aria-orientation', 'vertical');
      handle.setAttribute('aria-label', `Resize ${header.textContent.trim()} column`);
      header.appendChild(handle);
      handle.addEventListener('mousedown', (event) => {
        event.preventDefault();
        event.stopPropagation();
        const startX = event.clientX;
        const startWidth = widths[index];
        handle.classList.add('active');
        document.body.classList.add('dragging');
        const onMove = (moveEvent) => {
          widths[index] = Math.max(MINIMUMS[index], Math.round(startWidth + moveEvent.clientX - startX));
          cols[index].style.width = `${widths[index]}px`;
          table.style.width = `${totalWidth(widths)}px`;
        };
        const onUp = () => {
          handle.classList.remove('active');
          document.body.classList.remove('dragging');
          document.removeEventListener('mousemove', onMove);
          document.removeEventListener('mouseup', onUp);
          apply();
        };
        document.addEventListener('mousemove', onMove);
        document.addEventListener('mouseup', onUp);
      });
    });
  }

  globalThis.setupQueueColumnResize = setupQueueColumnResize;
  if (typeof module !== 'undefined') module.exports = { MINIMUMS, normalizeWidths, totalWidth };
})();

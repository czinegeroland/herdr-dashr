// Test-only (scripts/e2e/run.sh): the background colour the live log trail
// gives three known lines. dashr itself never reads page content.
(() => {
  const colour = (text) => {
    const cells = [...document.querySelectorAll('[role="gridcell"], td, div')]
      .filter((el) => el.children.length === 0 && (el.textContent || '').includes(text));
    for (let el of cells) {
      for (let depth = 0; el && depth < 6; depth++, el = el.parentElement) {
        const bg = getComputedStyle(el).backgroundColor;
        if (bg && bg !== 'rgba(0, 0, 0, 0)' && bg !== 'transparent') return bg;
      }
    }
    return cells.length ? 'none' : 'missing';
  };
  return JSON.stringify({expected: colour('E2E order 1 created'), forbidden: colour('E2E exception boom'), plain: colour('E2E plain line')});
})()

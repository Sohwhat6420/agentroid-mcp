// Runs before first paint: picks the theme from ?theme=, then a remembered choice, then the system.
(function () {
  try {
    var q = new URLSearchParams(location.search).get('theme');
    var saved = null;
    try { saved = localStorage.getItem('agentroid-jobview-theme'); } catch (e) { /* storage blocked */ }
    var t = q === 'dark' || q === 'light' ? q : saved === 'dark' || saved === 'light' ? saved : (matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light');
    document.documentElement.dataset.theme = t;
  } catch (e) { /* the CSS falls back to prefers-color-scheme */ }
})();

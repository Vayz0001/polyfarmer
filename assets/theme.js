/* Apply the saved colour theme before first paint (loaded synchronously in <head>
   so there is no flash). External rather than inline so the Content-Security-Policy
   can forbid inline scripts entirely. localStorage can throw in private windows. */
try {
  var t = localStorage.getItem("pf-theme");
  if (t) document.documentElement.dataset.theme = t;
} catch (e) {}

/* First-run launch screen: rotate reassuring messages while the engine starts, and
   switch to the failure state if the status poller reports an error. External (not
   inline) so the Content-Security-Policy can forbid inline scripts. */
(function () {
  var msgs = ["Authenticating with Polymarket…", "Clearing stale orders…", "Connecting to the order book…", "Almost there…"];
  var el = document.getElementById("launch-msg"), launch = document.getElementById("launch"), i = 0;
  if (!el || !launch) return;
  var t = setInterval(function () { i = Math.min(i + 1, msgs.length - 1); el.textContent = msgs[i]; }, 1800);
  document.body.addEventListener("htmx:afterSwap", function () {
    if (document.querySelector("[data-launch-failed]")) { clearInterval(t); launch.classList.add("failed"); }
  });
})();

/* Polyfarmer site: header state, settle-on-scroll fallback, story steps,
   the scoring explorer, card spotlights, tabs and copy buttons. No
   dependencies and no network. The page is complete without this script. */
(function () {
  "use strict";

  var reduce = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  var $$ = function (sel, root) { return Array.prototype.slice.call((root || document).querySelectorAll(sel)); };

  // ── header gets a backdrop once the page scrolls ─────────────────────
  var top = document.getElementById("top");
  if (top) {
    var onScroll = function () { top.classList.toggle("scrolled", window.scrollY > 12); };
    window.addEventListener("scroll", onScroll, { passive: true });
    onScroll();
  }

  // ── settle: browsers without CSS scroll timelines get the same slide ─
  var hasTimeline = window.CSS && CSS.supports && CSS.supports("animation-timeline: view()");
  if (!reduce && !hasTimeline && "IntersectionObserver" in window) {
    var sio = new IntersectionObserver(function (entries) {
      entries.forEach(function (e) {
        if (!e.isIntersecting) return;
        e.target.classList.add("js-on"); e.target.classList.remove("js-pending"); sio.unobserve(e.target);
      });
    }, { rootMargin: "0px 0px -6% 0px" });
    $$(".settle").forEach(function (el) {
      if (el.getBoundingClientRect().top > window.innerHeight) { el.classList.add("js-pending"); sio.observe(el); }
    });
  }

  // ── story: the step in the middle of the screen drives the window ────
  var steps = $$(".story .step");
  if (steps.length && "IntersectionObserver" in window) {
    var current = null;
    var stepIo = new IntersectionObserver(function (entries) {
      entries.forEach(function (e) {
        if (!e.isIntersecting || e.target === current) return;
        current = e.target;
        steps.forEach(function (s) { s.classList.toggle("on", s === current); });
        document.dispatchEvent(new CustomEvent("pf:step", { detail: current.getAttribute("data-step") }));
      });
    }, { rootMargin: "-45% 0px -45% 0px" });
    steps.forEach(function (s) { stepIo.observe(s); });
  }

  // ── scoring explorer ─────────────────────────────────────────────────
  // Same geometry as the drawn curves: midpoint x=280, 70px per cent,
  // weight 1 at y=40 and 0 at y=210, zone ±3¢.
  var range = document.getElementById("sc-dist");
  if (range) {
    var one = document.getElementById("sc-one");
    var dot = document.getElementById("sc-bid"), drop = document.getElementById("sc-drop");
    var dot2 = document.getElementById("sc-bid2"), drop2 = document.getElementById("sc-drop2");
    var sides = document.getElementById("sc-sides");
    var outDist = document.getElementById("sc-dist-out"), outScore = document.getElementById("sc-score");
    var outScoreNote = document.getElementById("sc-score-note");
    var outRisk = document.getElementById("sc-risk"), outRiskNote = document.getElementById("sc-risk-note");
    var ZONE = 3;
    var update = function () {
      var d = parseFloat(range.value);
      var w = d >= ZONE ? 0 : Math.pow((ZONE - d) / ZONE, 2);
      if (one.checked) w /= 3;
      // Two-sided: both legs on the full curve, mirrored about the midpoint.
      // One side: a single dot on the dashed one-third curve.
      var base = d >= ZONE ? 0 : Math.pow((ZONE - d) / ZONE, 2);
      var y = 210 - 170 * (one.checked ? base / 3 : base);
      var x = 280 - d * 70, x2 = 280 + d * 70;
      dot.style.transform = "translate(" + x.toFixed(1) + "px, " + y.toFixed(1) + "px)";
      drop.setAttribute("y2", (210 - y).toFixed(1));
      dot2.style.transform = "translate(" + x2.toFixed(1) + "px, " + y.toFixed(1) + "px)";
      drop2.setAttribute("y2", (210 - y).toFixed(1));
      dot2.classList.toggle("off", one.checked);
      sides.textContent = one.checked
        ? "One side: only the Yes bid. Polymarket pays a third of the score, and nothing when the midpoint is below 10¢ or above 90¢."
        : "Both sides: a bid on Yes and one on No, each the same distance from the midpoint. The weaker side sets the score.";
      outDist.textContent = d.toFixed(1) + "¢";
      outScore.textContent = w > 0 && w < 0.005 ? "<1%" : Math.round(w * 100) + "%";
      outScoreNote.textContent = d >= ZONE ? "outside the zone: earns nothing" : one.checked ? "one side only: a third of the two-sided score" : "of the best possible weight";
      var risk = d < 0.6 ? ["High", "risk-high", "first in line when the price moves"] : d < 1.5 ? ["Medium", "risk-mid", "orders ahead usually fill first"] : ["Low", "risk-low", "plenty of book in front of you"];
      outRisk.textContent = risk[0]; outRisk.className = "v " + risk[1]; outRiskNote.textContent = risk[2];
      range.style.setProperty("--p", ((d - 0.2) / 3.8 * 100).toFixed(1) + "%");
      range.setAttribute("aria-valuetext", d.toFixed(1) + " cents, score " + Math.round(w * 100) + " percent, fill risk " + risk[0].toLowerCase());
    };
    range.addEventListener("input", update);
    one.addEventListener("change", update);
    update();
  }

  // ── spotlight that follows the pointer across a card ─────────────────
  if (!reduce && window.matchMedia("(hover: hover)").matches) {
    $$(".tile").forEach(function (tile) {
      tile.addEventListener("pointermove", function (e) {
        var r = tile.getBoundingClientRect();
        tile.style.setProperty("--mx", (e.clientX - r.left).toFixed(0) + "px");
        tile.style.setProperty("--my", (e.clientY - r.top).toFixed(0) + "px");
      }, { passive: true });
    });
  }

  // ── tabs (ARIA tablist: click, arrow keys, Home/End) ────────────────
  $$('[role="tablist"]').forEach(function (list) {
    var tabs = $$('[role="tab"]', list);
    function select(tab, focus) {
      tabs.forEach(function (t) {
        var on = t === tab;
        t.setAttribute("aria-selected", on ? "true" : "false");
        t.tabIndex = on ? 0 : -1;
        var pane = document.getElementById(t.getAttribute("aria-controls"));
        if (pane) pane.hidden = !on;
      });
      if (focus) tab.focus();
    }
    tabs.forEach(function (t, i) {
      t.addEventListener("click", function () { select(t, false); });
      t.addEventListener("keydown", function (e) {
        var next = null;
        if (e.key === "ArrowRight") next = tabs[(i + 1) % tabs.length];
        else if (e.key === "ArrowLeft") next = tabs[(i - 1 + tabs.length) % tabs.length];
        else if (e.key === "Home") next = tabs[0];
        else if (e.key === "End") next = tabs[tabs.length - 1];
        if (next) { e.preventDefault(); select(next, true); }
      });
    });
  });

  // ── copy buttons ─────────────────────────────────────────────────────
  $$("[data-copy]").forEach(function (btn) {
    var timer = null;
    btn.addEventListener("click", function () {
      var pre = document.getElementById(btn.getAttribute("data-copy"));
      if (!pre) return;
      function done(ok) {
        btn.textContent = ok ? "Copied" : "Select and copy";
        btn.classList.toggle("done", ok);
        clearTimeout(timer);
        timer = setTimeout(function () { btn.textContent = "Copy"; btn.classList.remove("done"); }, 1800);
      }
      function selectText() {
        var r = document.createRange(); r.selectNodeContents(pre);
        var s = window.getSelection(); s.removeAllRanges(); s.addRange(r);
      }
      if (navigator.clipboard && navigator.clipboard.writeText) {
        navigator.clipboard.writeText(pre.textContent).then(function () { done(true); }, function () { selectText(); done(false); });
      } else { selectText(); done(false); }
    });
  });
})();

/* The crop field: an order book's cumulative depth drawn as rows of stems.
   Short near the midpoint, taller further out; the reward zone glows. Stems
   sway slowly and lean away from the pointer. Canvas 2D, two batched strokes
   a frame, ~30 fps, only while visible; a still frame under reduced motion. */
(function () {
  "use strict";

  var canvases = Array.prototype.slice.call(document.querySelectorAll("canvas[data-field]"));
  if (!canvases.length || !window.HTMLCanvasElement) return;
  var reduceQuery = window.matchMedia("(prefers-reduced-motion: reduce)");
  var colorQuery = window.matchMedia("(prefers-color-scheme: light)");

  function readColors() {
    var cs = getComputedStyle(document.documentElement);
    return {
      stem: cs.getPropertyValue("--field").trim() || "rgba(185,193,204,.22)",
      zone: cs.getPropertyValue("--field-zone").trim() || "rgba(108,140,255,.6)",
      leaf: cs.getPropertyValue("--leaf").trim() || "#2ebd73"
    };
  }
  var colors = readColors();

  function Field(canvas) {
    this.c = canvas;
    this.ctx = canvas.getContext("2d");
    this.kind = canvas.getAttribute("data-field");
    this.n = 0; this.w = 0; this.h = 0; this.dpr = 1;
    this.base = null; this.phase = null; this.lean = null;
    // tx/ty: where the pointer is; px/py: an eased copy the stems follow.
    this.tx = -1e4; this.ty = -1e4; this.px = -1e4; this.py = -1e4;
    this.visible = false;
    this.resize();
  }

  Field.prototype.resize = function () {
    var r = this.c.getBoundingClientRect();
    var dpr = Math.min(window.devicePixelRatio || 1, 2);
    var w = Math.max(1, Math.round(r.width)), h = Math.max(1, Math.round(r.height));
    if (w === this.w && h === this.h && dpr === this.dpr) return;
    this.w = w; this.h = h; this.dpr = dpr;
    this.c.width = Math.round(w * dpr); this.c.height = Math.round(h * dpr);
    var gap = w < 700 ? 7 : 6;
    var n = Math.floor(w / gap) + 1;
    this.n = n; this.gap = w / (n - 1);
    // Arrays are allocated here only, never per frame.
    this.base = new Float32Array(n); this.phase = new Float32Array(n); this.lean = new Float32Array(n);
    var mid = this.kind === "final" ? 0.5 : 0.62;
    var seed = 7;
    for (var i = 0; i < n; i++) {
      seed = (seed * 9301 + 49297) % 233280;
      var noise = seed / 233280;
      var d = Math.abs(i / (n - 1) - mid);
      // cumulative depth: a valley at the midpoint that rises toward the edges
      var depth = 0.1 + 0.62 * (1 - Math.exp(-d / 0.17)) + 0.18 * noise;
      if (this.kind === "final") depth *= 0.8;
      this.base[i] = depth * h * 0.8;
      this.phase[i] = i * 0.21 + noise * 2;
    }
    this.zoneLo = (mid - 0.075) * (n - 1);
    this.zoneHi = (mid + 0.075) * (n - 1);
    this.draw(performance.now(), true);
  };

  var REACH = 130; // px around the pointer that stems respond to

  Field.prototype.draw = function (t, still, dt) {
    var ctx = this.ctx, n = this.n, h = this.h, dpr = this.dpr, gap = this.gap;
    // Frame-rate independent easing: the same feel at 30 or 60 fps.
    var kPointer = 1 - Math.exp(-(dt || 16) / 90), kLean = 1 - Math.exp(-(dt || 16) / 260);
    if (this.px < -5e3) { this.px = this.tx; this.py = this.ty; }
    this.px += (this.tx - this.px) * kPointer; this.py += (this.ty - this.py) * kPointer;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, this.w, h);
    var time = still ? 0 : t / 1000;
    var px = this.px, py = this.py;
    ctx.lineWidth = 1;
    for (var pass = 0; pass < 2; pass++) {
      ctx.beginPath();
      for (var i = 0; i < n; i++) {
        var inZone = i >= this.zoneLo && i <= this.zoneHi;
        if ((pass === 1) !== inZone) continue;
        var x = i * gap, len = this.base[i];
        var sway = still ? 0 : Math.sin(time * 0.55 + this.phase[i]) * 0.035 + Math.sin(time * 0.21 + i * 0.05) * 0.02;
        // Lean away from the pointer with a soft (1 − r²)² falloff, fading in
        // as the pointer comes down to the stem tops; eased so it settles back.
        var target = 0, dx = x - px;
        if (dx > -REACH && dx < REACH) {
          var q = dx / REACH, fall = (1 - q * q) * (1 - q * q);
          var top = h - len, vy = (py - (top - 70)) / 140;
          var near = vy <= 0 ? 0 : vy >= 1 ? 1 : vy * vy * (3 - 2 * vy);
          target = (dx >= 0 ? 1 : -1) * Math.min(1, Math.abs(q) * 2.2) * fall * near * 0.32;
        }
        this.lean[i] += (target - this.lean[i]) * kLean;
        var a = sway + this.lean[i];
        var tx = x + Math.sin(a) * len, ty = h - Math.cos(a) * len;
        ctx.moveTo(x, h);
        ctx.quadraticCurveTo(x, h - len * 0.55, tx, ty);
      }
      ctx.strokeStyle = pass === 1 ? colors.zone : colors.stem;
      ctx.stroke();
    }
    // a few leaves on top of the tallest zone stems
    ctx.fillStyle = colors.leaf;
    for (var k = Math.ceil(this.zoneLo); k <= this.zoneHi; k += 6) {
      var len2 = this.base[k], a2 = still ? 0 : Math.sin(time * 0.55 + this.phase[k]) * 0.035 + this.lean[k];
      ctx.beginPath();
      ctx.arc(k * gap + Math.sin(a2) * len2, h - Math.cos(a2) * len2, 1.6, 0, 6.283);
      ctx.fill();
    }
  };

  var fields = canvases.map(function (c) { return new Field(c); });
  var raf = 0, last = 0, lastMove = -1e9;

  // 60 fps for a moment after the pointer moves (smooth response), 30 fps idle.
  function frame(t) {
    raf = 0;
    var budget = t - lastMove < 1500 ? 15 : 33;
    if (t - last >= budget) {
      var dt = Math.min(64, t - last);
      last = t;
      for (var i = 0; i < fields.length; i++) if (fields[i].visible) fields[i].draw(t, false, dt);
    }
    schedule();
  }
  function schedule() {
    if (raf || reduceQuery.matches || document.hidden) return;
    for (var i = 0; i < fields.length; i++) if (fields[i].visible) { raf = requestAnimationFrame(frame); return; }
  }
  function stillAll() { fields.forEach(function (f) { f.draw(0, true); }); }

  if ("IntersectionObserver" in window) {
    var io = new IntersectionObserver(function (entries) {
      entries.forEach(function (e) {
        for (var i = 0; i < fields.length; i++) if (fields[i].c === e.target) fields[i].visible = e.isIntersecting && e.intersectionRatio > 0;
      });
      schedule();
    }, { threshold: [0, 0.01] });
    fields.forEach(function (f) { io.observe(f.c); });
  } else { fields.forEach(function (f) { f.visible = true; }); schedule(); }

  if ("ResizeObserver" in window) {
    var ro = new ResizeObserver(function () { fields.forEach(function (f) { f.resize(); }); });
    fields.forEach(function (f) { ro.observe(f.c); });
  }

  // Pointer, relative to each canvas; passive and cheap.
  window.addEventListener("pointermove", function (e) {
    for (var i = 0; i < fields.length; i++) {
      var f = fields[i]; if (!f.visible) continue;
      var r = f.c.getBoundingClientRect();
      f.tx = e.clientX - r.left; f.ty = e.clientY - r.top;
    }
    lastMove = performance.now();
  }, { passive: true });

  document.addEventListener("visibilitychange", schedule);
  function onPrefs() { colors = readColors(); if (reduceQuery.matches) { if (raf) cancelAnimationFrame(raf); raf = 0; stillAll(); } else schedule(); }
  reduceQuery.addEventListener ? reduceQuery.addEventListener("change", onPrefs) : reduceQuery.addListener(onPrefs);
  colorQuery.addEventListener ? colorQuery.addEventListener("change", onPrefs) : colorQuery.addListener(onPrefs);

  if (reduceQuery.matches) stillAll(); else schedule();
})();

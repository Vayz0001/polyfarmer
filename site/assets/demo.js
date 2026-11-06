/* Simulated dashboard for the site. Made-up markets and numbers: a seeded
   random walk, no network, nothing traded. Rows are built once and only
   their text changes; each window's clock runs only while it is on screen.
   When the best bid moves, your order is cancelled and placed again at the
   same distance: the old one is crossed out and the outline slides to the
   new price (transform and opacity animations; nothing is added per tick). */
(function () {
  "use strict";

  var reduce = window.matchMedia("(prefers-reduced-motion: reduce)").matches;

  function el(tag, cls, text) {
    var e = document.createElement(tag);
    if (cls) e.className = cls;
    if (text != null) e.textContent = text;
    return e;
  }
  // Book levels are whole cents; distances and moves are too.
  function c1(v) { return (Math.abs(v - Math.round(v)) < 1e-9 ? String(Math.round(v)) : v.toFixed(1)) + "¢"; }
  // Midpoints can land on a quarter cent: show it rather than round it away.
  function cm(v) { return (Math.abs(v - Math.round(v)) < 1e-9 ? String(Math.round(v)) : Math.abs(v * 10 - Math.round(v * 10)) < 1e-9 ? v.toFixed(1) : v.toFixed(2)) + "¢"; }
  function int(v) { return Math.round(v).toLocaleString("en-US"); }
  function usd(v) { return "$" + v.toFixed(2); }
  // Polymarket's scoring: ((v − s) / v)² inside the zone, 0 outside.
  function weight(s, zone) { return s >= zone ? 0 : Math.pow((zone - s) / zone, 2); }

  function rng(seed) {
    return function () {
      seed |= 0; seed = (seed + 0x6d2b79f5) | 0;
      var t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
      t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
      return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
    };
  }

  var shared = { today: 4.18 };

  function BookWidget(root, opts) {
    var self = this;
    this.root = root;
    this.o = opts;
    this.rnd = rng(opts.seed);
    this.mode = opts.mode || "follow";
    this.bestBid = opts.bestBid; this.spread = 1;
    this.start = opts.bestBid; this.lastBest = opts.bestBid; this.lastW = 0;
    this.thin = 0; this.ticks = 0;
    this.ask = []; this.bid = []; this.view = [];
    // Sizes never fall below the floor, so outside the "guard" scene the
    // level in front of your order always holds more than your minimum.
    var floor = opts.floor || 600;
    for (var i = 0; i < opts.asks; i++) this.ask.push(floor + this.rnd() * 2600);
    for (var j = 0; j < opts.bids; j++) { this.bid.push(floor + this.rnd() * 2600); this.view.push(0); }
    function rows(box, n, side) {
      var list = [];
      for (var k = 0; k < n; k++) {
        var r = el("div", "r " + side), bar = el("i", "depth"), p = el("span"), s = el("span"), u = el("span");
        r.appendChild(bar); r.appendChild(p); r.appendChild(s); r.appendChild(u); box.appendChild(r);
        list.push({ r: r, bar: bar, p: p, s: s, u: u });
      }
      return list;
    }
    this.askRows = rows(this.out("asks"), opts.asks, "ask");
    var bids = this.out("bids");
    this.bidRows = rows(bids, opts.bids, "bidr");
    // Overlays on the bid ladder: your order's outline, the crossed-out copy
    // left behind when it is cancelled, and the bracket from the best bid.
    this.ghost = el("div", "peg ghost"); this.strike = el("i", "strike"); this.ghost.appendChild(this.strike);
    this.peg = el("div", "peg off"); this.glow = el("i", "glow"); this.peg.appendChild(this.glow);
    this.gap = el("div", "gap off"); this.gapLabel = el("span", null, c1(opts.dist)); this.gap.appendChild(this.gapLabel);
    [this.ghost, this.peg, this.gap].forEach(function (n) { n.setAttribute("aria-hidden", "true"); bids.appendChild(n); });
    this.pitch = this.bidRows[0].r.offsetHeight || 25;
    this.wasLive = false; this.later = null;
    this.timer = null;
    this.onScreen = false;
    this.render();
    if (!reduce && "IntersectionObserver" in window) {
      new IntersectionObserver(function (e) { self.onScreen = e[0].isIntersecting && e[0].intersectionRatio > 0; self.sync(); }, { threshold: [0, 0.01] }).observe(root);
      document.addEventListener("visibilitychange", function () { self.sync(); });
    }
  }

  BookWidget.prototype.out = function (name) { return this.root.querySelector('[data-out="' + name + '"]'); };
  BookWidget.prototype.set = function (name, text) { var n = this.out(name); if (n && n.textContent !== text) n.textContent = text; };

  BookWidget.prototype.sync = function () {
    var want = this.onScreen && !document.hidden, self = this;
    if (want && this.timer === null) this.timer = setInterval(function () { self.step(); }, this.o.interval);
    if (!want && this.timer !== null) { clearInterval(this.timer); this.timer = null; }
    if (!want && this.later !== null) { clearTimeout(this.later); this.later = null; this.gapLabel.textContent = c1(this.o.dist); this.gap.classList.remove("stretched"); }
  };

  BookWidget.prototype.setMode = function (mode) {
    if (mode === this.mode) return;
    this.mode = mode; this.ticks = 0; this.thin = 0;
    this.render();
  };

  BookWidget.prototype.step = function () {
    var r = this.rnd(), STEP = this.o.step;
    this.ticks++;
    if (this.mode !== "place" || this.ticks > 2) {
      if (r < 0.2) this.bestBid = Math.min(this.o.max, this.bestBid + STEP);
      else if (r < 0.4) this.bestBid = Math.max(this.o.min, this.bestBid - STEP);
    }
    this.spread = this.rnd() < 0.7 ? 1 : 2;
    for (var k = 0; k < this.ask.length; k++) this.ask[k] = Math.max(this.o.floor || 150, this.ask[k] * (0.9 + this.rnd() * 0.2));
    for (var m = 0; m < this.bid.length; m++) this.bid[m] = Math.max(this.o.floor || 150, this.bid[m] * (0.9 + this.rnd() * 0.2));
    // "guard": the orders in front of yours thin out for a few ticks, then recover
    if (this.mode === "guard") this.thin = (this.ticks % 9) < 5 ? Math.min(1, this.thin + 0.5) : Math.max(0, this.thin - 0.5);
    if (this.o.main) shared.today += 0.0024 * this.lastW / 0.2 * (0.7 + this.rnd() * 0.6);
    this.render();
  };

  BookWidget.prototype.render = function () {
    var o = this.o, STEP = o.step, DIST = o.dist, k;
    var mid = this.bestBid + this.spread / 2, you = this.bestBid - DIST;
    var aheadRows = Math.round(DIST / STEP);
    var placed = this.mode !== "place" || this.ticks >= 2;
    var ahead = 0;
    for (k = 0; k < this.bid.length; k++) {
      this.view[k] = k < aheadRows ? this.bid[k] * (1 - 0.92 * this.thin) : this.bid[k];
      if (k < aheadRows) ahead += this.view[k] * (this.bestBid - k * STEP) / 100;
    }
    var pulled = placed && ahead < o.minDepth;
    var live = placed && !pulled;
    // Like a real book: shares per level, and a running total in dollars
    // that adds up each level moving away from the spread. The depth bars
    // follow the running total, scaled to the deeper side.
    var askCum = [], bidCum = [], sum = 0;
    for (k = 0; k < this.ask.length; k++) { sum += this.ask[k] * (this.bestBid + this.spread + k * STEP) / 100; askCum.push(sum); }
    sum = 0;
    for (k = 0; k < this.view.length; k++) { sum += (this.view[k] + (live && k === aheadRows ? o.mine : 0)) * (this.bestBid - k * STEP) / 100; bidCum.push(sum); }
    var max = Math.max(askCum[askCum.length - 1], bidCum[bidCum.length - 1]);

    for (k = 0; k < this.askRows.length; k++) {
      var ai = this.askRows.length - 1 - k, ap = this.bestBid + this.spread + ai * STEP, as = this.ask[ai], a = this.askRows[k];
      a.p.textContent = c1(ap); a.s.textContent = int(as); a.u.textContent = "$" + int(askCum[ai]);
      a.bar.style.width = (askCum[ai] / max * 100).toFixed(1) + "%";
      a.r.classList.toggle("inzone", Math.abs(ap - mid) < o.zone);
    }
    for (k = 0; k < this.bidRows.length; k++) {
      var bp = this.bestBid - k * STEP, mine = live && k === aheadRows;
      var bs = this.view[k] + (mine ? o.mine : 0), b = this.bidRows[k];
      b.p.textContent = c1(bp); b.s.textContent = int(bs); b.u.textContent = "$" + int(bidCum[k]);
      b.bar.style.width = (bidCum[k] / max * 100).toFixed(1) + "%";
      b.r.classList.toggle("inzone", Math.abs(mid - bp) < o.zone);
    }
    this.repeg(placed, pulled, live, aheadRows);
    if (this.bestBid !== this.lastBest && !reduce) {
      // Alternate between two identical animations: changing the animation
      // name restarts it without forcing a layout (no offsetWidth trick).
      var top = this.bidRows[0].r, dir = this.bestBid > this.lastBest ? "up" : "down";
      this.flip = !this.flip;
      top.classList.remove("flash-up", "flash-down", "flash-up-b", "flash-down-b");
      top.classList.add("flash-" + dir + (this.flip ? "-b" : ""));
    }
    var s = mid - you, w = pulled || !placed ? 0 : weight(s, o.zone);
    this.set("mid", cm(mid));
    this.set("spread", "spread " + c1(this.spread));
    this.set("zone", "zone " + cm(mid - o.zone).slice(0, -1) + "–" + cm(mid + o.zone));
    if (o.main) {
      this.set("weight", Math.round(w * 100) + "%");
      var bar = this.out("weight-bar"); if (bar) bar.style.width = (w * 100).toFixed(1) + "%";
      this.set("share", (3.9 * w / 0.2).toFixed(1) + "%");
      this.set("today", usd(shared.today));
      var bt = document.querySelector('[data-out="bento-today"]'); if (bt && bt.textContent !== usd(shared.today)) bt.textContent = usd(shared.today);
      this.set("depth", "$" + int(ahead));
      this.set("move", c1(Math.abs(this.bestBid - this.start)));
      this.set("weight-note", this.bestBid !== this.lastBest && live
        ? "Best bid " + c1(this.bestBid) + ": cancelled " + c1(this.lastBest - DIST) + ", placed " + c1(you) + "."
        : "Inside the zone, " + cm(s) + " from the midpoint.");
    } else {
      this.note(placed, pulled, ahead, you, w);
    }
    this.lastBest = this.bestBid; this.lastW = w;
  };

  // Your order: the outline sits on its row, the bracket shows the distance
  // to the best bid. When the best bid moves the ladder re-centres at once,
  // so the old order is now one row away. It stays there for a beat (the
  // bracket shows the wrong distance), then it is crossed out as the outline
  // slides back to the same distance, and the new order lands with a glow.
  BookWidget.prototype.repeg = function (placed, pulled, live, at) {
    var P = this.pitch, peg = this.peg, gap = this.gap, to = "translateY(" + at * P + "px)";
    peg.classList.toggle("off", !placed);
    peg.classList.toggle("pulled", placed && pulled);
    gap.classList.toggle("off", !live);
    peg.style.transform = to;
    gap.style.top = P / 2 + "px";
    gap.style.height = at * P + "px";
    var moved = live && this.wasLive && this.bestBid !== this.lastBest;
    var appeared = live && !this.wasLive;
    this.wasLive = live;
    if (reduce || typeof peg.animate !== "function") return;
    if (this.later !== null) { clearTimeout(this.later); this.later = null; this.gapLabel.textContent = c1(this.o.dist); gap.classList.remove("stretched"); }
    var ease = "cubic-bezier(.16, 1, .3, 1)", glow = [{ opacity: 0, transform: "scale(.985)" }, { opacity: 1, transform: "scale(1)", offset: 0.3 }, { opacity: 0, transform: "scale(1.01)" }];
    if (appeared) { this.glow.animate(glow, { duration: 700, easing: "ease-out" }); return; }
    if (!moved) return;
    var from = at + Math.round((this.bestBid - this.lastBest) / this.o.step);
    if (from < 0 || from >= this.bidRows.length) return;
    var self = this, old = "translateY(" + from * P + "px)", HOLD = 260, SLIDE = 460;
    // the market moved: the order is now the wrong distance away
    this.gapLabel.textContent = c1(Math.abs(this.bestBid - (this.lastBest - this.o.dist)));
    gap.classList.add("stretched");
    // cancel: crossed out where it sits, while the outline leaves
    this.ghost.style.transform = old;
    this.ghost.animate([{ opacity: 1 }, { opacity: 1, offset: 0.6 }, { opacity: 0 }], { duration: 900, delay: HOLD });
    this.strike.animate([{ transform: "scaleX(0)" }, { transform: "scaleX(1)" }], { duration: 280, delay: HOLD, easing: ease, fill: "backwards" });
    // re-place: the outline and the bracket return to the same distance
    peg.animate([{ transform: old }, { transform: to }], { duration: SLIDE, delay: HOLD, easing: ease, fill: "backwards" });
    gap.animate([{ height: from * P + "px" }, { height: at * P + "px" }], { duration: SLIDE, delay: HOLD, easing: ease, fill: "backwards" });
    this.glow.animate(glow, { duration: 700, delay: HOLD + SLIDE - 120, easing: "ease-out", fill: "backwards" });
    this.later = setTimeout(function () { self.later = null; self.gapLabel.textContent = c1(self.o.dist); gap.classList.remove("stretched"); }, HOLD + SLIDE / 2);
  };

  BookWidget.prototype.note = function (placed, pulled, ahead, you, w) {
    var box = this.out("note"), txt, cls = "";
    if (this.mode === "place") {
      txt = placed ? "Placed $150 at " + c1(you) + ", " + c1(this.o.dist) + " below the best bid. Score " + Math.round(w * 100) + "%."
                   : "Previewing " + c1(this.o.dist) + " below the best bid: about " + Math.round(weight(this.spread / 2 + this.o.dist, this.o.zone) * 100) + "% weight.";
    } else if (this.mode === "follow") {
      txt = this.bestBid !== this.lastBest ? "Best bid " + c1(this.bestBid) + ": cancelled " + c1(this.lastBest - this.o.dist) + ", placed " + c1(you) + "."
                                           : "Resting at " + c1(you) + ", " + c1(this.o.dist) + " below the best bid.";
    } else if (this.mode === "guard") {
      txt = pulled ? "Only $" + int(ahead) + " ahead, under your $500 minimum. Order pulled." : "$" + int(ahead) + " ahead of you. Order resting.";
      cls = pulled ? "warn" : "";
    } else {
      txt = "Earning " + Math.round(w * 100) + "% weight. Paid after 00:00 UTC."; cls = "good";
    }
    this.set("note-text", txt);
    if (box) { var want = "story-note" + (cls ? " " + cls : ""); if (box.className !== want) box.className = want; }
  };

  var heroRoot = document.querySelector('[data-book="hero"]');
  var storyRoot = document.querySelector('[data-book="story"]');
  if (heroRoot) new BookWidget(heroRoot, { seed: 0x5eed1e5, bestBid: 48, asks: 6, bids: 8, step: 1, dist: 2, zone: 4.5, mine: 309, min: 44, max: 53, minDepth: 500, floor: 1200, interval: 1400, main: true });
  var story = storyRoot ? new BookWidget(storyRoot, { seed: 0xbadcafe, bestBid: 21, asks: 5, bids: 7, step: 1, dist: 2, zone: 4.5, mine: 735, min: 18, max: 24, floor: 2600, minDepth: 500, interval: 1300, mode: "place", main: false }) : null;

  // The story steps tell the pinned window which scene to play.
  document.addEventListener("pf:step", function (e) { if (story) story.setMode(e.detail); });

  // 30 days of payouts for the bento tile (static).
  var bars = document.querySelector('[data-out="payouts"]');
  if (bars) {
    var r = rng(42);
    for (var d = 0; d < 30; d++) {
      var h = 28 + 50 * (0.5 + 0.5 * Math.sin(d * 0.6)) * (0.6 + r() * 0.4) + d * 0.9;
      var b = el("i"); b.style.height = Math.min(100, h).toFixed(0) + "%"; bars.appendChild(b);
    }
  }
})();

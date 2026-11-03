/* Simulated dashboard for the site. Made-up markets and numbers: a seeded
   random walk, no network, nothing traded. Rows are built once and only
   their text changes; each window's clock runs only while it is on screen. */
(function () {
  "use strict";

  var reduce = window.matchMedia("(prefers-reduced-motion: reduce)").matches;

  function el(tag, cls, text) {
    var e = document.createElement(tag);
    if (cls) e.className = cls;
    if (text != null) e.textContent = text;
    return e;
  }
  function c1(v) { return v.toFixed(1) + "¢"; }
  // Midpoints can land on a quarter cent: show it rather than round it away.
  function cm(v) { return (Math.abs(v * 10 - Math.round(v * 10)) < 1e-9 ? v.toFixed(1) : v.toFixed(2)) + "¢"; }
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
    this.bestBid = opts.bestBid; this.spread = 1.0;
    this.start = opts.bestBid; this.lastBest = opts.bestBid; this.lastW = 0;
    this.thin = 0; this.ticks = 0;
    this.ask = []; this.bid = []; this.view = [];
    for (var i = 0; i < opts.asks; i++) this.ask.push(600 + this.rnd() * 2600);
    for (var j = 0; j < opts.bids; j++) { this.bid.push(600 + this.rnd() * 2600); this.view.push(0); }
    function rows(box, n, side) {
      var list = [];
      for (var k = 0; k < n; k++) {
        var r = el("div", "r " + side), bar = el("i", "bar"), p = el("span"), s = el("span"), u = el("span");
        r.appendChild(bar); r.appendChild(p); r.appendChild(s); r.appendChild(u); box.appendChild(r);
        list.push({ r: r, bar: bar, p: p, s: s, u: u });
      }
      return list;
    }
    this.askRows = rows(this.out("asks"), opts.asks, "ask");
    this.bidRows = rows(this.out("bids"), opts.bids, "bidr");
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
    this.spread = this.rnd() < 0.5 ? 0.5 : 1.0;
    for (var k = 0; k < this.ask.length; k++) this.ask[k] = Math.max(150, this.ask[k] * (0.9 + this.rnd() * 0.2));
    for (var m = 0; m < this.bid.length; m++) this.bid[m] = Math.max(150, this.bid[m] * (0.9 + this.rnd() * 0.2));
    // "guard": the orders in front of yours thin out for a few ticks, then recover
    if (this.mode === "guard") this.thin = (this.ticks % 9) < 5 ? Math.min(1, this.thin + 0.5) : Math.max(0, this.thin - 0.5);
    if (this.o.main) shared.today += 0.0024 * this.lastW / 0.34 * (0.7 + this.rnd() * 0.6);
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
    var max = 0;
    for (k = 0; k < this.ask.length; k++) max = Math.max(max, this.ask[k]);
    for (k = 0; k < this.view.length; k++) max = Math.max(max, this.view[k] + o.mine);

    for (k = 0; k < this.askRows.length; k++) {
      var ai = this.askRows.length - 1 - k, ap = this.bestBid + this.spread + ai * STEP, as = this.ask[ai], a = this.askRows[k];
      a.p.textContent = c1(ap); a.s.textContent = int(as); a.u.textContent = "$" + int(as * ap / 100);
      a.bar.style.width = (as / max * 100).toFixed(1) + "%";
      a.r.classList.toggle("inzone", Math.abs(ap - mid) < o.zone);
    }
    for (k = 0; k < this.bidRows.length; k++) {
      var bp = this.bestBid - k * STEP, mine = placed && Math.abs(bp - you) < 1e-9;
      var bs = this.view[k] + (mine && !pulled ? o.mine : 0), b = this.bidRows[k];
      b.p.textContent = c1(bp); b.s.textContent = int(bs); b.u.textContent = "$" + int(bs * bp / 100);
      b.bar.style.width = (bs / max * 100).toFixed(1) + "%";
      b.r.classList.toggle("inzone", Math.abs(mid - bp) < o.zone);
      b.r.classList.toggle("you", mine);
      b.r.classList.toggle("pulled", mine && pulled);
    }
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
      this.set("share", (3.9 * w / 0.34).toFixed(1) + "%");
      this.set("today", usd(shared.today));
      var bt = document.querySelector('[data-out="bento-today"]'); if (bt && bt.textContent !== usd(shared.today)) bt.textContent = usd(shared.today);
      this.set("depth", "$" + int(ahead));
      this.set("move", c1(Math.abs(this.bestBid - this.start)));
      this.set("weight-note", this.bestBid !== this.lastBest
        ? "Best bid moved to " + c1(this.bestBid) + ". Re-pegged to " + c1(you) + "."
        : "Inside the zone, " + cm(s) + " from the midpoint.");
    } else {
      this.note(placed, pulled, ahead, you, w);
    }
    this.lastBest = this.bestBid; this.lastW = w;
  };

  BookWidget.prototype.note = function (placed, pulled, ahead, you, w) {
    var box = this.out("note"), txt, cls = "";
    if (this.mode === "place") {
      txt = placed ? "Placed $150 at " + c1(you) + ", 1.0¢ below the best bid. Score " + Math.round(w * 100) + "%."
                   : "Previewing 1.0¢ below the best bid: about " + Math.round(weight(this.spread / 2 + 1, this.o.zone) * 100) + "% weight.";
    } else if (this.mode === "follow") {
      txt = this.bestBid !== this.lastBest ? "Best bid moved to " + c1(this.bestBid) + ". Re-pegged to " + c1(you) + "."
                                           : "Resting at " + c1(you) + ", inside the reward zone.";
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
  if (heroRoot) new BookWidget(heroRoot, { seed: 0x5eed1e5, bestBid: 48.0, asks: 6, bids: 8, step: 0.5, dist: 1.0, zone: 3, mine: 309, min: 45, max: 52, minDepth: 500, interval: 1400, main: true });
  var story = storyRoot ? new BookWidget(storyRoot, { seed: 0xbadcafe, bestBid: 21.5, asks: 5, bids: 7, step: 0.5, dist: 1.0, zone: 3.5, mine: 735, min: 19, max: 24, minDepth: 500, interval: 1300, mode: "place", main: false }) : null;

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

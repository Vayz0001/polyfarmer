/* polyfarmer dashboard — one script for every page (loaded deferred, after htmx).
   Sections: prefs · util · live events (SSE) + toasts · alerts window · confirm
   modal · drawer · activity filters · settings · market view · action router. */
(function () {
  "use strict";
  var PF = (window.PF = window.PF || {});

  // ── prefs (localStorage can throw in private mode — never let it break UI) ─
  function pref(k, d) { try { return localStorage.getItem(k) || d; } catch (e) { return d; } }
  function setPref(k, v) { try { localStorage.setItem(k, v); } catch (e) {} }

  // ── util ────────────────────────────────────────────────────────────────
  function $(sel, root) { return (root || document).querySelector(sel); }
  function $$(sel, root) { return Array.prototype.slice.call((root || document).querySelectorAll(sel)); }
  function esc(s) { var d = document.createElement("div"); d.textContent = s == null ? "" : String(s); return d.innerHTML; }
  function num(v) { var n = parseFloat(v); return isFinite(n) ? n : NaN; }
  function relTime(iso) {
    var t = new Date(iso).getTime(); if (isNaN(t)) return "";
    var s = Math.max(0, (Date.now() - t) / 1000);
    if (s < 45) return "just now";
    if (s < 3600) return Math.floor(s / 60) + "m ago";
    if (s < 86400) return Math.floor(s / 3600) + "h ago";
    if (s < 86400 * 7) return Math.floor(s / 86400) + "d ago";
    return new Date(t).toLocaleDateString(undefined, { month: "short", day: "numeric" });
  }
  PF.relTime = function (root) {
    $$(".feed-time[data-ts]", root).forEach(function (el) {
      var iso = el.getAttribute("data-ts");
      el.textContent = relTime(iso);
      el.title = new Date(iso).toLocaleString();
    });
  };

  // ── theme ───────────────────────────────────────────────────────────────
  function theme() { return document.documentElement.dataset.theme || "dark"; }
  function setTheme(t) { document.documentElement.dataset.theme = t; setPref("pf-theme", t); syncPrefSegs(); }
  function syncPrefSegs() {
    $$("#pref-theme button").forEach(function (b) { b.classList.toggle("selected", b.dataset.value === theme()); });
    var tm = pref("pf-toasts", "important");
    $$("#pref-toasts button").forEach(function (b) { b.classList.toggle("selected", b.dataset.value === tm); });
  }

  // ── toasts ──────────────────────────────────────────────────────────────
  function toast(title, body, tone, ttl) {
    var wrap = $("#pf-toasts"); if (!wrap) return;
    var el = document.createElement("div");
    el.className = "toast tone-" + (tone || "pos");
    el.innerHTML = '<span class="bar"></span><div><div class="t">' + esc(title) + "</div>" +
      (body ? '<div class="b">' + esc(body) + "</div>" : "") + "</div>" +
      '<button class="btn ghost icon x" aria-label="Dismiss"><svg class="i sm"><use href="#i-x"/></svg></button>';
    function dismiss() { el.classList.remove("show"); setTimeout(function () { el.remove(); }, 220); }
    el.onclick = dismiss;
    wrap.appendChild(el);
    while (wrap.children.length > 4) wrap.firstChild.remove();
    requestAnimationFrame(function () { el.classList.add("show"); });
    setTimeout(dismiss, ttl || 6000);
  }
  PF.toast = toast;

  function isFill(a) { return /\bfill/i.test(a.message || ""); }
  function shouldToast(a) {
    var mode = pref("pf-toasts", "important");
    if (mode === "none") return false;
    if (mode === "all") return true;
    return a.level !== "info" || isFill(a);
  }

  // ── live feed rows ──────────────────────────────────────────────────────
  function splitMsg(m) { var i = (m || "").indexOf("\n"); return i < 0 ? [m || "", ""] : [m.slice(0, i), m.slice(i + 1)]; }
  function feedRow(a, compact) {
    var parts = splitMsg(a.message);
    var row = document.createElement("div");
    row.className = "feed-row " + a.level + " tone-" + (a.tone || "pos") + " pf-rise";
    row.dataset.cat = a.category; row.dataset.level = a.level;
    row.innerHTML = '<span class="dot"></span>' +
      (compact ? "" : '<span class="feed-cat tag">' + esc(a.category) + "</span>") +
      '<div><div class="feed-title">' + esc(parts[0]) + "</div>" +
      (parts[1] ? '<div class="feed-body">' + esc(parts[1]) + "</div>" : "") + "</div>" +
      '<span class="feed-time" data-ts="' + esc(a.ts) + '"></span>';
    return row;
  }
  function prependFeeds(a) {
    $$(".pf-feed").forEach(function (feed) {
      var empty = $(".feed-empty", feed); if (empty) empty.remove();
      var compact = feed.classList.contains("compact");
      var row = feedRow(a, compact);
      if (compact && !$(".feed-day", feed)) {
        feed.insertBefore(row, feed.firstChild);
      } else {
        var first = feed.firstElementChild;
        if (!first || !first.classList.contains("feed-day") || first.dataset.day !== "Today") {
          var d = document.createElement("div");
          d.className = "feed-day"; d.dataset.day = "Today"; d.textContent = "Today";
          feed.insertBefore(d, feed.firstChild);
        }
        $('.feed-day[data-day="Today"]', feed).insertAdjacentElement("afterend", row);
      }
      var limit = parseInt(feed.dataset.limit || "0", 10);
      if (limit) $$(".feed-row", feed).slice(limit).forEach(function (r) { r.remove(); });
    });
    applyActivityFilter();
    PF.relTime();
  }

  // ── live events (one SSE stream: alerts + state changes) ───────────────
  var stateTimer = null;
  function connectEvents() {
    if (!window.EventSource || !$("#status-strip")) return;
    var es = new EventSource("/events");
    es.addEventListener("alert", function (e) {
      var a; try { a = JSON.parse(e.data); } catch (_) { return; }
      var parts = splitMsg(a.message);
      if (shouldToast(a)) toast(parts[0], parts[1], a.tone, a.level === "error" ? 12000 : 6000);
      prependFeeds(a);
      bumpBell(a);
    });
    es.addEventListener("state", function () {
      // Coalesce bursts (a replace = cancel + place) into one refresh.
      clearTimeout(stateTimer);
      stateTimer = setTimeout(function () { if (window.htmx) htmx.trigger(document.body, "pf:state"); }, 250);
    });
  }

  // ── alerts window (draggable, resizable, remembers position) ───────────
  var unread = 0, winLoaded = false;
  function bumpBell(a) {
    var win = $("#pf-win"), badge = $("#pf-bell-count");
    if (!badge || (win && !win.hidden)) return;
    if (a.level === "info" && !isFill(a) && a.tone !== "caution") return;
    unread++; badge.textContent = unread > 99 ? "99+" : unread; badge.hidden = false;
  }
  function initWindow() {
    var win = $("#pf-win"), bell = $("#pf-bell"), bar = $("#pf-win-bar");
    if (!win || !bell) return;
    function save() { setPref("pf-win", JSON.stringify({ open: !win.hidden, left: win.style.left, top: win.style.top, w: win.style.width, h: win.style.height })); }
    function clamp() {
      var r = win.getBoundingClientRect();
      if (r.left > innerWidth - 80) win.style.left = innerWidth - 80 + "px";
      if (r.top > innerHeight - 40) win.style.top = innerHeight - 40 + "px";
      if (r.left < 0) win.style.left = "8px";
      if (r.top < 0) win.style.top = "8px";
    }
    function open() {
      win.hidden = false;
      if (!win.style.left) { win.style.left = Math.max(8, innerWidth - win.offsetWidth - 20) + "px"; win.style.top = "52px"; }
      clamp();
      if (!winLoaded) {
        winLoaded = true;
        fetch("/activity/recent").then(function (r) { return r.text(); }).then(function (html) {
          $("#pf-win-feed").innerHTML = html; PF.relTime($("#pf-win-feed"));
        }).catch(function () { winLoaded = false; });
      }
      unread = 0; $("#pf-bell-count").hidden = true; bell.classList.add("active"); save();
    }
    function close() { win.hidden = true; bell.classList.remove("active"); save(); }
    bell.addEventListener("click", function () { win.hidden ? open() : close(); });
    $("#pf-win-x").addEventListener("click", close);
    try {
      var s = JSON.parse(pref("pf-win", "{}"));
      if (s.left) win.style.left = s.left; if (s.top) win.style.top = s.top;
      if (s.w) win.style.width = s.w; if (s.h) win.style.height = s.h;
      if (s.open) open();
    } catch (_) {}
    var drag = false, ox = 0, oy = 0;
    bar.addEventListener("mousedown", function (e) {
      if (e.target.closest("button, a")) return;
      drag = true; var r = win.getBoundingClientRect(); ox = e.clientX - r.left; oy = e.clientY - r.top;
      document.body.style.userSelect = "none"; e.preventDefault();
    });
    document.addEventListener("mousemove", function (e) { if (drag) { win.style.left = e.clientX - ox + "px"; win.style.top = e.clientY - oy + "px"; } });
    document.addEventListener("mouseup", function () { if (drag) { drag = false; document.body.style.userSelect = ""; clamp(); save(); } });
    if (window.ResizeObserver) new ResizeObserver(function () { if (!win.hidden) save(); }).observe(win);
  }

  // ── confirm modal (replaces window.confirm for hx-confirm) ─────────────
  function initModal() {
    var m = $("#pf-modal"); if (!m) return;
    var pending = null;
    function close() { m.hidden = true; pending = null; }
    document.addEventListener("htmx:confirm", function (e) {
      if (!e.detail.question) return;
      e.preventDefault();
      $("#pf-modal-q").textContent = e.detail.question;
      pending = e.detail; m.hidden = false; $("#pf-modal-ok").focus();
    });
    $("#pf-modal-ok").onclick = function () { if (pending) { var d = pending; close(); d.issueRequest(true); } };
    $("#pf-modal-cancel").onclick = close;
    m.addEventListener("click", function (e) { if (e.target === m) close(); });
    document.addEventListener("keydown", function (e) { if (e.key === "Escape" && !m.hidden) close(); });
  }

  // ── drawer (edit leg) ───────────────────────────────────────────────────
  function drawerOpen() { var d = $("#pf-drawer"); $("#pf-drawer-scrim").hidden = false; d.setAttribute("aria-hidden", "false"); requestAnimationFrame(function () { d.classList.add("open"); }); var f = $("input:not([type=hidden])", d); if (f) f.focus(); }
  function drawerClose() { var d = $("#pf-drawer"); if (!d) return; d.classList.remove("open"); d.setAttribute("aria-hidden", "true"); $("#pf-drawer-scrim").hidden = true; }
  function initDrawer() {
    if (!$("#pf-drawer")) return;
    $("#pf-drawer-scrim").addEventListener("click", drawerClose);
    document.addEventListener("keydown", function (e) { if (e.key === "Escape") drawerClose(); });
    document.body.addEventListener("htmx:afterSwap", function (e) { if (e.target.id === "pf-drawer" && e.target.innerHTML.trim()) drawerOpen(); });
    document.body.addEventListener("pf-saved", function (e) {
      drawerClose();
      toast((e.detail && e.detail.value) || "Saved", "", "info", 3000);
      if (window.htmx) htmx.trigger(document.body, "pf:state");
    });
  }

  // ── activity filters ────────────────────────────────────────────────────
  var actLevel = "all", actCat = "all";
  function applyActivityFilter() {
    var feed = $("#activity-feed"); if (!feed) return;
    $$(".feed-row", feed).forEach(function (r) {
      var lv = r.dataset.level;
      var okLevel = actLevel === "all" || (actLevel === "warn" ? lv === "warn" || lv === "error" : lv === actLevel);
      var okCat = actCat === "all" || r.dataset.cat === actCat;
      r.hidden = !(okLevel && okCat);
    });
  }

  // ── settings: private-key check (client-only, instant) ─────────────────
  function keyState(v) {
    var body = (v || "").trim().replace(/^0[xX]/, "");
    if (!body.length) return "empty";
    if (!/^[0-9a-fA-F]*$/.test(body)) return "invalid";
    if (body.length < 64) return "partial";
    return body.length === 64 ? "complete" : "invalid";
  }
  PF.keyLooksValid = function (v) { return keyState(v) === "complete"; };
  function onKeyInput(el) {
    var st = keyState(el.value), status = $("#wallet-detect-status"), wrap = $("#wallet-field-wrap");
    if (st !== "complete" && wrap) { wrap.className = "pf-collapse"; }
    if (!status) return;
    if (st === "empty" || st === "partial") status.innerHTML = "";
    else if (st === "invalid") status.innerHTML = '<div class="help" style="color:var(--ask)">That isn\'t a valid private key (64 hex characters).</div>';
    else status.innerHTML = '<div class="help">Looking up your Polymarket wallet on-chain…</div>';
  }

  // ── market view ─────────────────────────────────────────────────────────
  var MV = null;

  function mvInit() {
    var root = $("#mv"); if (!root) return;
    MV = {
      root: root,
      slug: root.dataset.slug,
      side: parseInt(root.dataset.side, 10) || 0,
      minShares: num(root.dataset.minShares) || 0,
      maxSpread: num(root.dataset.maxSpread) || 0, // cents
      tickC: num(root.dataset.tick) || 1,           // cents
      group: "",
      range: "1w",
      es: null,
      gotEvent: false,
      fallbackTimer: null,
      scrolled: false,
      book: null
    };
    var g = pref("pf-group", "");
    if (g && $('#group button[data-group="' + g + '"]')) { MV.group = g; mvSyncGroup(); }
    $("#price").addEventListener("input", function () { mvSyncShares(); mvUpdate(); });
    $("#order_size").addEventListener("input", function () { mvSyncShares(); mvUpdate(); });
    $("#order_shares").addEventListener("input", function () { mvSyncUsd(); mvUpdate(); });
    mvReadBook();
    mvCenterBook();
    mvSyncShares();
    mvUpdate(true);
    mvConnect();
    if (MV.group) mvFetchBook();
    window.addEventListener("pagehide", function () { if (MV.es) MV.es.close(); });
  }

  function decimals(x) { var s = String(x); var i = s.indexOf("."); return i < 0 ? 0 : s.length - i - 1; }
  function fmtC(c) { return (Math.round(c / MV.tickC) * MV.tickC).toFixed(decimals(MV.tickC)); }
  function priceP() { return num($("#price").value) / 100; }   // price units
  function both() { return $("#f-sides").value === "both"; }

  function mvSyncShares() {
    var p = priceP(), usd = num($("#order_size").value);
    $("#order_shares").value = p > 0 && usd > 0 ? Math.round(usd / p) : "";
  }
  function mvSyncUsd() {
    var p = priceP(), sh = num($("#order_shares").value);
    if (p > 0 && sh > 0) $("#order_size").value = (sh * p).toFixed(2);
  }

  function mvReadBook() {
    var d = $("#book-data");
    if (!d) { MV.book = null; return; }
    MV.book = { bb: num(d.dataset.bb), ba: num(d.dataset.ba), mid: num(d.dataset.mid) };
    $("#zone-bounds").textContent = d.dataset.blo + "–" + d.dataset.bhi + "¢";
    $("#zone-liq").textContent = d.dataset.usdc;
  }

  // Polymarket scoring: ((v − s) / v)², s = distance from mid in cents.
  function weight(mid, p) {
    if (!(MV.maxSpread > 0) || !(p > 0)) return 0;
    var s = Math.abs(mid - p) * 100;
    if (s >= MV.maxSpread) return 0;
    var r = (MV.maxSpread - s) / MV.maxSpread; return r * r;
  }
  function singleOk(mid) { return mid >= 0.1 && mid <= 0.9; }
  function effective(mid, a, b) {
    var lo = Math.min(a, b == null ? 0 : b), hi = Math.max(a, b == null ? 0 : b);
    return singleOk(mid) ? Math.max(lo, hi / 3) : lo;
  }
  function pct(w) { return Math.round(w * 100) + "%"; }

  // Instant (client-side) reward weight + peg hint + preview marker.
  function mvUpdate(skipPreview) {
    var p = priceP(), usd = num($("#order_size").value), b = MV.book;
    var box = $("#score"), val = $("#score-val"), bar = $("#score-bar"), why = $("#score-why");
    var reasons = [], eff = 0;

    if (!(MV.maxSpread > 0)) {
      box.className = "score bad"; val.textContent = "—"; bar.style.width = "0";
      why.textContent = "No reward program on this market.";
    } else if (!b || !(b.mid > 0) || !(p > 0) || !(usd > 0)) {
      box.className = "score"; val.textContent = "—"; bar.style.width = "0"; why.textContent = "";
    } else {
      var perUsd = both() ? usd / 2 : usd;
      var shares = perUsd / p;
      var meets = shares >= MV.minShares;
      var w = weight(b.mid, p), thisW = meets ? w : 0, otherW = null;
      if (!meets) reasons.push("needs ≥ " + MV.minShares + " shares per side (have " + Math.floor(shares) + ")");
      if (w === 0) reasons.push(p >= b.mid ? "at or above the midpoint" : "outside the reward zone (±" + MV.maxSpread + "¢)");
      if (b.bb > 0 && p >= b.bb) reasons.push("must sit below the best bid (" + (b.bb * 100).toFixed(1) + "¢)");
      if (both()) {
        if (b.bb > 0 && b.ba > 0) {
          var op = (1 - b.ba) - (b.bb - p), om = 1 - b.mid, osh = op > 0 ? perUsd / op : 0;
          otherW = op > 0 && osh >= MV.minShares ? weight(om, op) : 0;
          if (otherW === 0) reasons.push("other leg (" + (op * 100).toFixed(1) + "¢) doesn't score");
        } else otherW = 0;
      }
      eff = effective(b.mid, thisW, otherW);
      if (!both()) reasons.push(singleOk(b.mid) ? "one-sided scores ÷3 — Both sides for full weight" : "one-sided earns 0 while mid is under 10¢ / over 90¢");
      var tone = eff >= 0.4 ? "ok" : eff > 0 ? "warn" : "bad";
      box.className = "score " + tone;
      val.textContent = pct(eff);
      bar.style.width = Math.min(100, eff * 100) + "%";
      why.textContent = reasons.length ? reasons.join(" · ") : "Inside the zone, two-sided, above the size minimum.";
    }
    // peg hint
    var hint = $("#peg-hint");
    if (hint) hint.textContent = b && b.bb > 0 && p > 0 && p < b.bb ? "follows " + ((b.bb - p) * 100).toFixed(1) + "¢ below best bid" : "";
    // preset highlight
    $$(".presets:not([hidden]) .preset").forEach(function (el) { el.classList.toggle("selected", Math.abs(num(el.dataset.price) - p * 100) < 1e-6); });
    mvMarkPreview();
    if (!skipPreview) { var pv = $("#placement-preview"); if (pv && window.htmx) htmx.trigger(pv, "pf:preview"); }
  }

  function mvMarkPreview() {
    var p = priceP();
    $$("#book .book-row.preview").forEach(function (r) { r.classList.remove("preview"); });
    if (!(p > 0)) return;
    var g = num(MV.group) || 0, target = g > 0 ? Math.floor(p / g + 1e-9) * g : p;
    var rows = $$("#book .book-row.bid[data-price]");
    for (var i = 0; i < rows.length; i++) {
      if (Math.abs(num(rows[i].dataset.price) - target) < 1e-9) { rows[i].classList.add("preview"); break; }
    }
  }

  // Centre the ladder on the spread. The spread row is position:sticky, so its
  // offsetTop is where it's stuck, not where it sits — anchor on the bids block.
  function mvCenterBook() {
    var b = $("#book"), bids = b && $(".book-side.bids", b);
    if (b && bids) b.scrollTop = Math.max(0, bids.offsetTop - b.clientHeight / 2);
  }

  function mvSwapBook(html) {
    var b = $("#book"); if (!b) return;
    var top = b.scrollTop;
    b.innerHTML = html;
    if (MV.scrolled) b.scrollTop = top; else mvCenterBook();
    mvReadBook();
    mvUpdate(true);
  }

  function mvLive(state) {
    var el = $("#book-live"); if (!el) return;
    var dot = $(".dot", el), txt = el.lastElementChild;
    dot.className = "dot " + (state === "live" ? "ok" : state === "connecting" ? "idle" : "warn");
    txt.textContent = state === "live" ? "live" : state === "connecting" ? "connecting" : "polling";
  }

  function mvBookParams() { return "slug=" + encodeURIComponent(MV.slug) + "&side=" + MV.side + (MV.group ? "&group=" + encodeURIComponent(MV.group) : ""); }
  function mvFetchBook() {
    fetch("/markets/view/book?" + mvBookParams()).then(function (r) { return r.text(); }).then(mvSwapBook).catch(function () {});
  }

  // Live book over SSE; if no update arrives, fall back to REST polling until it does.
  function mvConnect() {
    if (MV.es) MV.es.close();
    clearInterval(MV.fallbackTimer);
    MV.gotEvent = false;
    mvLive("connecting");
    if (!window.EventSource) { MV.fallbackTimer = setInterval(mvFetchBook, 3000); mvLive("polling"); return; }
    var es = (MV.es = new EventSource("/markets/view/book/stream?" + mvBookParams()));
    es.addEventListener("book", function (e) {
      if (!MV.gotEvent) { MV.gotEvent = true; clearInterval(MV.fallbackTimer); }
      mvLive("live");
      mvSwapBook(e.data);
    });
    es.onerror = function () { mvLive("polling"); };
    setTimeout(function () {
      if (!MV.gotEvent && MV.es === es) { mvLive("polling"); MV.fallbackTimer = setInterval(function () { if (!MV.gotEvent) mvFetchBook(); }, 3000); }
    }, 8000);
  }

  function mvSyncGroup() { $$("#group button").forEach(function (b) { b.classList.toggle("selected", b.dataset.group === MV.group || (!MV.group && b === $("#group button"))); }); }
  function mvSetGroup(g) { MV.group = g; setPref("pf-group", g); mvSyncGroup(); mvFetchBook(); mvConnect(); }

  function mvLoadChart() {
    if (window.htmx) htmx.ajax("GET", "/markets/view/chart?slug=" + encodeURIComponent(MV.slug) + "&side=" + MV.side + "&range=" + MV.range, "#chart");
  }

  function mvSelectSide(i) {
    MV.side = i;
    $("#f-side").value = i;
    var btn = $('#side-toggle button[data-side="' + i + '"]');
    $$("#side-toggle button").forEach(function (b) { b.classList.toggle("selected", b === btn); });
    $$(".presets").forEach(function (p) { p.hidden = p.dataset.presets !== String(i); });
    var label = btn ? btn.dataset.label : "";
    $("#book-title").textContent = label; $("#chart-title").textContent = label;
    var pre = $('.presets[data-presets="' + i + '"] .preset:nth-child(2)') || $('.presets[data-presets="' + i + '"] .preset');
    if (pre) $("#price").value = pre.dataset.price;
    MV.scrolled = false;
    mvFetchBook(); mvConnect(); mvLoadChart();
    mvSyncShares(); mvUpdate();
  }

  function mvSetMode(m) {
    $("#f-sides").value = m;
    $$("#mode-toggle button").forEach(function (b) { b.classList.toggle("selected", b.dataset.mode === m); });
    $("#split-note").hidden = m !== "both";
    mvUpdate();
  }

  // ── click router (data-action) ──────────────────────────────────────────
  document.addEventListener("click", function (e) {
    var el = e.target.closest("[data-action]"); if (!el) return;
    var a = el.dataset.action;
    switch (a) {
      case "theme": setTheme(theme() === "dark" ? "light" : "dark"); break;
      case "pref-theme": setTheme(el.dataset.value); break;
      case "pref-toasts": setPref("pf-toasts", el.dataset.value); syncPrefSegs(); break;
      case "drawer-close": drawerClose(); break;
      case "reveal": {
        var k = $("#private_key"), show = k.type === "password";
        k.type = show ? "text" : "password"; el.setAttribute("aria-pressed", show); el.setAttribute("aria-label", show ? "Hide private key" : "Show private key");
        break;
      }
      case "pick-wallet": $("#proxy_wallet").value = el.dataset.addr; break;
      case "filter": {
        var kind = el.dataset.kind, v = el.dataset.value;
        if (kind === "level") actLevel = v; else actCat = v;
        $$('[data-action="filter"][data-kind="' + kind + '"]').forEach(function (c) { c.classList.toggle("selected", c === el); });
        applyActivityFilter(); break;
      }
      case "tab": $$("button", el.parentElement).forEach(function (b) { b.classList.toggle("selected", b === el); }); break;
      case "side": if (MV) mvSelectSide(parseInt(el.dataset.side, 10)); break;
      case "mode": if (MV) mvSetMode(el.dataset.mode); break;
      case "preset": if (MV) { $("#price").value = el.dataset.price; mvSyncShares(); mvUpdate(); } break;
      case "book-price": if (MV) { $("#price").value = fmtC(num(el.dataset.price) * 100); mvSyncShares(); mvUpdate(); } break;
      case "group": if (MV) mvSetGroup(el.dataset.group); break;
      case "range": if (MV) { MV.range = el.dataset.range; $$("#ranges button").forEach(function (b) { b.classList.toggle("selected", b === el); }); mvLoadChart(); } break;
    }
  });

  // Market thumbnails come from Polymarket's CDN; hide any that fail to load.
  document.addEventListener("error", function (e) { if (e.target.tagName === "IMG") e.target.style.visibility = "hidden"; }, true);
  document.addEventListener("input", function (e) { if (e.target.matches && e.target.matches("[data-keycheck]")) onKeyInput(e.target); });
  document.addEventListener("scroll", function (e) { if (MV && e.target && e.target.id === "book") MV.scrolled = true; }, true);

  // ── boot ────────────────────────────────────────────────────────────────
  function boot() {
    syncPrefSegs();
    initModal();
    initDrawer();
    initWindow();
    connectEvents();
    mvInit();
    PF.relTime();
    setInterval(function () { PF.relTime(); }, 30000);
    document.body.addEventListener("htmx:afterSwap", function (e) { PF.relTime(e.target); applyActivityFilter(); });
  }
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", boot); else boot();
})();

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
  // Older than a week: the date, and (where there's room) the time of day,
  // which the day headings above a feed don't give.
  function relTime(iso, withTime) {
    var t = new Date(iso).getTime(); if (isNaN(t)) return "";
    var s = Math.max(0, (Date.now() - t) / 1000);
    if (s < 45) return "just now";
    if (s < 3600) return Math.floor(s / 60) + "m ago";
    if (s < 86400) return Math.floor(s / 3600) + "h ago";
    if (s < 86400 * 7) return Math.floor(s / 86400) + "d ago";
    var opts = { month: "short", day: "numeric" };
    if (withTime) { opts.hour = "numeric"; opts.minute = "2-digit"; }
    return new Date(t).toLocaleString(undefined, opts);
  }
  PF.relTime = function (root) {
    $$(".feed-time[data-ts]", root).forEach(function (el) {
      var iso = el.getAttribute("data-ts");
      el.textContent = relTime(iso, !el.closest(".tbl"));   // table Time columns stay compact
      el.title = new Date(iso).toLocaleString();
    });
  };

  // UTC moments rendered by the server (<time class="pf-local">) → the viewer's
  // own time with the UTC original in brackets: "07:00 (00:00 UTC)".
  PF.localTimes = function (root) {
    $$("time.pf-local[datetime]", root).forEach(function (el) {
      var d = new Date(el.getAttribute("datetime"));
      if (isNaN(d.getTime())) return;
      var utc = el.getAttribute("data-utc") || el.textContent;
      el.setAttribute("data-utc", utc);
      el.title = d.toLocaleString();
      if (d.getTimezoneOffset() === 0) { el.textContent = utc; return; }
      // Local time in 12-hour form ("7:00 AM"); the UTC part stays 24-hour.
      var opts = { hour: "numeric", minute: "2-digit", hour12: true };
      var dated = el.hasAttribute("data-date");
      if (dated) { opts.month = "short"; opts.day = "numeric"; }
      // The bracket repeats the date only when UTC is on a different day.
      var sameDay = d.getUTCDate() === d.getDate();
      var utcOpts = { hour: "2-digit", minute: "2-digit", hourCycle: "h23", timeZone: "UTC" };
      if (dated && !sameDay) { utcOpts.month = "short"; utcOpts.day = "numeric"; }
      el.textContent = d.toLocaleString(undefined, opts) + " (" + d.toLocaleString(undefined, utcOpts) + " UTC)";
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
    if (ttl !== -1) setTimeout(dismiss, ttl || 6000); // -1: stays until dismissed
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
    var pending = null, ok = $("#pf-modal-ok");
    // Back to the button that opened it (it's still on the page: swaps of its
    // panel are held while the dialog is open).
    function close() { var back = pending && pending.elt; m.hidden = true; pending = null; if (back && back.isConnected) back.focus(); }
    document.addEventListener("htmx:confirm", function (e) {
      if (!e.detail.question) return;
      e.preventDefault();
      $("#pf-modal-q").textContent = e.detail.question;
      // The action's own name ("Remove") on the button; Cancel has focus, so
      // Enter or Space never confirms by accident.
      ok.textContent = (e.detail.elt && e.detail.elt.getAttribute("data-confirm-label")) || "Confirm";
      pending = e.detail; m.hidden = false; $("#pf-modal-cancel").focus();
    });
    // While the dialog is open, don't let a background refresh replace the
    // panel holding the clicked button: htmx silently drops a request whose
    // element has left the page, so Confirm would do nothing.
    document.body.addEventListener("htmx:beforeSwap", function (e) {
      if (pending && e.detail.target && e.detail.target.contains(pending.elt)) e.detail.shouldSwap = false;
    });
    ok.onclick = function () {
      if (!pending) return;
      var d = pending; pending = null; m.hidden = true;
      // The row may disappear with the response: park focus on its panel.
      var home = d.elt && d.elt.closest("[tabindex='-1']");
      if (home) home.focus({ preventScroll: true });
      d.issueRequest(true);
    };
    $("#pf-modal-cancel").onclick = close;
    m.addEventListener("click", function (e) { if (e.target === m) close(); });
    document.addEventListener("keydown", function (e) {
      if (m.hidden) return;
      if (e.key === "Escape") { close(); return; }
      if (e.key === "Tab") trapTab(e, m);
    });
  }

  // Keep Tab / Shift+Tab inside an open dialog or drawer.
  function trapTab(e, box) {
    var f = $$("a[href], button:not([disabled]), input:not([disabled]):not([type=hidden]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex='-1'])", box)
      .filter(function (el) { return el.offsetParent !== null; });
    if (!f.length) { e.preventDefault(); return; }
    var first = f[0], last = f[f.length - 1];
    if (!box.contains(document.activeElement)) { e.preventDefault(); first.focus(); }
    else if (e.shiftKey && document.activeElement === first) { e.preventDefault(); last.focus(); }
    else if (!e.shiftKey && document.activeElement === last) { e.preventDefault(); first.focus(); }
  }

  // ── drawer (edit leg) ───────────────────────────────────────────────────
  // The closed drawer is inert (no Tab stops, no Enter-submit of its hidden
  // form). Opening focuses its first field once it's visible; every way of
  // closing it returns focus to the button that opened it (found by id, as
  // the table may have refreshed meanwhile).
  var drawerOpener = null;
  function drawerIsOpen() { var d = $("#pf-drawer"); return !!d && d.classList.contains("open"); }
  function drawerOpen() {
    var d = $("#pf-drawer"); $("#pf-drawer-scrim").hidden = false;
    d.removeAttribute("inert"); d.setAttribute("aria-hidden", "false");
    requestAnimationFrame(function () {
      d.classList.add("open");
      var f = $("[data-autofocus]", d) || $("input:not([type=hidden])", d); if (f) f.focus({ preventScroll: true });
    });
  }
  function drawerClose() {
    var d = $("#pf-drawer"); if (!d || !drawerIsOpen()) return;
    d.classList.remove("open"); d.setAttribute("aria-hidden", "true"); d.setAttribute("inert", "");
    $("#pf-drawer-scrim").hidden = true;
    var back = drawerOpener && document.getElementById(drawerOpener); drawerOpener = null;
    if (back) back.focus();
  }
  function initDrawer() {
    if (!$("#pf-drawer")) return;
    $("#pf-drawer-scrim").addEventListener("click", drawerClose);
    document.addEventListener("keydown", function (e) {
      var modal = $("#pf-modal");
      if (!drawerIsOpen() || (modal && !modal.hidden)) return;   // the dialog handles its own keys
      if (e.key === "Escape") drawerClose();
      else if (e.key === "Tab") trapTab(e, $("#pf-drawer"));
    });
    document.body.addEventListener("htmx:beforeRequest", function (e) {
      if (e.detail.target && e.detail.target.id === "pf-drawer" && e.detail.elt && e.detail.elt.id && !$("#pf-drawer").contains(e.detail.elt)) drawerOpener = e.detail.elt.id;
    });
    document.body.addEventListener("htmx:afterSwap", function (e) { if (e.target.id === "pf-drawer" && e.target.innerHTML.trim()) drawerOpen(); });
    // Errors from a row action (server sends HX-Trigger: {"pf-error": "…"}). A
    // toast, because the panel's own banner is replaced by the next refresh.
    document.body.addEventListener("pf-error", function (e) {
      toast("Couldn't do that", (e.detail && e.detail.value) || "", "err", -1);
    });
    document.body.addEventListener("pf-saved", function (e) {
      drawerClose();
      toast((e.detail && e.detail.value) || "Saved", "", "info", 3000);
      if (window.htmx) htmx.trigger(document.body, "pf:state");
    });
  }

  // ── refreshes keep your place ───────────────────────────────────────────
  // Live fragments (the Markets table every 5s, the Overview book every 10s)
  // are replaced wholesale. Focus survives because every control in them has
  // a stable id (htmx restores focus by id); the sideways scroll of their
  // tables is carried across the swap here.
  var keptScroll = null;
  document.addEventListener("htmx:beforeSwap", function (e) {
    var t = e.detail.target; keptScroll = null;
    if (!t) return;
    var wraps = $$(".tbl-wrap", t);
    if (wraps.some(function (w) { return w.scrollLeft > 0; })) keptScroll = { target: t, left: wraps.map(function (w) { return w.scrollLeft; }) };
  });
  document.addEventListener("htmx:afterSwap", function (e) {
    if (!keptScroll || keptScroll.target !== e.detail.target) return;
    var left = keptScroll.left; keptScroll = null;
    $$(".tbl-wrap", e.detail.target).forEach(function (w, i) { if (left[i]) w.scrollLeft = left[i]; });
  });

  // ── "Added to your markets" after Start farming ─────────────────────────
  // The start request answers with a redirect to Markets; note the market in
  // sessionStorage and say so on the next page. (No handler change: this only
  // reads the response the form already gets.)
  document.addEventListener("htmx:afterRequest", function (e) {
    var d = e.detail;
    // (htmx follows HX-Redirect before it sets d.successful, so check the status.)
    if (!d || !d.elt || d.elt.id !== "order-form" || !d.xhr || d.xhr.status < 200 || d.xhr.status >= 300 || !d.xhr.getResponseHeader("HX-Redirect")) return;
    var h = $(".mv-head h1");
    try { sessionStorage.setItem("pf-started", ((h && h.textContent) || "").trim().slice(0, 200)); } catch (_) {}
  });
  function startedToast() {
    var q = null;
    try { q = sessionStorage.getItem("pf-started"); sessionStorage.removeItem("pf-started"); } catch (_) {}
    if (q !== null) toast("Added to your markets", q, "info", 8000);
  }

  // ── picker: markets without rewards are tucked away ────────────────────
  function setPickerDim(hide) {
    var b = $('[data-action="picker-toggle"]'); if (!b) return;
    $$("#picker-table tr.dim").forEach(function (r) { r.hidden = hide; });
    b.dataset.shown = hide ? "" : "1";
    b.textContent = (hide ? "Show " : "Hide ") + b.dataset.n + " markets without rewards";
  }
  function initPicker() { var more = $(".picker-more"); if (!more) return; more.hidden = false; setPickerDim(true); }

  // ── activity filters ────────────────────────────────────────────────────
  var actLevel = "all", actCat = "all";
  function applyActivityFilter() {
    var feed = $("#activity-feed"); if (!feed) return;
    var rows = $$(".feed-row", feed), shown = 0;
    rows.forEach(function (r) {
      var lv = r.dataset.level;
      var okLevel = actLevel === "all" || (actLevel === "warn" ? lv === "warn" || lv === "error" : lv === actLevel);
      var okCat = actCat === "all" || r.dataset.cat === actCat;
      r.hidden = !(okLevel && okCat);
      if (!r.hidden) shown++;
    });
    // A day heading shows only while one of its rows does.
    $$(".feed-day", feed).forEach(function (d) {
      var any = false;
      for (var n = d.nextElementSibling; n && !n.classList.contains("feed-day"); n = n.nextElementSibling) {
        if (n.classList.contains("feed-row") && !n.hidden) { any = true; break; }
      }
      d.hidden = !any;
    });
    // Its own class: prependFeeds removes .feed-empty when live rows arrive.
    var none = $(".feed-nomatch", feed);
    if (rows.length && !shown) {
      if (!none) { none = document.createElement("div"); none.className = "feed-nomatch"; none.textContent = "No activity matches this filter."; feed.appendChild(none); }
      none.hidden = false;
    } else if (none) none.hidden = true;
  }

  // ── settings: private-key check (client-only, instant) ─────────────────
  function keyState(v) {
    var body = (v || "").trim().replace(/^0[xX]/, "");
    if (!body.length) return "empty";
    if (!/^[0-9a-fA-F]*$/.test(body)) return "invalid";
    if (body.length < 64) return "partial";
    return body.length === 64 ? "complete" : "invalid";
  }
  function onKeyInput(el) {
    var st = keyState(el.value), status = $("#wallet-detect-status"), wrap = $("#wallet-field-wrap");
    if (st !== "complete" && wrap) { wrap.className = "pf-collapse"; }
    if (!status) return;
    if (st === "empty" || st === "partial") status.innerHTML = "";
    else if (st === "invalid") status.innerHTML = '<div class="help" style="color:var(--ask)">That isn\'t a valid private key (64 hex characters).</div>';
    else {
      status.innerHTML = '<div class="help">Looking up your Polymarket wallet on-chain…</div>';
      // Only a complete 64-hex key is sent to the server for wallet detection (htmx
      // listens for this event; a partial key never leaves the browser).
      el.dispatchEvent(new CustomEvent("pf-key-complete", { bubbles: true }));
    }
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
    // What the ladder on screen shows: the server renders the page's side at
    // the raw tick; later swaps say which side and grouping they were asked for.
    MV.shown = { side: MV.side, group: "" };
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
    // Keep keyboard focus when the panel is refreshed from under it (htmx
    // restores focus by id, so only a vanished element needs help).
    var posFocus = null;
    document.body.addEventListener("htmx:beforeSwap", function (e) {
      if (e.target.id !== "your-position") return;
      var a = document.activeElement;
      // Focus on the container itself (where Remove's confirm parks it) has no
      // element to restore: "" means "find it a new home after the swap".
      posFocus = e.target.firstElementChild && e.target.contains(a) ? (a !== e.target && a.id) || "" : null;
    });
    document.body.addEventListener("htmx:afterSwap", function (e) {
      if (e.target.id !== "your-position") return;
      mvDupNote();
      var had = posFocus; posFocus = null;
      if (had === null || (had && document.getElementById(had))) return;
      if (!e.target.firstElementChild) {
        // The last leg was removed: carry focus on to the order form below.
        var form = $("#order-form").closest("section");
        form.setAttribute("tabindex", "-1"); form.focus({ preventScroll: true });
      } else {
        // An extended leg loses its Extend button: land on its Edit button.
        var alt = had && document.getElementById(had.replace(/-extend$/, "-edit"));
        (alt || e.target).focus({ preventScroll: true });
      }
    });
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
    // Round up to the cent: rounding down would buy slightly fewer shares than
    // typed (108 × 21.9¢ = $23.652 → $23.65 → 107.99), missing a share minimum.
    if (p > 0 && sh > 0) $("#order_size").value = (Math.ceil(Math.round(sh * p * 1e6) / 1e4) / 100).toFixed(2);
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

  // Instant (client-side) reward weight + peg hint.
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
      // The other leg's price (Both sides): same distance below its own best bid.
      var op = both() && b.bb > 0 && b.ba > 0 ? (1 - b.ba) - (b.bb - p) : 0;
      if (!meets) {
        // What it would take: the minimum in shares at the dearer leg's price.
        var needPer = MV.minShares * Math.max(p, op > 0 ? op : 0);
        reasons.push("needs ≥ " + MV.minShares + " shares per side (have " + Math.floor(shares) + "): about $" + Math.ceil(needPer * (both() ? 2 : 1)) + " in total");
      }
      if (w === 0) reasons.push(p >= b.mid ? "at or above the midpoint" : "outside the reward zone (±" + MV.maxSpread + "¢)");
      if (b.bb > 0 && p >= b.bb) reasons.push("must sit below the best bid (" + (b.bb * 100).toFixed(1) + "¢)");
      if (both()) {
        if (b.bb > 0 && b.ba > 0) {
          var om = 1 - b.mid, osh = op > 0 ? perUsd / op : 0;
          otherW = op > 0 && osh >= MV.minShares ? weight(om, op) : 0;
          if (otherW === 0) reasons.push("other leg (" + (op * 100).toFixed(1) + "¢) doesn't score");
        } else otherW = 0;
      }
      eff = effective(b.mid, thisW, otherW);
      // (The "one side scores a third" note is in the preview below; only the
      // case that explains a 0% is repeated here.)
      if (!both() && !singleOk(b.mid)) reasons.push("one-sided earns 0 while mid is under 10¢ / over 90¢");
      var tone = eff >= 0.4 ? "ok" : eff > 0 ? "warn" : "bad";
      box.className = "score " + tone;
      val.textContent = pct(eff);
      bar.style.width = Math.min(100, eff * 100) + "%";
      why.textContent = reasons.length ? reasons.join(" · ") : both() ? "Inside the zone, two-sided, above the size minimum." : "Inside the zone and above the size minimum.";
    }
    // peg hint
    var hint = $("#peg-hint");
    if (hint) hint.textContent = b && b.bb > 0 && p > 0 && p < b.bb ? "follows " + ((b.bb - p) * 100).toFixed(1) + "¢ below best bid" : "";
    mvMarkBid();
    mvDupNote();
    if (!skipPreview) { var pv = $("#placement-preview"); if (pv && window.htmx) htmx.trigger(pv, "pf:preview"); }
  }

  // "You already farm Yes here": starting again would add a second leg on the
  // same outcome. Which outcomes you farm comes from the panel above the form
  // (#position-panel, refreshed every 5s and after every change).
  function mvDupNote() {
    var note = $("#dup-note"); if (!note) return;
    var panel = $("#position-panel");
    var farmed = panel && panel.dataset.farmed ? panel.dataset.farmed.split(",") : [];
    var picked = both() ? $$("#side-toggle button") : [$('#side-toggle button[data-side="' + MV.side + '"]')];
    var dup = picked.filter(function (b) { return b && b.dataset.token && farmed.indexOf(b.dataset.token) >= 0; })
      .map(function (b) { return b.dataset.label; });
    note.hidden = !dup.length;
    note.textContent = !dup.length ? "" :
      "You already farm " + dup.join(" and ") + " here. Starting adds " +
      (dup.length > 1 ? "a second leg on each" : "a second " + dup[0] + " leg") +
      "; to change the one you have, use Edit above.";
  }

  // Show the price in the form on the bid ladder: its level is outlined, or a
  // marker line sits where the bid would go when nobody bids that price.
  // The ladder is re-rendered on every book update, so this runs after each.
  function mvMarkBid() {
    var side = $("#book .book-side.bids"); if (!side) return;
    $$(".book-row.picked", side).forEach(function (r) { r.classList.remove("picked"); });
    var old = $(".pick-line", side); if (old) old.remove();
    // Right after a Yes/No switch the other outcome's ladder is still on screen.
    if (!MV.shown || MV.shown.side !== MV.side) return;
    var p = priceP(), b = MV.book;
    if (!(p > 0) || !b || (b.bb > 0 && p > b.bb)) return;   // only where a bid can rest
    var rows = $$(".book-row.bid", side); if (!rows.length) return;
    var tick = function (x) { return Math.round(x * 100 / MV.tickC); };
    // With price grouping on, a bid's row is its bucket, floored like the
    // server does it (market_data::build_ladder).
    var per = Math.max(1, tick(num(MV.shown.group) || 0)), want = Math.floor(tick(p) / per) * per, before = null;
    for (var i = 0; i < rows.length; i++) {
      var t = tick(num(rows[i].dataset.price));
      if (t === want) { rows[i].classList.add("picked"); return; }
      if (t < want) { before = rows[i]; break; }   // bids run best (highest) first
    }
    var line = document.createElement("div");
    line.className = "book-row pick-line";
    var px = document.createElement("span"); px.className = "px"; px.textContent = fmtC(p * 100) + "¢";
    var tag = document.createElement("span"); tag.className = "pick-tag"; tag.textContent = before ? "your bid · no orders here yet" : "your bid · below the levels shown";
    line.appendChild(px); line.appendChild(tag);
    side.insertBefore(line, before);
  }

  // Centre the ladder on the spread. The spread row is position:sticky, so its
  // offsetTop is where it's stuck, not where it sits — anchor on the bids block.
  function mvCenterBook() {
    var b = $("#book"), bids = b && $(".book-side.bids", b);
    if (b && bids) b.scrollTop = Math.max(0, bids.offsetTop - b.clientHeight / 2);
  }

  function mvSwapBook(html, shown) {
    var b = $("#book"); if (!b) return;
    MV.shown = shown;
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
  function mvAsked() { return { side: MV.side, group: MV.group }; }
  function mvFetchBook() {
    var asked = mvAsked();
    fetch("/markets/view/book?" + mvBookParams()).then(function (r) { return r.text(); }).then(function (h) { mvSwapBook(h, asked); }).catch(function () {});
  }

  // Live book over SSE; if no update arrives, fall back to REST polling until it does.
  function mvConnect() {
    if (MV.es) MV.es.close();
    clearInterval(MV.fallbackTimer);
    MV.gotEvent = false;
    mvLive("connecting");
    if (!window.EventSource) { MV.fallbackTimer = setInterval(mvFetchBook, 3000); mvLive("polling"); return; }
    var asked = mvAsked(), es = (MV.es = new EventSource("/markets/view/book/stream?" + mvBookParams()));
    es.addEventListener("book", function (e) {
      if (!MV.gotEvent) { MV.gotEvent = true; clearInterval(MV.fallbackTimer); }
      mvLive("live");
      mvSwapBook(e.data, asked);
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
    var label = btn ? btn.dataset.label : "";
    $("#book-title").textContent = label; $("#chart-title").textContent = label;
    if (btn && btn.dataset.defaultPrice) $("#price").value = btn.dataset.defaultPrice;
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


  // ── minimizable panels (market page: Trades, Top holders) ───────────────
  // The set lives in localStorage ("pf-collapsed", space-separated keys) and on
  // <html data-collapsed>, which theme.js restores before first paint and the
  // CSS reads. A minimized panel's first fetch is held until it is opened.
  function collapsedKeys() { return (document.documentElement.dataset.collapsed || "").split(" ").filter(Boolean); }
  function isCollapsed(key) { return collapsedKeys().indexOf(key) >= 0; }
  function syncCollapseButtons() {
    $$(".collapse-btn[data-key]").forEach(function (b) {
      var shut = isCollapsed(b.dataset.key), name = b.getAttribute("aria-label").replace(/^(Minimize|Expand) /, "");
      b.setAttribute("aria-expanded", String(!shut));
      b.title = (shut ? "Expand " : "Minimize ") + name;
      b.setAttribute("aria-label", b.title);
    });
  }
  function setCollapsed(key, shut) {
    var keys = collapsedKeys().filter(function (k) { return k !== key; });
    if (shut) keys.push(key);
    var v = keys.join(" ");
    if (v) document.documentElement.dataset.collapsed = v; else delete document.documentElement.dataset.collapsed;
    try { if (v) localStorage.setItem("pf-collapsed", v); else localStorage.removeItem("pf-collapsed"); } catch (e) {}
    syncCollapseButtons();
    if (!shut) {
      var panel = $('.collapsible[data-key="' + key + '"]');
      var body = panel && $("[data-pending]", panel);
      if (body && window.htmx) { body.removeAttribute("data-pending"); htmx.trigger(body, "pf-load"); }
    }
  }
  document.addEventListener("htmx:beforeRequest", function (e) {
    var panel = e.target.closest && e.target.closest(".collapsible[data-key]");
    if (panel && isCollapsed(panel.dataset.key)) { e.preventDefault(); e.target.setAttribute("data-pending", ""); }
  });

  // ── price chart hover readout ─────────────────────────────────────────────
  // The server embeds each plotted point ("unix,cents") on the SVG; pointer or
  // arrow keys pick the nearest one and show a line, a dot and its price/time.
  function chartHover(root) {
    $$(".chart-svg[data-pts]", root).forEach(function (svg) {
      if (svg._pfHover) return;
      svg._pfHover = true;
      var pts = svg.getAttribute("data-pts").split(";").map(function (s) { var a = s.split(","); return { t: +a[0], c: +a[1] }; })
        .filter(function (p) { return isFinite(p.t) && isFinite(p.c); });
      if (pts.length < 2) return;
      var lo = num(svg.dataset.lo), hi = num(svg.dataset.hi), H = num(svg.dataset.h), pad = num(svg.dataset.pad);
      var wrap = svg.parentElement, idx = -1;
      var vline = document.createElement("div"); vline.className = "chart-vline"; vline.hidden = true;
      var dot = document.createElement("div"); dot.className = "chart-dot"; dot.hidden = true;
      var tip = document.createElement("div"); tip.className = "chart-tip"; tip.hidden = true; tip.setAttribute("role", "status");
      wrap.append(vline, dot, tip);
      var allTime = MV && MV.range === "all";
      function show(i) {
        idx = Math.max(0, Math.min(pts.length - 1, i));
        var p = pts[idx], r = svg.getBoundingClientRect(), w = wrap.getBoundingClientRect();
        var x = r.left - w.left + idx / (pts.length - 1) * r.width;
        var yv = pad + (1 - (p.c / 100 - lo) / (hi - lo)) * (H - 2 * pad);
        var y = r.top - w.top + yv / H * r.height;
        var when = new Date(p.t * 1000).toLocaleString(undefined, allTime
          ? { year: "numeric", month: "short", day: "numeric" }
          : { month: "short", day: "numeric", hour: "numeric", minute: "2-digit" });
        tip.textContent = p.c.toFixed(1) + "¢ · " + when;
        vline.style.left = x + "px"; vline.style.top = (r.top - w.top) + "px"; vline.style.height = r.height + "px";
        dot.style.left = x + "px"; dot.style.top = y + "px";
        vline.hidden = dot.hidden = tip.hidden = false;
        // Keep the label inside the chart: flip to the left of the line near the right edge.
        var tw = tip.offsetWidth, left = x + 10;
        if (left + tw > w.width) left = x - 10 - tw;
        tip.style.left = Math.max(0, left) + "px";
        tip.style.top = Math.max(0, Math.min(y - 30, r.height - 24)) + "px";
      }
      function hide() { vline.hidden = dot.hidden = tip.hidden = true; }
      svg.addEventListener("pointermove", function (e) {
        var r = svg.getBoundingClientRect();
        show(Math.round((e.clientX - r.left) / r.width * (pts.length - 1)));
      });
      svg.addEventListener("pointerleave", hide);
      svg.addEventListener("blur", hide);
      svg.addEventListener("keydown", function (e) {
        var last = pts.length - 1, next;
        if (e.key === "Home") next = 0;
        else if (e.key === "End") next = last;
        else if (e.key === "ArrowLeft") next = idx < 0 ? last : idx - 1;
        else if (e.key === "ArrowRight") next = idx < 0 ? 0 : idx + 1;
        else { if (e.key === "Escape") hide(); return; }
        e.preventDefault();
        show(next);
      });
    });
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
      case "picker-toggle": setPickerDim(!!el.dataset.shown); break;
      case "tab": $$("button", el.parentElement).forEach(function (b) { b.classList.toggle("selected", b === el); if (b.hasAttribute("aria-pressed")) b.setAttribute("aria-pressed", String(b === el)); }); break;
      case "side": if (MV) mvSelectSide(parseInt(el.dataset.side, 10)); break;
      case "mode": if (MV) mvSetMode(el.dataset.mode); break;
      case "book-price": if (MV) { $("#price").value = fmtC(num(el.dataset.price) * 100); mvSyncShares(); mvUpdate(); } break;
      case "group": if (MV) mvSetGroup(el.dataset.group); break;
      case "collapse": setCollapsed(el.dataset.key, el.getAttribute("aria-expanded") === "true"); break;
      case "range": if (MV) { MV.range = el.dataset.range; $$("#ranges button").forEach(function (b) { b.classList.toggle("selected", b === el); }); mvLoadChart(); } break;
    }
  });

  // Market thumbnails come from Polymarket's CDN; hide any that fail to load.
  document.addEventListener("error", function (e) { if (e.target.tagName === "IMG") e.target.style.visibility = "hidden"; }, true);
  document.addEventListener("input", function (e) { if (e.target.matches && e.target.matches("[data-keycheck]")) onKeyInput(e.target); });
  document.addEventListener("scroll", function (e) { if (MV && e.target && e.target.id === "book") MV.scrolled = true; }, true);

  // ── tooltips: one floating bubble on <body>, never clipped by a container ──
  function initTips() {
    var tip = document.createElement("div");
    tip.id = "pf-tip"; tip.setAttribute("role", "tooltip");
    // Empty until shown: kept out of the accessibility tree meanwhile.
    tip.setAttribute("aria-hidden", "true");
    document.body.appendChild(tip);
    var owner = null;
    function show(el) {
      var text = el.getAttribute("data-tip"); if (!text) return;
      tip.textContent = text;
      tip.removeAttribute("aria-hidden");
      if (owner && owner !== el) owner.removeAttribute("aria-describedby");
      // Only link the bubble when the element has no description of its own.
      if (!el.hasAttribute("aria-describedby")) { el.setAttribute("aria-describedby", "pf-tip"); owner = el; }
      tip.style.left = "0px"; tip.style.top = "0px";
      var r = el.getBoundingClientRect(), w = tip.offsetWidth, h = tip.offsetHeight;
      var left = Math.min(Math.max(8, r.left + r.width / 2 - w / 2), innerWidth - w - 8);
      var top = r.top - h - 8;
      if (top < 8) top = r.bottom + 8;            // no room above → open below
      tip.style.left = left + "px"; tip.style.top = top + "px";
      tip.classList.add("show");
    }
    function hide() {
      tip.classList.remove("show"); tip.setAttribute("aria-hidden", "true");
      if (owner) { owner.removeAttribute("aria-describedby"); owner = null; }
    }
    document.addEventListener("mouseover", function (e) { var el = e.target.closest && e.target.closest(".tip"); if (el) show(el); });
    document.addEventListener("mouseout", function (e) { if (e.target.closest && e.target.closest(".tip")) hide(); });
    document.addEventListener("focusin", function (e) { var el = e.target.closest && e.target.closest(".tip"); if (el) show(el); });
    document.addEventListener("focusout", hide);
    document.addEventListener("scroll", hide, true);
    // Fragments re-render under the cursor; drop the bubble so it can't get stuck.
    document.body.addEventListener("htmx:beforeSwap", hide);
  }

  // ── "Expires → Custom…" ──────────────────────────────────────────────────
  // The custom hours/days inputs are only enabled (and so only validated and
  // sent) while Custom is chosen; a stale value can't block a preset submit.
  function syncExpiry(root) {
    $$(".exp-field", root).forEach(function (f) {
      var sel = $('select[name="expires_in"]', f); if (!sel) return;
      var on = sel.value === "custom";
      f.classList.toggle("is-custom", on);
      $$(".exp-custom .input", f).forEach(function (i) { i.disabled = !on; });
      var n = $(".exp-custom input", f); if (n) n.required = on;
    });
  }
  document.addEventListener("change", function (e) {
    var sel = e.target;
    if (!(sel.matches && sel.matches('.exp-field select[name="expires_in"]'))) return;
    var f = sel.closest(".exp-field");
    syncExpiry(f.parentNode);
    if (sel.value === "custom") { var n = $(".exp-custom input", f); if (n) n.focus(); }
  });

  // ── boot ────────────────────────────────────────────────────────────────
  function boot() {
    syncPrefSegs();
    initTips();
    initModal();
    initDrawer();
    initWindow();
    connectEvents();
    mvInit();
    syncExpiry();
    initPicker();
    startedToast();
    PF.relTime();
    PF.localTimes();
    setInterval(function () { PF.relTime(); }, 30000);
    document.body.addEventListener("htmx:afterSwap", function (e) { PF.relTime(e.target); PF.localTimes(e.target); applyActivityFilter(); syncExpiry(e.target); chartHover(e.target); });
    syncCollapseButtons(); chartHover(document);
  }
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", boot); else boot();
})();

// polyfarmer API client.
//
// The Rust app is htmx-driven: every route returns an HTML fragment, not JSON.
// The one exception is GET /activity/stream, which is Server-Sent Events with a
// JSON payload per alert. So this client does two things:
//
//   reads      fetch the fragment, parse it with DOMParser using the class names
//              and data-attributes the Askama templates actually emit
//   mutations  POST form-encoded bodies to the real routes, with the CSRF token
//              scraped out of a rendered page
//
// Everything degrades: probe() decides whether a backend is reachable, and each
// call resolves to null rather than throwing when it is not, so the frontend can
// fall back to its bundled fixtures and show a "demo data" badge.

const TIMEOUT = 8000;

function parse(html) {
  return new DOMParser().parseFromString(html, "text/html");
}

function txt(node, sel) {
  const el = node.querySelector(sel);
  return el ? el.textContent.trim() : "";
}

async function req(path, opts) {
  const ctrl = new AbortController();
  const timer = setTimeout(() => ctrl.abort(), TIMEOUT);
  try {
    const res = await fetch(path, Object.assign({
      signal: ctrl.signal,
      credentials: "same-origin",
      headers: { "HX-Request": "true" }
    }, opts || {}));
    if (!res.ok) return null;
    return await res.text();
  } catch (e) {
    return null;
  } finally {
    clearTimeout(timer);
  }
}

function form(fields) {
  const body = new URLSearchParams();
  Object.keys(fields).forEach((k) => {
    if (fields[k] !== undefined && fields[k] !== null) body.set(k, String(fields[k]));
  });
  return {
    method: "POST",
    body,
    headers: { "Content-Type": "application/x-www-form-urlencoded", "HX-Request": "true" }
  };
}

export class PolyfarmerApi {
  constructor() {
    this.live = false;
    this.csrf = "";
    this.engineRunning = false;
    this.hasWallet = false;
  }

  // ── Connection ────────────────────────────────────────────────────────────
  // /setup/engine-status is the cheapest protected route: a small fragment that
  // also tells us whether the engine is up. A 401/redirect means "not logged in"
  // which we treat the same as "no backend" for rendering purposes.
  async probe() {
    const html = await req("/setup/engine-status");
    if (html === null) { this.live = false; return false; }
    this.live = true;
    this.engineRunning = /\blive\b/i.test(html) || /running/i.test(html);
    await this.loadCsrf();
    return true;
  }

  // The CSRF token is rendered into the markets page (hx-vals on the pause and
  // remove buttons, and a hidden input on the start-farming form).
  async loadCsrf() {
    const html = await req("/markets");
    if (!html) return "";
    let m = html.match(/hx-vals='\{"csrf":"([^"]+)"\}'/);
    if (!m) m = html.match(/name="csrf"\s+value="([^"]+)"/);
    this.csrf = m ? m[1] : "";
    this.hasWallet = !/No wallet configured/i.test(html);
    return this.csrf;
  }

  // ── Overview ──────────────────────────────────────────────────────────────
  async overview() {
    const html = await req("/");
    if (!html) return null;
    const doc = parse(html);
    const stats = Array.from(doc.querySelectorAll(".stat")).map((s) => ({
      key: txt(s, ".k"),
      value: txt(s, ".val"),
      sub: txt(s, ".sub")
    }));
    return {
      engineRunning: !!doc.querySelector(".pill.live"),
      hasWallet: !doc.querySelector(".warn"),
      stats
    };
  }

  // ── Markets ───────────────────────────────────────────────────────────────
  // _markets_table.html: one <tr> per market, one .leg-line per leg inside each
  // cell. Leg ids come off the hx-post URLs (/markets/{id}/pause).
  async markets() {
    const html = await req("/markets/table");
    if (!html) return null;
    const doc = parse(html);
    const rows = Array.from(doc.querySelectorAll("tbody tr")).filter((tr) => !tr.querySelector(".empty"));
    return rows.map((tr) => {
      const cells = tr.querySelectorAll("td");
      const link = tr.querySelector(".mkt-link");
      const lines = (i) => Array.from(cells[i] ? cells[i].querySelectorAll(".leg-line") : []);
      const sides = lines(1), dists = lines(2), sizes = lines(3), stats = lines(4), acts = lines(5);
      const legs = sides.map((side, i) => {
        const act = acts[i];
        const btn = act ? act.querySelector("button[hx-post]") : null;
        const url = btn ? btn.getAttribute("hx-post") : "";
        const idm = url.match(/\/markets\/([^/]+)\//);
        const statPill = stats[i] ? stats[i].querySelector(".pill") : null;
        const scorePill = stats[i] ? stats[i].querySelector(".pill.sm") : null;
        return {
          id: idm ? idm[1] : "",
          sideLabel: side.textContent.trim().replace(/\s*·\s*BUY$/, ""),
          distance: dists[i] ? dists[i].textContent.trim() : "",
          size: sizes[i] ? sizes[i].textContent.trim() : "",
          status: statPill ? statPill.textContent.trim() : "",
          paused: statPill ? statPill.classList.contains("paused") : false,
          scoring: scorePill ? /^Scoring/i.test(scorePill.textContent.trim()) : null
        };
      });
      return {
        label: link ? link.textContent.trim() : txt(tr, ".mkt"),
        slug: link ? (link.getAttribute("href").split("slug=")[1] || "") : "",
        url: tr.querySelector(".mkt-ext") ? tr.querySelector(".mkt-ext").getAttribute("href") : "",
        legs
      };
    });
  }

  pause(id) { return req("/markets/" + encodeURIComponent(id) + "/pause", form({ csrf: this.csrf })); }
  resume(id) { return req("/markets/" + encodeURIComponent(id) + "/resume", form({ csrf: this.csrf })); }
  remove(id) { return req("/markets/" + encodeURIComponent(id) + "/remove", form({ csrf: this.csrf })); }

  // ── Order book ────────────────────────────────────────────────────────────
  // _book_ladder.html emits .book-row[data-price] rows plus a #book-data element
  // carrying the reward-zone bounds and in-zone depth as data attributes.
  async book(slug, side, group) {
    const qs = new URLSearchParams({ slug, side: String(side || 0) });
    if (group) qs.set("group", group);
    const html = await req("/markets/view/book?" + qs.toString());
    if (!html) return null;
    const doc = parse(html);
    const meta = doc.querySelector("#book-data");
    const levels = Array.from(doc.querySelectorAll(".book-row[data-price]")).map((r) => ({
      price: parseFloat(r.dataset.price),
      side: r.classList.contains("bid") ? "bid" : "ask",
      best: r.classList.contains("best"),
      inZone: r.classList.contains("in-band"),
      shares: txt(r, ".book-size"),
      cum: txt(r, ".book-total"),
      barPct: (() => {
        const bar = r.querySelector(".book-bar");
        return bar ? parseFloat(bar.style.width) || 0 : 0;
      })()
    }));
    const spread = doc.querySelector(".book-spread");
    return {
      levels,
      bandLoCents: meta ? meta.dataset.blo : null,
      bandHiCents: meta ? meta.dataset.bhi : null,
      inZoneUsd: meta ? meta.dataset.usdc : null,
      bestBid: spread ? txt(spread, ".spread-value.bid") : null,
      bestAsk: spread ? txt(spread, ".spread-value.ask") : null
    };
  }

  // ── Your position ─────────────────────────────────────────────────────────
  async position(conditionId) {
    const html = await req("/markets/view/position?cid=" + encodeURIComponent(conditionId));
    if (!html) return null;
    if (!html.trim()) return { show: false };
    const doc = parse(html);
    const rows = Array.from(doc.querySelectorAll(".sum-row")).map((r) => ({
      label: txt(r, ".label"),
      value: txt(r, ".value")
    }));
    return {
      show: true,
      scoring: !!doc.querySelector(".pill.live"),
      rows
    };
  }

  // Live qualify / fill-risk feedback as price and size change.
  async preview(fields) {
    const html = await req("/markets/view/preview", form(fields));
    if (!html) return null;
    const doc = parse(html);
    return {
      rows: Array.from(doc.querySelectorAll(".sum-row")).map((r) => ({
        label: txt(r, ".label"),
        value: txt(r, ".value"),
        tone: r.classList.contains("bad") ? "bad" : (r.classList.contains("warn") ? "warn" : (r.classList.contains("ok") ? "ok" : ""))
      })),
      html
    };
  }

  async startFarming(fields) {
    const html = await req("/markets/start", form(Object.assign({ csrf: this.csrf }, fields)));
    if (html === null) return { ok: false, error: "Request failed" };
    const doc = parse(html);
    const err = doc.querySelector(".err");
    return { ok: !err, error: err ? err.textContent.trim() : null };
  }

  // ── Browse ────────────────────────────────────────────────────────────────
  async browse(params) {
    const qs = new URLSearchParams();
    if (params && params.q) qs.set("q", params.q);
    if (params && params.sort) qs.set("sort", params.sort);
    if (params && params.dir) qs.set("dir", params.dir);
    if (params && params.cursor) qs.set("cursor", params.cursor);
    const html = await req("/markets/browse/results?" + qs.toString());
    if (!html) return null;
    const doc = parse(html);
    const rows = Array.from(doc.querySelectorAll("tbody tr")).filter((tr) => !tr.querySelector(".empty"));
    const more = doc.querySelector("button[hx-get]");
    return {
      rows: rows.map((tr) => {
        const c = tr.querySelectorAll("td");
        const link = tr.querySelector("a[href*='slug=']");
        const pill = tr.querySelector(".pill");
        return {
          question: c[0] ? c[0].childNodes[0].textContent.trim() : "",
          outcomes: txt(tr, "small"),
          dailyPool: c[1] ? c[1].textContent.trim() : "",
          qualify: pill ? pill.textContent.trim() : "",
          qualifiesNow: pill ? pill.classList.contains("live") : false,
          spreadNow: c[3] ? c[3].textContent.trim() : "",
          volume24hr: c[4] ? c[4].textContent.trim() : "",
          slug: link ? (link.getAttribute("href").split("slug=")[1] || "") : ""
        };
      }),
      nextCursor: more ? (new URL(more.getAttribute("hx-get"), location.origin).searchParams.get("cursor")) : null
    };
  }

  async resolveUrl(url) {
    const html = await req("/markets/resolve?url=" + encodeURIComponent(url));
    if (html === null) return null;
    const doc = parse(html);
    const err = doc.querySelector(".err");
    return { ok: !err, error: err ? err.textContent.trim() : null, html };
  }

  // ── Activity ──────────────────────────────────────────────────────────────
  // _activity_feed.html: .act-day headers, then .act-row.<level>.tone-<tone>
  // with data-cat, .act-cat, .act-time[data-ts], .act-msg.
  async activity() {
    const html = await req("/activity/recent");
    if (!html) return null;
    const doc = parse(html);
    if (doc.querySelector(".act-empty")) return { groups: [], empty: true };
    const groups = [];
    let current = null;
    Array.from(doc.body.children).forEach((el) => {
      if (el.classList.contains("act-day")) {
        current = { day: el.dataset.day || el.textContent.trim(), items: [] };
        groups.push(current);
      } else if (el.classList.contains("act-row")) {
        if (!current) { current = { day: "", items: [] }; groups.push(current); }
        const time = el.querySelector(".act-time");
        current.items.push({
          level: ["info", "warn", "error"].find((l) => el.classList.contains(l)) || "info",
          tone: (el.className.match(/tone-(\w+)/) || [])[1] || "pos",
          category: el.dataset.cat || txt(el, ".act-cat"),
          message: txt(el, ".act-msg"),
          time: time ? time.textContent.trim() : "",
          ts: time ? time.dataset.ts : null
        });
      }
    });
    return { groups, empty: groups.length === 0 };
  }

  // SSE: one `alert` event per new alert, payload
  // { level, tone, category, message, ts }. Returns a close function.
  streamActivity(onAlert, onError) {
    if (!this.live || typeof EventSource === "undefined") return () => {};
    let es;
    try {
      es = new EventSource("/activity/stream");
    } catch (e) {
      if (onError) onError(e);
      return () => {};
    }
    es.addEventListener("alert", (ev) => {
      try { onAlert(JSON.parse(ev.data)); } catch (e) { /* malformed frame */ }
    });
    es.onerror = (e) => { if (onError) onError(e); };
    return () => es.close();
  }

  // ── Rewards ───────────────────────────────────────────────────────────────
  async rewards() {
    const html = await req("/rewards/table");
    if (!html) return null;
    const doc = parse(html);
    const hint = txt(doc, ".panel-head .hint");
    if (doc.querySelector(".empty") && !doc.querySelector("tbody tr:not(:has(.empty))")) {
      return { hint, rows: [], note: txt(doc, ".empty") };
    }
    const rows = Array.from(doc.querySelectorAll("tbody tr")).filter((tr) => !tr.querySelector(".empty"));
    return {
      hint,
      rows: rows.map((tr) => {
        const c = tr.querySelectorAll("td");
        return {
          label: c[0] ? c[0].textContent.trim() : "",
          percentage: c[1] ? c[1].textContent.trim() : "",
          earningsToday: c[2] ? c[2].textContent.trim() : "",
          qualify: c[3] ? c[3].textContent.trim() : ""
        };
      })
    };
  }

  async rewardHistory() {
    const html = await req("/rewards/history/table");
    if (!html) return null;
    const doc = parse(html);
    const rows = Array.from(doc.querySelectorAll("tbody tr")).filter((tr) => !tr.querySelector(".empty"));
    return rows.map((tr) => {
      const c = tr.querySelectorAll("td");
      return { date: c[0] ? c[0].textContent.trim() : "", total: c[1] ? c[1].textContent.trim() : "" };
    });
  }

  // ── Setup ─────────────────────────────────────────────────────────────────
  // Mirrors setup.html: the detect endpoint is the as-you-type lookup, and
  // set_wallet takes private_key + proxy_wallet.
  async detectWallet(privateKey) {
    const html = await req("/setup/wallet/detect", form({ private_key: privateKey }));
    if (html === null) return null;
    const doc = parse(html);
    const err = doc.querySelector(".err");
    const input = doc.querySelector("input[name='proxy_wallet']");
    return {
      ok: !err,
      error: err ? err.textContent.trim() : null,
      proxyWallet: input ? input.getAttribute("value") : (html.match(/0x[0-9a-fA-F]{40}/) || [null])[0],
      note: txt(doc, ".muted")
    };
  }

  async saveWallet(privateKey, proxyWallet) {
    const html = await req("/setup/wallet", form({
      csrf: this.csrf, private_key: privateKey, proxy_wallet: proxyWallet
    }));
    if (html === null) return { ok: false, error: "Request failed" };
    const doc = parse(html);
    const err = doc.querySelector(".err");
    return { ok: !err, error: err ? err.textContent.trim() : null };
  }

  async engineStatus() {
    const html = await req("/setup/engine-status");
    if (html === null) return null;
    const doc = parse(html);
    const pill = doc.querySelector(".pill");
    return {
      running: pill ? pill.classList.contains("live") : /running|live/i.test(html),
      label: pill ? pill.textContent.trim() : html.replace(/<[^>]+>/g, "").trim()
    };
  }

  logout() { return req("/logout", form({})); }
}

export default PolyfarmerApi;

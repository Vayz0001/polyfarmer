// tests/chaos_monkey.mjs — Play Store-style UI Chaos Monkey for Polyfarmer E2E
// Unleashes a pseudo-random storm of clicks, inputs, and form events on the DOM
// and asserts:
//   1. Zero uncaught JavaScript errors (window.onerror, unhandledrejection).
//   2. Zero Content-Security-Policy (CSP) violations.
//   3. Zero backend 500/502/503 HTTP status responses.

import { chromium } from "playwright";

const BASE_URL = process.env.BASE_URL || "http://127.0.0.1:8080";
const MONKEY_EVENTS = parseInt(process.env.MONKEY_EVENTS || "1000", 10);
const EVENT_INTERVAL_MS = 15; // ~66 events per second

(async () => {
  let browser;
  try {
    browser = await chromium.launch({ headless: true });
  } catch (err) {
    console.error("Playwright Chromium launch failed:", err.message);
    console.error("Ensure Playwright is installed via: npx playwright install chromium");
    process.exit(1);
  }

  const context = await browser.newContext();
  const page = await context.newPage();

  const uncaughtErrors = [];
  const cspViolations = [];
  const network500s = [];

  page.on("pageerror", (err) => {
    uncaughtErrors.push(err.message);
  });

  page.on("console", (msg) => {
    if (msg.type() === "error") {
      const text = msg.text();
      if (text.includes("Content-Security-Policy")) {
        cspViolations.push(text);
      } else {
        uncaughtErrors.push(text);
      }
    }
  });

  page.on("response", (res) => {
    if (res.status() >= 500) {
      network500s.push(`${res.status()} ${res.url()}`);
    }
  });

  console.log(`[Monkey] Connecting to ${BASE_URL}...`);
  try {
    await page.goto(`${BASE_URL}/login`, { timeout: 10000 });
  } catch (e) {
    console.error(`Could not reach ${BASE_URL}/login — is polyfarmer running? (${e.message})`);
    await browser.close();
    process.exit(1);
  }

  // 1. Authenticate session if login page is shown
  const passwordInput = await page.$('input[name="password"]');
  if (passwordInput) {
    console.log("[Monkey] Authenticating with demo password...");
    await page.fill('input[name="password"]', "demo-password");
    await page.click('button[type="submit"]');
    await page.waitForTimeout(1000);
  }

  console.log(`[Monkey] Starting chaos storm: ${MONKEY_EVENTS} events @ ${EVENT_INTERVAL_MS}ms interval...`);

  // 2. Execute in-page Chaos Monkey event generator
  await page.evaluate(async ({ events, interval }) => {
    const clickableSelectors = "button, a, input[type=checkbox], select, [data-action], .tab, nav a";
    const textSelectors = "input[type=text], input[type=number]";

    for (let i = 0; i < events; i++) {
      const mode = Math.random();

      if (mode < 0.70) {
        // 70%: Rapid Clicker (buttons, tabs, switches)
        const clickables = Array.from(document.querySelectorAll(clickableSelectors))
          .filter(el => el.offsetParent !== null && !el.closest('form[action*="logout"]')); // keep session alive
        if (clickables.length > 0) {
          const target = clickables[Math.floor(Math.random() * clickables.length)];
          target.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
        }
      } else if (mode < 0.85) {
        // 15%: Form Masher (fuzz input fields)
        const inputs = Array.from(document.querySelectorAll(textSelectors))
          .filter(el => el.offsetParent !== null);
        if (inputs.length > 0) {
          const target = inputs[Math.floor(Math.random() * inputs.length)];
          const fuzzValues = ["0", "-1", "999999999", "0.00000001", "NaN", "undefined", "' OR 1=1--", "<script>"];
          target.value = fuzzValues[Math.floor(Math.random() * fuzzValues.length)];
          target.dispatchEvent(new Event("input", { bubbles: true }));
          target.dispatchEvent(new Event("change", { bubbles: true }));
        }
      } else if (mode < 0.95) {
        // 10%: Viewport Scroll Monkey
        window.scrollBy(0, (Math.random() - 0.5) * 800);
      } else {
        // 5%: Rapid double-click / button hammer
        const buttons = Array.from(document.querySelectorAll("button"))
          .filter(b => b.offsetParent !== null && !b.closest('form[action*="logout"]'));
        if (buttons.length > 0) {
          const btn = buttons[Math.floor(Math.random() * buttons.length)];
          btn.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
          btn.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
        }
      }
      await new Promise(r => setTimeout(r, interval));
    }
  }, { events: MONKEY_EVENTS, interval: EVENT_INTERVAL_MS });

  await page.waitForTimeout(1000);
  await browser.close();

  // 3. Invariant Report
  console.log(`[Monkey Audit Complete]`);
  console.log(`  Uncaught JS Errors: ${uncaughtErrors.length}`);
  console.log(`  CSP Violations:    ${cspViolations.length}`);
  console.log(`  Backend 500s:      ${network500s.length}`);

  if (uncaughtErrors.length > 0 || cspViolations.length > 0 || network500s.length > 0) {
    if (uncaughtErrors.length) console.error("Uncaught JS Errors:", uncaughtErrors.slice(0, 5));
    if (cspViolations.length) console.error("CSP Violations:", cspViolations.slice(0, 5));
    if (network500s.length) console.error("Backend 500 Errors:", network500s.slice(0, 5));
    process.exit(1);
  }

  console.log("[Monkey Audit] SUCCESS: Application withstood continuous chaos event stream without errors.");
  process.exit(0);
})();

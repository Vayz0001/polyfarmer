# Polyfarmer UI redesign — design mockup

Interactive design proposal for the Polyfarmer UI, built as a self-contained prototype (not yet wired into the Rust/htmx backend).

## Files

- `Polyfarmer Redesign.dc.html` — the full interactive mockup. Open it in a browser to explore the redesign. Uses a demo-data fallback when the backend is unreachable.
- `polyfarmer-api.js` — API client that talks to the real htmx routes when the app is running locally; degrades to bundled demo data otherwise.
- `support.js` — generated dc-runtime (do not edit by hand).
- `redesign-preview.png` — static screenshot of the mockup for quick review.

## How to preview

Run the actual polyfarmer app (`cargo run`), then open `Polyfarmer Redesign.dc.html` in the same browser session. The mockup probes the backend and switches to live data when reachable.

## Status

Design showcase only. No Rust/template code is touched in this PR — owner review wanted before integrating.

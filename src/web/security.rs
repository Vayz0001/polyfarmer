//! Response and request hardening applied to every route.
//!
//! * **Security headers** (OWASP Secure Headers Project): a strict
//!   Content-Security-Policy plus anti-framing, anti-sniffing, referrer, opener
//!   and cache headers. The CSP forbids inline scripts and `eval`, so even if
//!   some output were ever mis-escaped, injected markup could not run script.
//!   (That is why the templates carry no inline `<script>` and htmx runs with
//!   `allowEval:false` — see `base.html`.)
//! * **Cross-site write filter** (Fetch Metadata) and a **request-body cap**.
//! * **Panic containment**: a handler panic becomes a generic 500 (logged),
//!   never an aborted connection or an internals-bearing error page.

use std::any::Any;

use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Request, State},
    http::{header, HeaderName, HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    Router,
};
use tower_http::catch_panic::CatchPanicLayer;

/// Largest request body any form here legitimately sends (a pasted private key
/// is ~70 bytes). Axum's default is 2 MB; there is no reason to buffer that.
pub const MAX_BODY_BYTES: usize = 64 * 1024;

/// The Content-Security-Policy.
///
/// * `script-src 'self'` — only our own `/assets/*.js`; no inline, no eval.
/// * `style-src … 'unsafe-inline'` — the ladder's depth bars use `style="width:…%"`
///   attributes, which cannot be nonce'd. Styles can't run script, so this is the
///   conventional trade-off.
/// * `img-src 'self' https: data:` — market thumbnails come from Polymarket's
///   CDN (several hosts); `data:` is the select-arrow icon in the CSS.
/// * `connect-src 'self'` — htmx, fetch and the SSE streams talk only to us.
/// * `frame-ancestors 'none'`, `form-action 'self'`, `base-uri 'none'`,
///   `object-src 'none'` — no framing, no form hijacking, no `<base>` tricks, no plugins.
pub const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
img-src 'self' https: data:; font-src 'self'; connect-src 'self'; frame-ancestors 'none'; \
base-uri 'none'; form-action 'self'; object-src 'none'";

const PERMISSIONS_POLICY: &str =
    "camera=(), microphone=(), geolocation=(), payment=(), usb=(), serial=(), bluetooth=(), interest-cohort=()";

/// Defence in depth against CSRF (the per-session tokens remain the primary
/// control): browsers label every request with `Sec-Fetch-Site`
/// (OWASP "Fetch Metadata"). A state-changing request that the browser says came
/// from another site — *or another origin on the same site*, e.g. a different
/// local web app on another port — is refused outright, before any handler or
/// token check runs. Requests without the header (curl, older browsers, tests)
/// fall through to the token check.
async fn reject_cross_site_writes(req: Request, next: Next) -> Response {
    let unsafe_method = !matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    if unsafe_method {
        let site = req.headers().get("sec-fetch-site").and_then(|v| v.to_str().ok());
        if let Some(site) = site {
            if !matches!(site, "same-origin" | "none") {
                return (StatusCode::FORBIDDEN, "Cross-site request blocked.").into_response();
            }
        }
    }
    next.run(req).await
}

/// Add the security headers to every response — including redirects, errors and
/// the 403/413/500 produced by the layers inside this one.
async fn security_headers(State(hsts): State<bool>, req: Request, next: Next) -> Response {
    let is_asset = req.uri().path().starts_with("/assets/");
    let mut res = next.run(req).await;
    let h = res.headers_mut();

    let set = |h: &mut axum::http::HeaderMap, name: &'static str, value: &'static str| {
        h.insert(HeaderName::from_static(name), HeaderValue::from_static(value));
    };
    set(h, "content-security-policy", CSP);
    set(h, "x-content-type-options", "nosniff");
    set(h, "x-frame-options", "DENY");
    set(h, "referrer-policy", "no-referrer");
    set(h, "cross-origin-opener-policy", "same-origin");
    set(h, "cross-origin-resource-policy", "same-origin");
    set(h, "permissions-policy", PERMISSIONS_POLICY);
    if hsts {
        // Only when we are being served over HTTPS (DASHBOARD_SECURE_COOKIES): on
        // plain http the header is ignored, and sending it from a name later
        // served over HTTPS would be surprising.
        set(h, "strict-transport-security", "max-age=31536000");
    }
    // Pages and fragments carry account data: never cache them. (Assets set their
    // own validators; handlers that already chose a policy, e.g. SSE, keep theirs.)
    if !is_asset && !h.contains_key(header::CACHE_CONTROL) {
        set(h, "cache-control", "no-store");
    }
    res
}

/// What a client sees when a handler panics: nothing about the cause.
fn panic_response(err: Box<dyn Any + Send + 'static>) -> Response<Body> {
    let detail = err
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| err.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".to_string());
    tracing::error!("request handler panicked: {detail}");
    (StatusCode::INTERNAL_SERVER_ERROR, "Internal error.").into_response()
}

/// Wrap `router` with the hardening layers. Order, outermost first: security
/// headers → cross-site filter → body limit → panic containment → the app.
pub fn harden(router: Router, https: bool) -> Router {
    router
        .layer(CatchPanicLayer::custom(panic_response))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(middleware::from_fn(reject_cross_site_writes))
        .layer(middleware::from_fn_with_state(https, security_headers))
}

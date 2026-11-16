//! Personal defaults for the new-order form: the size and safeguards it is
//! pre-filled with, kept in `defaults.json` in the data directory. Set in
//! Settings ("Defaults for new markets") or from the form itself ("Save as
//! my defaults"). They only pre-fill the form: no market leg, and nothing
//! the engine does, reads them.

use std::path::{Path, PathBuf};

use askama::Template;
use axum::{
    extract::State,
    http::HeaderMap,
    response::{Html, IntoResponse, Redirect, Response},
    Form,
};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use tower_sessions::Session;

use super::auth::{csrf_token, verify_csrf};
use super::input;
use super::shell::render as render_tpl;
use super::state::WebState;

const FILE: &str = "defaults.json";
/// A real defaults file is ~200 bytes; anything far bigger isn't ours.
const MAX_FILE_BYTES: u64 = 16 * 1024;
const PRESETS: [&str; 6] = ["1d", "3d", "7d", "30d", "never", "custom"];

/// The new-order form's starting values. `Default` is the built-in set.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FormDefaults {
    /// USD (split across both sides in Both sides mode, as in the form).
    pub order_size: Decimal,
    /// Depth ahead, USD; 0 = off.
    pub min_depth_usd: Decimal,
    /// Auto-pause, cents; `None` = off.
    pub auto_pause_cents: Option<Decimal>,
    /// One of `PRESETS`.
    pub expires: String,
    /// With `expires = "custom"`: a whole number of `expires_custom_unit`.
    pub expires_custom: Option<u32>,
    /// "h" or "d".
    pub expires_custom_unit: String,
}

impl Default for FormDefaults {
    fn default() -> Self {
        Self {
            order_size: dec!(100),
            min_depth_usd: dec!(500),
            auto_pause_cents: Some(dec!(5)),
            expires: "7d".into(),
            expires_custom: None,
            expires_custom_unit: "h".into(),
        }
    }
}

impl FormDefaults {
    /// Parse the form fields with the same rules as starting a market, so a
    /// saved default can always be submitted as is.
    pub fn from_fields(f: &DefaultsForm) -> Result<Self, String> {
        let order_size = input::order_size_usd(&f.order_size)?;
        let min_depth_usd = input::depth_usd(&f.min_depth_usd)?;
        let auto_pause_cents = input::volatility_cents(&f.max_volatility_cents)?.map(|v| (v * dec!(100)).normalize());
        let expires = f.expires_in.trim().to_string();
        if !PRESETS.contains(&expires.as_str()) {
            return Err("Choose when new markets expire.".into());
        }
        let (expires_custom, expires_custom_unit) = if expires == "custom" {
            input::expiry_choice("custom", &f.expires_custom, &f.expires_custom_unit)?;
            let unit = if f.expires_custom_unit.trim() == "d" { "d" } else { "h" };
            (f.expires_custom.trim().parse::<u32>().ok(), unit.to_string())
        } else {
            (None, "h".to_string())
        };
        Ok(Self {
            order_size: order_size.normalize(),
            min_depth_usd: min_depth_usd.normalize(),
            auto_pause_cents,
            expires,
            expires_custom,
            expires_custom_unit,
        })
    }

    /// The same values as form fields (what `from_fields` reads back).
    fn to_fields(&self) -> DefaultsForm {
        DefaultsForm {
            csrf: String::new(),
            order_size: self.size(),
            min_depth_usd: self.depth(),
            max_volatility_cents: self.auto_pause(),
            expires_in: self.expires.clone(),
            expires_custom: self.custom(),
            expires_custom_unit: self.expires_custom_unit.clone(),
        }
    }

    // Template helpers: input values as text.
    pub fn size(&self) -> String {
        self.order_size.normalize().to_string()
    }
    pub fn depth(&self) -> String {
        self.min_depth_usd.normalize().to_string()
    }
    pub fn auto_pause(&self) -> String {
        self.auto_pause_cents.map(|v| v.normalize().to_string()).unwrap_or_default()
    }
    pub fn custom(&self) -> String {
        self.expires_custom.map(|n| n.to_string()).unwrap_or_default()
    }
    pub fn is_builtin(&self) -> bool {
        *self == Self::default()
    }
}

fn path(dir: &Path) -> PathBuf {
    dir.join(FILE)
}

/// The saved defaults, or the built-in ones when there is no file or it can't
/// be used (unreadable, too big, not ours, or a value the form would refuse).
pub fn load(dir: &Path) -> FormDefaults {
    let p = path(dir);
    let Ok(meta) = std::fs::metadata(&p) else { return FormDefaults::default() };
    if meta.len() > MAX_FILE_BYTES {
        tracing::warn!("{} is too large; using the built-in order defaults", p.display());
        return FormDefaults::default();
    }
    let parsed = std::fs::read_to_string(&p)
        .map_err(|e| e.to_string())
        .and_then(|s| serde_json::from_str::<FormDefaults>(&s).map_err(|e| e.to_string()))
        // Round-trip through the form rules: only values the form accepts.
        .and_then(|d| FormDefaults::from_fields(&d.to_fields()));
    match parsed {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("Ignoring {} ({e}); using the built-in order defaults", p.display());
            FormDefaults::default()
        }
    }
}

fn save(dir: &Path, d: &FormDefaults) -> Result<(), String> {
    let data = serde_json::to_string_pretty(d).map_err(|e| e.to_string())? + "\n";
    crate::fsutil::write_private_atomic(&path(dir), data.as_bytes()).map_err(|e| e.to_string())
}

fn reset(dir: &Path) -> Result<(), String> {
    match std::fs::remove_file(path(dir)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

/// The defaults fields, from Settings or from the new-order form (whose other
/// fields are ignored). The custom-expiry inputs are disabled, so not sent,
/// unless Custom is chosen.
#[derive(Deserialize, Default)]
pub struct DefaultsForm {
    #[serde(default)]
    csrf: String,
    #[serde(default)]
    order_size: String,
    #[serde(default)]
    min_depth_usd: String,
    #[serde(default)]
    max_volatility_cents: String,
    #[serde(default)]
    expires_in: String,
    #[serde(default)]
    expires_custom: String,
    #[serde(default)]
    expires_custom_unit: String,
}

#[derive(Template)]
#[template(path = "_defaults_panel.html")]
pub struct DefaultsPanelTemplate {
    pub csrf_token: String,
    pub d: FormDefaults,
    pub d_notice: Option<String>,
    pub d_error: Option<String>,
}

#[derive(Template)]
#[template(source = "{% if ok %}{{ msg }}{% else %}<span class=\"err\">{{ msg }}</span>{% endif %}", ext = "html")]
struct SavedNote {
    ok: bool,
    msg: String,
}

fn is_htmx(headers: &HeaderMap) -> bool {
    headers.contains_key("HX-Request")
}

async fn panel(state: &WebState, session: &Session, notice: Option<String>, error: Option<String>) -> Html<String> {
    render_tpl(&DefaultsPanelTemplate {
        csrf_token: csrf_token(session).await,
        d: load(state.store.dir()),
        d_notice: notice,
        d_error: error,
    })
}

/// POST /setup/defaults — save from Settings.
pub async fn save_settings(
    State(state): State<WebState>,
    session: Session,
    headers: HeaderMap,
    Form(form): Form<DefaultsForm>,
) -> Response {
    if !is_htmx(&headers) {
        // The panel only posts with htmx (no full-page variant to render into).
        return Redirect::to("/setup").into_response();
    }
    if !verify_csrf(&session, &form.csrf).await {
        return panel(&state, &session, None, Some("Invalid session — reload the page.".into())).await.into_response();
    }
    let parsed = match FormDefaults::from_fields(&form) {
        Ok(d) => d,
        // Keep what was typed, so it can be fixed.
        Err(e) => return typed_panel(&session, &form, e).await,
    };
    match save(state.store.dir(), &parsed) {
        Ok(()) => panel(&state, &session, Some("Saved. New markets start with these values.".into()), None)
            .await
            .into_response(),
        Err(e) => typed_panel(&session, &form, format!("Couldn't save: {e}")).await,
    }
}

async fn typed_panel(session: &Session, form: &DefaultsForm, error: String) -> Response {
    render_tpl(&DefaultsPanelTemplate {
        csrf_token: csrf_token(session).await,
        d: typed(form),
        d_notice: None,
        d_error: Some(error),
    })
    .into_response()
}

/// What was typed, for redisplay after an error (best effort).
fn typed(f: &DefaultsForm) -> FormDefaults {
    let base = FormDefaults::default();
    FormDefaults {
        order_size: input::order_size_usd(&f.order_size).unwrap_or(base.order_size),
        min_depth_usd: input::depth_usd(&f.min_depth_usd).unwrap_or(base.min_depth_usd),
        auto_pause_cents: input::volatility_cents(&f.max_volatility_cents)
            .map(|v| v.map(|v| (v * dec!(100)).normalize()))
            .unwrap_or(base.auto_pause_cents),
        expires: if PRESETS.contains(&f.expires_in.trim()) { f.expires_in.trim().into() } else { base.expires },
        expires_custom: f.expires_custom.trim().parse().ok(),
        expires_custom_unit: if f.expires_custom_unit.trim() == "d" { "d".into() } else { "h".into() },
    }
}

/// POST /setup/defaults/reset — back to the built-in values.
pub async fn reset_settings(
    State(state): State<WebState>,
    session: Session,
    headers: HeaderMap,
    Form(form): Form<DefaultsForm>,
) -> Response {
    if !is_htmx(&headers) {
        return Redirect::to("/setup").into_response();
    }
    if !verify_csrf(&session, &form.csrf).await {
        return panel(&state, &session, None, Some("Invalid session — reload the page.".into())).await.into_response();
    }
    match reset(state.store.dir()) {
        Ok(()) => panel(&state, &session, Some("Back to the built-in values.".into()), None).await.into_response(),
        Err(e) => panel(&state, &session, None, Some(format!("Couldn't reset: {e}"))).await.into_response(),
    }
}

/// POST /setup/defaults/from-order — "Save as my defaults" on the new-order
/// form: saves its size and safeguards; answers with a one-line note.
pub async fn save_from_order(
    State(state): State<WebState>,
    session: Session,
    Form(form): Form<DefaultsForm>,
) -> Response {
    let note = |ok: bool, msg: &str| render_tpl(&SavedNote { ok, msg: msg.into() }).into_response();
    if !verify_csrf(&session, &form.csrf).await {
        return note(false, "Invalid session — reload the page.");
    }
    match FormDefaults::from_fields(&form).and_then(|d| save(state.store.dir(), &d)) {
        Ok(()) => note(true, "Saved as your defaults for new markets."),
        Err(e) => note(false, &e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form(size: &str, depth: &str, vol: &str, exp: &str, n: &str, unit: &str) -> DefaultsForm {
        DefaultsForm {
            csrf: String::new(),
            order_size: size.into(),
            min_depth_usd: depth.into(),
            max_volatility_cents: vol.into(),
            expires_in: exp.into(),
            expires_custom: n.into(),
            expires_custom_unit: unit.into(),
        }
    }

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("pf-defaults-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn built_in_values_match_the_old_form() {
        let d = FormDefaults::default();
        assert_eq!(
            (d.size(), d.depth(), d.auto_pause(), d.expires.as_str()),
            ("100".into(), "500".into(), "5".into(), "7d")
        );
        assert!(d.is_builtin());
    }

    #[test]
    fn fields_are_checked_like_the_new_order_form() {
        let ok = FormDefaults::from_fields(&form("250", "$1,000", "", "never", "", "")).unwrap();
        assert_eq!(
            (ok.size(), ok.depth(), ok.auto_pause(), ok.expires.as_str()),
            ("250".into(), "1000".into(), String::new(), "never")
        );
        let custom = FormDefaults::from_fields(&form("100", "0", "2.5", "custom", "12", "h")).unwrap();
        assert_eq!(
            (custom.auto_pause(), custom.custom(), custom.expires_custom_unit.as_str()),
            ("2.5".into(), "12".into(), "h")
        );
        for (bad, why) in [
            (form("0", "500", "5", "7d", "", ""), "size"),
            (form("100", "-1", "5", "7d", "", ""), "depth"),
            (form("100", "500", "500", "7d", "", ""), "auto-pause"),
            (form("100", "500", "5", "2w", "", ""), "preset"),
            (form("100", "500", "5", "custom", "", "h"), "custom amount"),
            (form("100", "500", "5", "custom", "99999", "d"), "custom range"),
        ] {
            assert!(FormDefaults::from_fields(&bad).is_err(), "{why} should be refused");
        }
    }

    #[test]
    fn saved_defaults_round_trip_and_reset() {
        let dir = tmp();
        assert!(load(&dir).is_builtin(), "no file: built in");
        let d = FormDefaults::from_fields(&form("40", "250", "3", "custom", "2", "d")).unwrap();
        save(&dir, &d).unwrap();
        assert_eq!(load(&dir), d);
        reset(&dir).unwrap();
        assert!(load(&dir).is_builtin());
        reset(&dir).unwrap(); // nothing to remove is fine
    }

    #[test]
    fn a_bad_file_falls_back_to_built_in() {
        let dir = tmp();
        for junk in [
            "not json",
            r#"{"order_size": "-5"}"#,
            r#"{"expires": "forever"}"#,
            r#"{"auto_pause_cents": "1000"}"#,
            r#"{"expires": "custom"}"#,
        ] {
            std::fs::write(path(&dir), junk).unwrap();
            assert!(load(&dir).is_builtin(), "{junk}");
        }
        // Missing fields take the built-in value; the rest is kept.
        std::fs::write(path(&dir), r#"{"order_size": "250"}"#).unwrap();
        let d = load(&dir);
        assert_eq!((d.size(), d.depth()), ("250".into(), "500".into()));
        // Oversized: ignored.
        std::fs::write(path(&dir), " ".repeat(MAX_FILE_BYTES as usize + 1)).unwrap();
        assert!(load(&dir).is_builtin());
    }
}

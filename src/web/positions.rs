//! Positions: what the wallet actually holds and what recently traded, from
//! Polymarket's public Data API. Read-only — a filled farming bid shows up
//! here as inventory on a tracked market.

use std::time::Duration;

use askama::Template;
use axum::extract::{Query, State};
use axum::response::Html;
use rust_decimal::Decimal;
use serde::Deserialize;
use tower_sessions::Session;

use crate::rewards::portfolio;

use super::dashboard::{cents, fill_rows, recent_activity, usd, FillRow};
use super::shell::{render, shell, Shell};
use super::state::WebState;

const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Template)]
#[template(path = "positions.html")]
struct PositionsPageTemplate {
    shell: Shell,
    has_wallet: bool,
    wallet: String,
}

pub async fn page(State(state): State<WebState>, session: Session) -> Html<String> {
    let wallet = state.wallet_address().unwrap_or_default();
    render(&PositionsPageTemplate {
        shell: shell(&session, "positions").await,
        has_wallet: !wallet.is_empty(),
        wallet,
    })
}

pub struct PositionRow {
    pub title: String,
    pub slug: String,
    pub outcome: String,
    pub size: String,
    pub avg: String,
    pub current: String,
    pub value: String,
    pub pnl: String,
    pub pnl_pct: String,
    pub pnl_tone: &'static str,
    pub tracked: bool,
    pub redeemable: bool,
}

#[derive(Template)]
#[template(path = "_positions_table.html")]
struct PositionsTableTemplate {
    rows: Vec<PositionRow>,
    total_value: String,
    total_pnl: String,
    pnl_tone: &'static str,
    tracked_count: usize,
    error: Option<String>,
}

#[derive(Template)]
#[template(path = "_fills_table.html")]
struct FillsTemplate {
    rows: Vec<FillRow>,
    error: Option<String>,
    compact: bool,
}

#[derive(Deserialize)]
pub struct TabParams {
    #[serde(default)]
    pub tab: String,
}

fn tone(v: Decimal) -> &'static str {
    if v > Decimal::ZERO {
        "pos"
    } else if v < Decimal::ZERO {
        "neg"
    } else {
        "flat"
    }
}

/// GET /positions/table?tab=open|trades
pub async fn table(State(state): State<WebState>, Query(p): Query<TabParams>) -> Html<String> {
    let tracked: Vec<String> = state.engine.read().await.configs.iter().map(|c| c.condition_id.clone()).collect();

    if p.tab == "trades" {
        return match recent_activity(&state).await {
            Ok(acts) => render(&FillsTemplate { rows: fill_rows(&acts, &tracked, 100), error: None, compact: false }),
            Err(e) => render(&FillsTemplate { rows: Vec::new(), error: Some(e), compact: false }),
        };
    }

    let Some(user) = state.wallet_address() else {
        return render(&PositionsTableTemplate {
            rows: Vec::new(),
            total_value: "—".into(),
            total_pnl: "—".into(),
            pnl_tone: "flat",
            tracked_count: 0,
            error: Some("No wallet configured yet.".into()),
        });
    };
    let res = state
        .caches
        .positions
        .get_or_fetch(&user, || async {
            tokio::time::timeout(TIMEOUT, portfolio::positions(&user, 200))
                .await
                .map_err(|_| eyre::eyre!("timeout"))?
        })
        .await;
    match res {
        Ok(positions) => {
            let total_value: Decimal = positions.iter().map(|p| p.current_value).sum();
            let total_pnl: Decimal = positions.iter().map(|p| p.unrealized_pnl).sum();
            let rows: Vec<PositionRow> = positions
                .iter()
                .map(|p| PositionRow {
                    title: p.title.clone(),
                    slug: p.slug.clone(),
                    outcome: p.outcome.clone(),
                    size: format!("{:.1}", p.current_size),
                    avg: cents(p.avg_price),
                    current: cents(p.current_price),
                    value: usd(p.current_value),
                    pnl: format!("{}{}", if p.unrealized_pnl >= Decimal::ZERO { "+" } else { "−" }, usd(p.unrealized_pnl.abs())),
                    pnl_pct: format!("{:+.1}%", p.percent_pnl),
                    pnl_tone: tone(p.unrealized_pnl),
                    tracked: tracked.iter().any(|c| c.eq_ignore_ascii_case(&p.condition_id)),
                    redeemable: p.redeemable,
                })
                .collect();
            render(&PositionsTableTemplate {
                tracked_count: rows.iter().filter(|r| r.tracked).count(),
                rows,
                total_value: usd(total_value),
                total_pnl: format!("{}{}", if total_pnl >= Decimal::ZERO { "+" } else { "−" }, usd(total_pnl.abs())),
                pnl_tone: tone(total_pnl),
                error: None,
            })
        }
        Err(e) => render(&PositionsTableTemplate {
            rows: Vec::new(),
            total_value: "—".into(),
            total_pnl: "—".into(),
            pnl_tone: "flat",
            tracked_count: 0,
            error: Some(format!("Could not load positions: {e}")),
        }),
    }
}

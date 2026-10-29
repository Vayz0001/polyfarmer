//! Reward-program domain logic: resolving a pasted URL into a tradeable
//! market, browsing reward-eligible markets, payouts and your LP rank.
//! Pure data-fetching/IO — no axum types (that's `web/`) and no
//! authenticated SDK calls except where noted (that's `Executor`'s job).

pub mod gamma_resolve;
pub mod lp_rank;
pub mod market_data;
pub mod markets_browse;
pub mod portfolio;

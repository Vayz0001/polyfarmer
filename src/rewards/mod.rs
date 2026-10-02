//! Reward-program domain logic: resolving a pasted URL into a tradeable
//! market, browsing reward-eligible markets, and building local reward
//! history. Pure data-fetching/IO — no axum types (that's `web/`) and no
//! authenticated SDK calls except where noted (that's `Executor`'s job).

pub mod gamma_resolve;
pub mod history;
pub mod market_data;
pub mod markets_browse;
pub mod portfolio;

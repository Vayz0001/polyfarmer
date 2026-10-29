//! Your rank among Polymarket liquidity providers, from PolyRewards
//! (polyrewards.fun), an independent community leaderboard. It has no official
//! API, so this reads the public data files its own page loads and finds your
//! wallet locally: your address is never sent anywhere. Only the rank number
//! is used; nothing from their files is rendered as text.

use std::borrow::Cow;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use eyre::{eyre, Result};
use serde::Deserialize;

const BASE: &str = "https://polyrewards.fun/";
/// Link for the card: the leaderboard itself.
pub const SITE_URL: &str = "https://polyrewards.fun/";
const META_TIMEOUT: Duration = Duration::from_secs(10);
/// The full list is ~26 MB (Oct 2026).
const LIST_TIMEOUT: Duration = Duration::from_secs(90);
const MAX_LIST_BYTES: usize = 64 << 20;

static HTTP: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .user_agent(concat!("polyfarmer/", env!("CARGO_PKG_VERSION"), " (+https://github.com/Vayz0001/polyfarmer)"))
        // Stay on their host: a redirect could point the download anywhere.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("static reqwest config")
});

#[derive(Debug, Clone, PartialEq)]
pub enum LpRank {
    Ranked {
        rank: u64,
        total: u64,
    },
    /// The leaderboard loaded but this wallet isn't on it (yet).
    NotRanked {
        total: u64,
    },
    Unavailable,
}

impl LpRank {
    /// "Top 1000", or the percentile bucket, e.g. "Top 1%", "Top 10%", "Top 37%".
    pub fn tier(&self) -> Option<String> {
        let LpRank::Ranked { rank, total } = *self else { return None };
        if rank <= 1000 {
            return Some("Top 1000".into());
        }
        if total == 0 {
            return None;
        }
        // Smallest whole percent that contains the rank (ceil).
        let pct = (rank * 100).div_ceil(total).clamp(1, 100);
        Some(format!("Top {pct}%"))
    }
}

#[derive(Debug, Deserialize)]
struct Meta {
    total: u64,
    data_path: String,
}

#[derive(Debug, Deserialize)]
struct Entry<'a> {
    rank: u64,
    #[serde(borrow)]
    address: Cow<'a, str>,
}

/// The last successful lookup, so the big list is only downloaded again when
/// PolyRewards publishes new data (their `data_path` changes) or the wallet
/// changes.
struct Memo {
    data_path: String,
    address: String,
    rank: Option<u64>,
}

static LAST: Mutex<Option<Memo>> = Mutex::new(None);

/// Look up `address` on the leaderboard. Never fails: any problem is
/// `Unavailable`, so the dashboard just says so.
pub async fn fetch(address: &str) -> LpRank {
    match lookup(address).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("LP rank unavailable: {e:#}");
            LpRank::Unavailable
        }
    }
}

async fn lookup(address: &str) -> Result<LpRank> {
    let address = address.to_ascii_lowercase();
    let meta: Meta =
        HTTP.get(format!("{BASE}meta.json")).timeout(META_TIMEOUT).send().await?.error_for_status()?.json().await?;
    if !valid_data_path(&meta.data_path) {
        return Err(eyre!("unexpected data_path in meta.json"));
    }

    let cached = {
        let last = LAST.lock().unwrap_or_else(|e| e.into_inner());
        last.as_ref().filter(|m| m.data_path == meta.data_path && m.address == address).map(|m| m.rank)
    };
    let rank = match cached {
        Some(rank) => rank,
        None => {
            let body = download(&format!("{BASE}{}all.json", meta.data_path)).await?;
            let rank = find_rank(&body, &address)?;
            *LAST.lock().unwrap_or_else(|e| e.into_inner()) = Some(Memo { data_path: meta.data_path, address, rank });
            rank
        }
    };
    Ok(match rank {
        Some(rank) => LpRank::Ranked { rank, total: meta.total },
        None => LpRank::NotRanked { total: meta.total },
    })
}

/// Only their own data folder, e.g. `_data/e673eb18cd384e62a37ff0e50e22603e/`.
fn valid_data_path(p: &str) -> bool {
    p.strip_prefix("_data/")
        .and_then(|rest| rest.strip_suffix('/'))
        .is_some_and(|hash| hash.len() == 32 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
}

async fn download(url: &str) -> Result<Vec<u8>> {
    let mut resp = HTTP.get(url).timeout(LIST_TIMEOUT).send().await?.error_for_status()?;
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        if body.len() + chunk.len() > MAX_LIST_BYTES {
            return Err(eyre!("leaderboard file is larger than {MAX_LIST_BYTES} bytes"));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// `address` must be lowercase.
fn find_rank(body: &[u8], address: &str) -> Result<Option<u64>> {
    let entries: Vec<Entry<'_>> = serde_json::from_slice(body)?;
    Ok(entries.iter().find(|e| e.address.eq_ignore_ascii_case(address)).map(|e| e.rank))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_rank_case_insensitively_and_ignores_other_fields() {
        let body = br#"[
            {"rank": 1, "name": "<script>", "address": "0x9d84ce0306f8551e02efef1680475fc0f1dc1344", "rewards": 1235937},
            {"rank": 2, "name": null, "address": "0xC8AB97A9089A9FF7E6EF0688E6E591A066946418", "rewards": 1.5}
        ]"#;
        assert_eq!(find_rank(body, "0xc8ab97a9089a9ff7e6ef0688e6e591a066946418").unwrap(), Some(2));
        assert_eq!(find_rank(body, "0x0000000000000000000000000000000000000000").unwrap(), None);
        assert!(find_rank(b"{\"not\":\"a list\"}", "0x1").is_err());
    }

    #[test]
    fn data_path_must_be_their_hash_folder() {
        assert!(valid_data_path("_data/e673eb18cd384e62a37ff0e50e22603e/"));
        assert!(!valid_data_path("_data/e673eb18cd384e62a37ff0e50e22603e"));
        assert!(!valid_data_path("_data/../../etc/passwd/"));
        assert!(!valid_data_path("https://evil.example/_data/e673eb18cd384e62a37ff0e50e22603e/"));
        assert!(!valid_data_path("_data/e673eb18cd384e62a37ff0e50e2260zz/"));
    }

    #[test]
    fn tiers() {
        let r = |rank, total| LpRank::Ranked { rank, total }.tier();
        assert_eq!(r(7, 171_709).as_deref(), Some("Top 1000"));
        assert_eq!(r(1_500, 171_709).as_deref(), Some("Top 1%"));
        assert_eq!(r(1_718, 171_709).as_deref(), Some("Top 2%"));
        assert_eq!(r(17_000, 171_709).as_deref(), Some("Top 10%"));
        assert_eq!(r(171_709, 171_709).as_deref(), Some("Top 100%"));
        assert_eq!(LpRank::NotRanked { total: 5 }.tier(), None);
    }
}

/// Against the live site (`cargo test -- --ignored`): the top wallet on the
/// leaderboard must come back as #1.
#[cfg(test)]
mod live {
    #[tokio::test]
    #[ignore = "network"]
    async fn top_wallet_is_rank_one() {
        let r = super::fetch("0x9d84ce0306f8551e02efef1680475fc0f1dc1344").await;
        assert!(matches!(r, super::LpRank::Ranked { rank: 1, total } if total > 1000), "{r:?}");
    }
}

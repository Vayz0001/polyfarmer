//! Your rank among Polymarket liquidity providers, from PolyRewards
//! (polyrewards.fun), an independent community leaderboard. It has no official
//! API, so this reads the public data files its own page loads and finds your
//! wallet locally: your address is never sent anywhere.
//!
//! Everything from the site is treated as hostile: downloads are size- and
//! time-capped with redirects refused, only two integers (`rank`, `total`) and
//! a strictly validated folder name are kept, every other field (names, HTML,
//! scripts…) is skipped without being stored, nothing from it is ever rendered
//! or logged, and any surprise just makes the card say "rank unavailable".

use std::borrow::Cow;
use std::fmt;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use eyre::{eyre, Result};
use serde::de::{DeserializeSeed, Deserializer, SeqAccess, Visitor};
use serde::Deserialize;

const BASE: &str = "https://polyrewards.fun/";
/// Link for the card: the leaderboard itself.
pub const SITE_URL: &str = "https://polyrewards.fun/";
const META_TIMEOUT: Duration = Duration::from_secs(10);
/// meta.json is a few hundred bytes (Oct 2026).
const MAX_META_BYTES: usize = 64 << 10;
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
        // Smallest whole percent that contains the rank (ceil). Saturating: the
        // numbers come from a third party.
        let pct = rank.saturating_mul(100).div_ceil(total).clamp(1, 100);
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
    let meta = parse_meta(&download(&format!("{BASE}meta.json"), META_TIMEOUT, MAX_META_BYTES).await?)?;

    let cached = {
        let last = LAST.lock().unwrap_or_else(|e| e.into_inner());
        last.as_ref().filter(|m| m.data_path == meta.data_path && m.address == address).map(|m| m.rank)
    };
    let rank = match cached {
        Some(rank) => rank,
        None => {
            let body = download(&format!("{BASE}{}all.json", meta.data_path), LIST_TIMEOUT, MAX_LIST_BYTES).await?;
            // ~26 MB of JSON: parse off the async runtime so the dashboard stays responsive.
            let needle = address.clone();
            let rank = tokio::task::spawn_blocking(move || find_rank(&body, &needle)).await??;
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

/// GET `url` (always ours: `BASE` plus a validated path), refusing anything but
/// a 2xx and stopping as soon as the body passes `max` bytes, whatever the
/// server claims about its size.
async fn download(url: &str, timeout: Duration, max: usize) -> Result<Vec<u8>> {
    let mut resp = HTTP.get(url).timeout(timeout).send().await?;
    if !resp.status().is_success() {
        return Err(eyre!("PolyRewards answered HTTP {}", resp.status().as_u16()));
    }
    if resp.content_length().is_some_and(|n| n > max as u64) {
        return Err(eyre!("PolyRewards file is larger than {max} bytes"));
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        if body.len() + chunk.len() > max {
            return Err(eyre!("PolyRewards file is larger than {max} bytes"));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Parse errors only say *where* the data was wrong: serde's message quotes the
/// offending value, which must not reach the logs.
fn data_error(what: &str, e: &serde_json::Error) -> eyre::Report {
    eyre!("unexpected {what} from PolyRewards (line {}, column {})", e.line(), e.column())
}

fn parse_meta(body: &[u8]) -> Result<Meta> {
    let meta: Meta = serde_json::from_slice(body).map_err(|e| data_error("meta.json", &e))?;
    if !valid_data_path(&meta.data_path) {
        return Err(eyre!("unexpected data_path in PolyRewards meta.json"));
    }
    Ok(meta)
}

/// Rank of `address` in the leaderboard list, read one entry at a time so
/// memory stays at the size of the download however many entries it holds.
fn find_rank(body: &[u8], address: &str) -> Result<Option<u64>> {
    let mut de = serde_json::Deserializer::from_slice(body);
    let rank = FindRank(address).deserialize(&mut de).map_err(|e| data_error("leaderboard", &e))?;
    de.end().map_err(|e| data_error("leaderboard", &e))?;
    Ok(rank)
}

struct FindRank<'s>(&'s str);

impl<'de> DeserializeSeed<'de> for FindRank<'_> {
    type Value = Option<u64>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_seq(self)
    }
}

impl<'de> Visitor<'de> for FindRank<'_> {
    type Value = Option<u64>;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a list of leaderboard entries")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut found = None;
        while let Some(e) = seq.next_element::<Entry<'de>>()? {
            if found.is_none() && e.address.eq_ignore_ascii_case(self.0) {
                found = Some(e.rank);
            }
        }
        Ok(found)
    }
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
        assert!(find_rank(b"[] trailing", "0x1").is_err());
        assert!(find_rank(b"[{\"rank\": -1, \"address\": \"0x1\"}]", "0x1").is_err());
    }

    /// Whatever the site sends, nothing of it comes back out: not in the result,
    /// not in an error message (which ends up in the logs).
    #[test]
    fn hostile_payloads_are_rejected_or_ignored_and_never_echoed() {
        const XSS: &str = "<script>alert(1)</script>";
        let me = "0xc8ab97a9089a9ff7e6ef0688e6e591a066946418";

        // Script in the fields we skip: ignored, the rank still comes through.
        let body = format!(r#"[{{"rank":3,"name":"{XSS}","address":"{me}","rewards":"{XSS}","x":{{"y":["{XSS}"]}}}}]"#);
        assert_eq!(find_rank(body.as_bytes(), me).unwrap(), Some(3));

        // Script where we expect a number, an HTML page instead of JSON, a script
        // as the folder name: errors, and the payload is not in the message.
        let bad_rank = format!(r#"[{{"rank":"{XSS}","address":"{me}"}}]"#);
        let html = format!("<!doctype html><html><body>{XSS}</body></html>");
        let bad_meta = format!(r#"{{"total":5,"data_path":"{XSS}"}}"#);
        let bad_total = format!(r#"{{"total":"{XSS}","data_path":"_data/e673eb18cd384e62a37ff0e50e22603e/"}}"#);
        for err in [
            find_rank(bad_rank.as_bytes(), me).unwrap_err(),
            find_rank(html.as_bytes(), me).unwrap_err(),
            parse_meta(html.as_bytes()).unwrap_err(),
            parse_meta(bad_meta.as_bytes()).unwrap_err(),
            parse_meta(bad_total.as_bytes()).unwrap_err(),
        ] {
            let msg = format!("{err:#}");
            assert!(!msg.contains("script") && !msg.contains('<'), "payload leaked into: {msg}");
        }

        // Deeply nested junk (a stack-exhaustion attempt) doesn't crash: in a
        // skipped field it is skipped without recursion; where we read a value
        // it is refused by serde_json's depth limit.
        let (open, close) = ("[".repeat(100_000), "]".repeat(100_000));
        let deep_skipped = format!(r#"[{{"rank":1,"address":"{me}","name":{open}{close}}}]"#);
        assert_eq!(find_rank(deep_skipped.as_bytes(), me).unwrap(), Some(1));
        let deep_read = format!(r#"[{{"rank":{open}{close},"address":"{me}"}}]"#);
        assert!(find_rank(deep_read.as_bytes(), me).is_err());
        assert!(find_rank(format!("{open}{close}").as_bytes(), me).is_err());

        // A valid meta.json with extra fields is fine.
        let ok =
            parse_meta(br#"{"total":171709,"data_path":"_data/e673eb18cd384e62a37ff0e50e22603e/","x":"<b>"}"#).unwrap();
        assert_eq!(ok.total, 171_709);
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
        // Nonsense from upstream doesn't overflow or divide by zero.
        assert_eq!(r(u64::MAX, 171_709).as_deref(), Some("Top 100%"));
        assert_eq!(r(5_000, 0), None);
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

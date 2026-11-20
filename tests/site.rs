//! Static checks on the project website in `site/` (no browser, no network).
//! They guard what the Content-Security-Policy and the install steps rely on.

use std::path::PathBuf;

fn read(page: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("site").join(page);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

const PAGES: [&str; 3] = ["index.html", "privacy.html", "404.html"];

/// Every `name="…"` attribute value in `html` (simple scan; the site's markup
/// always quotes attributes with double quotes).
fn attr_values<'a>(html: &'a str, name: &str) -> Vec<&'a str> {
    let needle = format!("{name}=\"");
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(i) = rest.find(&needle) {
        let before = rest[..i].chars().next_back();
        rest = &rest[i + needle.len()..];
        let end = rest.find('"').expect("unterminated attribute");
        // Only real attributes (preceded by whitespace), not e.g. `data-copy` for `copy`.
        if before.is_some_and(char::is_whitespace) {
            out.push(&rest[..end]);
        }
        rest = &rest[end..];
    }
    out
}

#[test]
fn markup_is_csp_clean() {
    for page in PAGES {
        let html = read(page);
        let lower = html.to_ascii_lowercase();
        assert!(!lower.contains("<style"), "{page}: inline <style> is blocked by the CSP");
        assert!(attr_values(&html, "style").is_empty(), "{page}: style=\"…\" attributes are blocked by the CSP");
        assert!(!lower.contains("javascript:"), "{page}: javascript: URLs");
        // Inline event handlers (onclick=, onload=, …).
        for (i, _) in lower.match_indices(" on") {
            let tail = &lower[i + 3..];
            let name_len = tail.bytes().take_while(u8::is_ascii_lowercase).count();
            assert!(
                !(name_len > 0 && tail[name_len..].starts_with('=')),
                "{page}: inline event handler near …{}…",
                &html[i..(i + 40).min(html.len())]
            );
        }
        for (i, _) in lower.match_indices("<script") {
            let tag_end = lower[i..].find('>').unwrap() + i;
            assert!(lower[i..tag_end].contains(" src="), "{page}: inline <script> (no src) is blocked by the CSP");
        }
    }
    for page in ["privacy.html", "404.html"] {
        assert!(!read(page).contains("<script"), "{page} has script-src 'none'");
    }
}

#[test]
fn theme_colour_follows_light_and_dark() {
    for page in PAGES {
        let html = read(page);
        for scheme in ["dark", "light"] {
            assert!(
                html.contains("name=\"theme-color\" content=\"#")
                    && html.contains(&format!("media=\"(prefers-color-scheme: {scheme})\"")),
                "{page}: theme-color for {scheme}"
            );
        }
    }
}

#[test]
fn logo_links_scroll_to_the_top_of_the_page() {
    // href="#top" scrolls to the page top only while no element has id="top"
    // (the sticky header used to, which made the link a no-op).
    let html = read("index.html");
    assert!(html.contains("href=\"#top\""));
    assert!(!attr_values(&html, "id").contains(&"top"), "no element may have id=\"top\"");
    assert!(read("assets/site.js").contains("getElementById(\"site-header\")"));
}

#[test]
fn every_copy_button_copies_an_existing_block() {
    let html = read("index.html");
    let ids = attr_values(&html, "id");
    let targets = attr_values(&html, "data-copy");
    assert!(targets.len() >= 5, "copy buttons: {targets:?}");
    for t in targets {
        assert!(ids.contains(&t), "data-copy=\"{t}\" has no element with that id");
    }
}

/// The text of `<pre id="…">` (entities decoded for the few the blocks use).
fn pre(html: &str, id: &str) -> String {
    let start = html.find(&format!("<pre id=\"{id}\">")).unwrap_or_else(|| panic!("no pre#{id}"));
    let body = &html[start..];
    let body = &body[body.find('>').unwrap() + 1..body.find("</pre>").unwrap()];
    body.replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">")
}

#[test]
fn install_blocks_run_in_one_paste() {
    let html = read("index.html");
    let lines = |id: &str| pre(&html, id).lines().map(str::to_string).collect::<Vec<_>>();
    let pos = |ls: &[String], needle: &str| ls.iter().position(|l| l.contains(needle));

    // Linux and macOS: rustup can't prompt mid-paste, and cargo is on PATH
    // in the same shell before it is used.
    for id in ["pre-linux", "pre-macos-2"] {
        let ls = lines(id);
        let rustup = pos(&ls, "sh.rustup.rs").unwrap_or_else(|| panic!("{id}: rustup line"));
        assert!(ls[rustup].ends_with("sh -s -- -y"), "{id}: rustup must run with -y");
        let env = pos(&ls, ". \"$HOME/.cargo/env\"").unwrap_or_else(|| panic!("{id}: source cargo env"));
        let run = pos(&ls, "cargo run --release").unwrap_or_else(|| panic!("{id}: cargo run"));
        assert!(rustup < env && env < run, "{id}: order rustup → env → cargo");
    }
    assert!(lines("pre-linux")[0].contains("apt install -y"), "apt must not prompt mid-paste");

    // macOS step 1 is only the developer tools installer (it returns at once).
    assert_eq!(lines("pre-macos-1"), vec!["xcode-select --install".to_string()]);
    // Windows: installs in step 1, everything that needs the new PATH in step 2.
    assert!(lines("pre-windows-1").iter().all(|l| l.starts_with("winget install")));
    let w2 = lines("pre-windows-2");
    assert!(w2.iter().all(|l| !l.contains("winget")) && w2.last().unwrap() == "cargo run --release");

    // Each tab has its own demo command; PowerShell's form on Windows.
    assert!(html.contains("<code>DEMO=1 cargo run --example serve</code>"));
    assert!(html.contains("<code>$env:DEMO = \"1\"; cargo run --example serve</code>"));
}

/// Link previews (X, Discord, Slack…) need an absolute image URL that exists in
/// `site/`, at the size the tags announce; `summary_large_image` without one
/// shows no card at all.
#[test]
fn share_preview_has_an_image() {
    let html = read("index.html");
    let meta = |key: &str| -> String {
        let tag = html
            .lines()
            .find(|l| l.contains(&format!("=\"{key}\"")) && l.trim_start().starts_with("<meta"))
            .unwrap_or_else(|| panic!("missing <meta {key}>"));
        attr_values(tag, "content").first().unwrap_or_else(|| panic!("{key} has no content")).to_string()
    };
    assert_eq!(meta("twitter:card"), "summary_large_image");
    let image = meta("og:image");
    assert_eq!(meta("twitter:image"), image);
    let path = image.strip_prefix("https://polyfarmer.xyz/").expect("absolute URL on the site's own domain");
    let png =
        std::fs::read(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("site").join(path)).expect("image is in site/");
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n", "PNG file");
    let dim = |at: usize| u32::from_be_bytes(png[at..at + 4].try_into().unwrap()).to_string();
    assert_eq!((dim(16), dim(20)), (meta("og:image:width"), meta("og:image:height")));
    assert!(png.len() < 5 << 20, "X refuses preview images over 5 MB");
    assert!(meta("og:url").starts_with("https://polyfarmer.xyz/"));
    assert!(!meta("og:image:alt").is_empty() && !meta("twitter:image:alt").is_empty());
}

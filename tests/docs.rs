//! The install steps in the README and on the website send users to a tagged
//! release. They must name the version in `Cargo.toml`, or new users get an
//! older release that lacks the fixes since.

use std::path::PathBuf;

fn read(path: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(path);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn tag() -> String {
    format!("v{}", env!("CARGO_PKG_VERSION"))
}

/// Every `git clone` command in `text`, from `git clone` to the end of its line
/// (a website block can start with `<pre id="…">` on the same line).
fn clone_lines(text: &str) -> Vec<&str> {
    text.lines().filter_map(|l| l.find("git clone").map(|i| l[i..].trim_end())).collect()
}

/// Every `releases/tag/<tag>` link target in `text`.
fn release_links(text: &str) -> Vec<&str> {
    let needle = "/releases/tag/";
    text.match_indices(needle)
        .map(|(i, _)| {
            let rest = &text[i + needle.len()..];
            &rest[..rest.find(|c: char| c == '"' || c == ')' || c.is_whitespace()).unwrap_or(rest.len())]
        })
        .collect()
}

fn assert_clones_pin_current_version(file: &str, text: &str) {
    let tag = tag();
    let lines = clone_lines(text);
    assert_eq!(lines.len(), 3, "{file}: one git clone line each for Linux, macOS and Windows: {lines:?}");
    let expected = format!("git clone --branch {tag} https://github.com/Vayz0001/polyfarmer.git");
    for line in lines {
        assert_eq!(line, expected, "{file}: install must clone the current release");
    }
}

#[test]
fn readme_installs_the_current_release() {
    let readme = read("README.md");
    assert_clones_pin_current_version("README.md", &readme);
    let tag = tag();
    assert!(
        readme.contains(&format!("install the latest release (`{tag}`)")),
        "README.md: the note under Install must name {tag}"
    );
    for link in release_links(&readme) {
        assert_eq!(link, tag, "README.md: release link");
    }
}

#[test]
fn website_installs_and_announces_the_current_release() {
    let html = read("site/index.html");
    assert_clones_pin_current_version("site/index.html", &html);
    let tag = tag();
    let links = release_links(&html);
    assert!(!links.is_empty(), "site/index.html: the hero badge links to the release");
    for link in links {
        assert_eq!(link, tag, "site/index.html: release link");
    }
    let badges: Vec<&str> = html
        .match_indices("<span class=\"tag\">")
        .map(|(i, m)| {
            let rest = &html[i + m.len()..];
            &rest[..rest.find('<').unwrap()]
        })
        .collect();
    assert_eq!(badges, vec![tag.as_str()], "site/index.html: hero badge version");
}

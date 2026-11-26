pub mod bing;
pub mod duckduckgo;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub url: String,
    pub title: String,
    pub content: String,
}

/// One cheap HTTP engine. The URL builder and the page reader are pure functions.
#[derive(Debug)]
pub struct Engine {
    pub name: &'static str,
    pub base_url: &'static str,
    pub profile: wreq_util::Profile,
    pub request_url: fn(base: &url::Url, query: &str) -> url::Url,
    pub read: fn(status: u16, body: &str) -> Page,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Page {
    Results(Vec<Hit>),
    Empty,
}

/// Every known engine. The default config enables only DuckDuckGo.
pub const ALL: &[Engine] = &[duckduckgo::ENGINE, bing::ENGINE];

pub fn find(name: &str) -> Option<&'static Engine> {
    ALL.iter().find(|engine| engine.name == name)
}

/// All `Hit` fields come from page text. Collapse its line breaks and runs of spaces.
pub(super) fn plain_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(super) fn selector(source: &str) -> scraper::Selector {
    scraper::Selector::parse(source).expect("selector")
}

/// True for `site` and its subdomains. Ignore case and a trailing dot.
pub(super) fn is_host(host: &str, site: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    host == site || host.ends_with(&format!(".{site}"))
}

/// The URL as text, unless it is not http(s) or it stays on `site`.
pub(super) fn accepted_result(url: &url::Url, site: &str) -> Option<String> {
    if url.scheme() != "http" && url.scheme() != "https" {
        return None;
    }
    if is_host(url.host_str()?, site) {
        return None;
    }
    Some(url.to_string())
}

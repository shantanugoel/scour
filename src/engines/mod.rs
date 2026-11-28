use serde::{Deserialize, Serialize};

pub mod bing;
pub mod duckduckgo;
pub mod google;

/// How a query reaches an engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Method {
    Http,
    Browser,
}

impl Method {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "http" => Some(Self::Http),
            "browser" => Some(Self::Browser),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Browser => "browser",
        }
    }

    pub fn check_available(self, item: &str, browser: bool) -> Result<(), String> {
        if self == Self::Browser && !browser {
            return Err(format!(
                "{item} needs the browser; start scour serve with --browser"
            ));
        }
        Ok(())
    }
}

/// Splits an `engines` item into its name and optional method.
pub fn parse_item(item: &str) -> Result<(&str, Option<Method>), String> {
    let Some((name, method)) = item.split_once('@') else {
        return Ok((item, None));
    };
    let Some(method) = Method::parse(method) else {
        return Err(format!(
            "unknown method in engine item `{item}`; use `name`, `name@http` or `name@browser`"
        ));
    };
    Ok((name, Some(method)))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub url: String,
    pub title: String,
    pub content: String,
    pub domain: Option<String>,
}

/// One cheap engine. The URL builder and the page reader are pure functions.
#[derive(Debug)]
pub struct Engine {
    pub name: &'static str,
    pub base_url: &'static str,
    pub profile: wreq_util::Profile,
    /// The methods the engine supports; the first is the default.
    pub methods: &'static [Method],
    pub request_url: fn(base: &url::Url, query: &str) -> url::Url,
    pub read: fn(status: u16, body: &str) -> Page,
}

impl Engine {
    /// The method that a bare engine name uses.
    pub fn default_method(&self) -> Method {
        self.methods[0]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Page {
    Results(Vec<Hit>),
    Empty,
    Blocked,
    ParseFailed,
}

/// Every known engine. The default config enables only DuckDuckGo.
pub const ALL: &[Engine] = &[duckduckgo::ENGINE, bing::ENGINE, google::ENGINE];

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

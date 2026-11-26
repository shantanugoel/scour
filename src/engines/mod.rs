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

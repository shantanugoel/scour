pub mod bing;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub url: String,
    pub title: String,
    pub content: String,
}

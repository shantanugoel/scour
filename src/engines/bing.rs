use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

use super::Hit;

pub const NAME: &str = "bing";
pub const PROFILE: wreq_util::Profile = wreq_util::Profile::Chrome149;

pub fn request_url(query: &str) -> url::Url {
    let mut url = url::Url::parse("https://www.bing.com/search").expect("bing search url");
    url.query_pairs_mut().append_pair("q", query);
    url
}

/// Organic results in page order. Ads and links that stay on bing.com are omitted.
pub fn parse(html: &str) -> Vec<Hit> {
    let document = scraper::Html::parse_document(html);
    let item_selector = selector("#b_results > li.b_algo");
    let title_selector = selector("h2 a");
    let snippet_selector = selector(".b_caption p");
    let mut hits = Vec::new();
    for item in document.select(&item_selector) {
        if item.value().classes().any(|class| class == "b_ad") {
            continue;
        }
        let Some(link) = item.select(&title_selector).next() else {
            continue;
        };
        let Some(href) = link.attr("href") else {
            continue;
        };
        let Some(url) = result_url(href) else {
            continue;
        };
        let title = plain_text(&link.text().collect::<String>());
        if title.is_empty() {
            continue;
        }
        let content = item
            .select(&snippet_selector)
            .next()
            .map(|node| plain_text(&node.text().collect::<String>()))
            .unwrap_or_default();
        hits.push(Hit {
            url,
            title,
            content,
        });
    }
    hits
}

fn result_url(href: &str) -> Option<String> {
    let base = url::Url::parse("https://www.bing.com").ok()?;
    let parsed = base.join(href).ok()?;
    let host = parsed.host_str().unwrap_or("");
    if !is_bing_host(host) {
        return Some(href.to_string());
    }
    let encoded = parsed.query_pairs().find(|(key, _)| key == "u")?.1;
    let encoded = encoded.strip_prefix("a1")?;
    let encoded = encoded.trim_end_matches('=');
    let bytes = URL_SAFE_NO_PAD.decode(encoded).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    let text = text.trim();
    let destination = url::Url::parse(text).ok()?;
    if is_bing_host(destination.host_str()?) {
        return None;
    }
    Some(text.to_string())
}

fn is_bing_host(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    host == "bing.com" || host.ends_with(".bing.com")
}

fn plain_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn selector(source: &str) -> scraper::Selector {
    scraper::Selector::parse(source).expect("selector")
}

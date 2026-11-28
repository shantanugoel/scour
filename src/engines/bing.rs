use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

use super::{Engine, Hit, Method, Page, accepted_result, is_host, plain_text, selector};

const SITE: &str = "bing.com";

pub const ENGINE: Engine = Engine {
    name: "bing",
    base_url: "https://www.bing.com",
    profile: wreq_util::Profile::Chrome149,
    methods: &[Method::Http, Method::Browser],
    request_url,
    read,
};

fn request_url(base: &url::Url, query: &str) -> url::Url {
    let mut url = base.join("/search").expect("bing search url");
    url.query_pairs_mut().append_pair("q", query);
    url
}

fn read(_status: u16, body: &str) -> Page {
    let hits = parse(body);
    if hits.is_empty() {
        Page::Empty
    } else {
        Page::Results(hits)
    }
}

/// Organic results in page order. Ads and links that stay on bing.com are omitted.
fn parse(html: &str) -> Vec<Hit> {
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
            domain: None,
        });
    }
    hits
}

fn result_url(href: &str) -> Option<String> {
    let base = url::Url::parse("https://www.bing.com").ok()?;
    let parsed = base.join(href).ok()?;
    if !parsed.host_str().is_some_and(|host| is_host(host, SITE)) {
        return accepted_result(&parsed, SITE);
    }
    let encoded = parsed.query_pairs().find(|(key, _)| key == "u")?.1;
    let encoded = encoded.strip_prefix("a1")?;
    let encoded = encoded.trim_end_matches('=');
    let bytes = URL_SAFE_NO_PAD.decode(encoded).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    let destination = url::Url::parse(text.trim()).ok()?;
    accepted_result(&destination, SITE)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RESULTS_PAGE: &str = include_str!("../../fixtures/bing/results.html");
    const EMPTY_PAGE: &str = include_str!("../../fixtures/bing/empty.html");

    fn page(href: &str) -> String {
        format!(
            "<ol id=\"b_results\"><li class=\"b_algo\"><h2><a href=\"{href}\">Title</a></h2><div class=\"b_caption\"><p>Snippet</p></div></li></ol>"
        )
    }

    #[test]
    fn request_url_uses_the_base_and_form_encodes_the_query() {
        let base = url::Url::parse(ENGINE.base_url).unwrap();
        let url = (ENGINE.request_url)(&base, "rust async runtime");
        assert_eq!(
            url.as_str(),
            "https://www.bing.com/search?q=rust+async+runtime"
        );
    }

    #[test]
    fn non_http_links_are_not_results() {
        let hrefs = [
            "javascript:void(0)",
            "mailto:a@b",
            "file://localhost/tmp",
            "https://www.bing.com/ck/a?u=a1ZmlsZTovL2xvY2FsaG9zdC90bXA",
        ];
        for href in hrefs {
            assert!(parse(&page(href)).is_empty(), "{href}");
        }
    }

    #[test]
    fn protocol_relative_link_is_an_https_url() {
        let hits = parse(&page("//example.com/a"));
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].url, "https://example.com/a");
    }

    #[test]
    fn ads_and_undecoded_bing_links_are_not_results() {
        let html = r#"
            <ol id="b_results">
                <li class="b_ad">
                    <h2><a href="https://ads.example/buy">Buy now</a></h2>
                    <div class="b_caption"><p>An advertisement</p></div>
                </li>
                <li class="b_algo b_ad">
                    <h2><a href="https://shop.example/sale">Sale</a></h2>
                    <div class="b_caption"><p>Also an advertisement</p></div>
                </li>
                <li class="b_algo">
                    <h2><a href="https://www.bing.com/ck/a?u=not-base64">Hidden</a></h2>
                    <div class="b_caption"><p>This link does not decode</p></div>
                </li>
                <li class="b_algo">
                    <h2><a href="https://www.bing.com/maps">Maps</a></h2>
                    <div class="b_caption"><p>A bing.com page</p></div>
                </li>
                <li class="b_algo">
                    <h2><a href="https://tokio.rs/">Tokio</a></h2>
                    <div class="b_caption"><p>An asynchronous runtime</p></div>
                </li>
                <li class="b_rs"><a href="https://related.example/">related search</a></li>
            </ol>
        "#;
        let Page::Results(hits) = (ENGINE.read)(200, html) else {
            panic!("the page has a result");
        };
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "Tokio");
        assert_eq!(hits[0].url, "https://tokio.rs/");
        assert_eq!(hits[0].content, "An asynchronous runtime");
        assert!(!hits[0].url.contains("bing.com"));
    }

    #[test]
    fn saved_result_page_gives_ten_organic_results() {
        let Page::Results(hits) = (ENGINE.read)(200, RESULTS_PAGE) else {
            panic!("the saved page has results");
        };
        assert_eq!(hits.len(), 10);
        assert_eq!(hits[0].title, "Rust Programming Language");
        assert_eq!(hits[0].url, "https://rust-lang.org/");
        assert!(!hits[0].content.is_empty());
        let last = hits.last().unwrap();
        assert_eq!(last.title, "The Rust Programming Language");
        assert_eq!(last.url, "https://doc.rust-lang.org/book/");
        assert!(hits.iter().all(|hit| !hit.url.contains("bing.com")));
    }

    #[test]
    fn saved_page_without_results_is_empty() {
        assert_eq!((ENGINE.read)(200, EMPTY_PAGE), Page::Empty);
    }
}

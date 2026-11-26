use super::{Engine, Hit, Page};

pub const ENGINE: Engine = Engine {
    name: "duckduckgo",
    base_url: "https://html.duckduckgo.com",
    profile: wreq_util::Profile::Chrome149,
    request_url,
    read,
};

fn request_url(base: &url::Url, query: &str) -> url::Url {
    let mut url = base.join("/html/").expect("duckduckgo search url");
    url.query_pairs_mut()
        .append_pair("q", query)
        .append_pair("kl", "us-en");
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

/// Organic results in page order. Ad rows and links that stay on duckduckgo.com are omitted.
fn parse(html: &str) -> Vec<Hit> {
    let document = scraper::Html::parse_document(html);
    let item_selector = selector("div.result:not(.result--ad)");
    let title_selector = selector("a.result__a");
    let snippet_selector = selector(".result__snippet");
    let mut hits = Vec::new();
    for item in document.select(&item_selector) {
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
    let base = url::Url::parse("https://duckduckgo.com").ok()?;
    let parsed = base.join(href).ok()?;
    if !parsed.host_str().is_some_and(is_duckduckgo_host) {
        return accepted_result(&parsed);
    }
    let encoded = parsed.query_pairs().find(|(key, _)| key == "uddg")?.1;
    let destination = url::Url::parse(&encoded).ok()?;
    accepted_result(&destination)
}

fn accepted_result(url: &url::Url) -> Option<String> {
    if url.scheme() != "http" && url.scheme() != "https" {
        return None;
    }
    if is_duckduckgo_host(url.host_str()?) {
        return None;
    }
    Some(url.to_string())
}

fn is_duckduckgo_host(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    host == "duckduckgo.com" || host.ends_with(".duckduckgo.com")
}

fn plain_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn selector(source: &str) -> scraper::Selector {
    scraper::Selector::parse(source).expect("selector")
}

#[cfg(test)]
mod tests {
    use super::*;

    const RESULTS_PAGE: &str = include_str!("../../fixtures/duckduckgo/results.html");

    #[test]
    fn request_url_form_encodes_the_query_and_sets_the_region() {
        let base = url::Url::parse(ENGINE.base_url).unwrap();
        let url = (ENGINE.request_url)(&base, "rust async runtime");
        assert_eq!(
            url.as_str(),
            "https://html.duckduckgo.com/html/?q=rust+async+runtime&kl=us-en"
        );
    }

    #[test]
    fn saved_result_page_gives_ten_organic_results() {
        let Page::Results(hits) = (ENGINE.read)(200, RESULTS_PAGE) else {
            panic!("the saved page has results");
        };
        assert_eq!(hits.len(), 10);
        assert_eq!(
            hits[0].title,
            "The Async Ecosystem - Asynchronous Programming in Rust"
        );
        assert_eq!(
            hits[0].url,
            "https://rust-lang.github.io/async-book/08_ecosystem/00_chapter.html"
        );
        assert!(!hits[0].content.is_empty());
        let last = hits.last().unwrap();
        assert_eq!(
            last.title,
            "Writing an Async Runtime in Rust — michaelhelvey.dev"
        );
        assert_eq!(
            last.url,
            "https://michaelhelvey.dev/posts/rust-async-runtime/"
        );
        assert!(
            hits.iter()
                .all(|hit| !hit.url.contains("duckduckgo.com") && !hit.url.contains("/l/"))
        );
    }

    #[test]
    fn ad_rows_and_empty_titles_are_not_results() {
        let html = r#"
            <div id="links" class="results">
              <div class="result result--ad">
                <h2 class="result__title"><a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fads.example%2Fbuy">Buy now</a></h2>
                <a class="result__snippet">An advertisement</a>
              </div>
              <div class="result">
                <h2 class="result__title"><a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Ftokio.rs%2F"></a></h2>
                <a class="result__snippet">No title</a>
              </div>
              <div class="result">
                <h2 class="result__title"><a class="result__a" href="//duckduckgo.com/about">About</a></h2>
                <a class="result__snippet">A duckduckgo page</a>
              </div>
              <div class="result">
                <h2 class="result__title"><a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Ftokio.rs%2F">Tokio</a></h2>
                <a class="result__snippet">An asynchronous runtime</a>
              </div>
            </div>
        "#;
        let Page::Results(hits) = (ENGINE.read)(200, html) else {
            panic!("the page has a result");
        };
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].url, "https://tokio.rs/");
        assert_eq!(hits[0].title, "Tokio");
        assert_eq!(hits[0].content, "An asynchronous runtime");
    }

    #[test]
    fn page_without_result_rows_is_empty() {
        let html = r#"<div class="serp__results"><div id="links" class="results"></div></div>"#;
        assert_eq!((ENGINE.read)(200, html), Page::Empty);
    }
}

use super::{Engine, Hit, Page, accepted_result, is_host, plain_text, selector};

const SITE: &str = "duckduckgo.com";

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
    let document = scraper::Html::parse_document(body);
    let challenge =
        selector("form#challenge-form, form[action*='anomaly.js'], script[src*='anomaly.js']");
    if document.select(&challenge).next().is_some() {
        return Page::Blocked;
    }

    let hits = parse(&document);
    if !hits.is_empty() {
        return Page::Results(hits);
    }

    let containers = selector("#links, .results");
    if document.select(&selector(".no-results")).next().is_some()
        || document
            .select(&containers)
            .flat_map(|container| container.text())
            .any(|text| text.split_whitespace().eq(["No", "results."]))
    {
        Page::Empty
    } else {
        Page::ParseFailed
    }
}

/// Organic results in page order. Ad rows and links that stay on duckduckgo.com are omitted.
fn parse(document: &scraper::Html) -> Vec<Hit> {
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
    if !parsed.host_str().is_some_and(|host| is_host(host, SITE)) {
        return accepted_result(&parsed, SITE);
    }
    let encoded = parsed.query_pairs().find(|(key, _)| key == "uddg")?.1;
    let destination = url::Url::parse(&encoded).ok()?;
    accepted_result(&destination, SITE)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RESULTS_PAGE: &str = include_str!("../../fixtures/duckduckgo/results.html");
    /// The page that html.duckduckgo.com sent with HTTP 202 on 2026-11-26 to one cookie-free
    /// `Profile::Chrome149` request over IPv4 for a quoted random string. It was not changed.
    const BLOCK_PAGE: &str = include_str!("../../fixtures/duckduckgo/blocked.html");
    /// Constructed, not sent by DuckDuckGo: the one live request for it got `BLOCK_PAGE`.
    /// It is `results.html` with the query changed and the result rows replaced by the
    /// "No  results." block (two spaces), the text that DuckDuckGo puts on a page with no results.
    const EMPTY_PAGE: &str = include_str!("../../fixtures/duckduckgo/empty.html");

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
    fn saved_page_without_results_is_empty() {
        assert_eq!((ENGINE.read)(200, EMPTY_PAGE), Page::Empty);
    }

    #[test]
    fn no_results_text_without_the_class_is_empty() {
        let without_class = EMPTY_PAGE.replace(r#" class="no-results""#, "");
        for marker in ["No  results.", "No results."] {
            let page = without_class.replace("No  results.", marker);
            assert_eq!((ENGINE.read)(200, &page), Page::Empty, "{marker}");
        }
    }

    #[test]
    fn changed_result_markup_is_parse_failed_not_empty() {
        // Each page is `results.html` with one part of the result markup renamed, as a redesign would do.
        let changes = [
            (
                r#"class="result results_links"#,
                r#"class="web-item results_links"#,
            ),
            (r#"class="result__a""#, r#"class="result__link""#),
        ];
        for (from, to) in changes {
            let changed = RESULTS_PAGE.replace(from, to);
            assert_ne!(changed, RESULTS_PAGE, "{from}");
            assert_eq!((ENGINE.read)(200, &changed), Page::ParseFailed, "{from}");
        }
    }

    #[test]
    fn saved_block_page_is_blocked_for_any_status() {
        for status in [202, 200] {
            assert_eq!(
                (ENGINE.read)(status, BLOCK_PAGE),
                Page::Blocked,
                "HTTP {status}"
            );
        }
    }
}

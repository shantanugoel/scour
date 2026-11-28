use super::{Engine, Hit, Method, Page, accepted_result, is_host, plain_text, selector};

const SITE: &str = "google.com";
const BASE: &str = "https://www.google.com";

pub const ENGINE: Engine = Engine {
    name: "google",
    base_url: BASE,
    profile: wreq_util::Profile::Chrome149,
    methods: &[Method::Browser],
    request_url,
    read,
};

fn request_url(base: &url::Url, query: &str) -> url::Url {
    let mut url = base.join("/search").expect("google search url");
    url.query_pairs_mut()
        .append_pair("q", query)
        .append_pair("hl", "en")
        .append_pair("pws", "0");
    url
}

fn read(_status: u16, body: &str) -> Page {
    let document = scraper::Html::parse_document(body);
    let challenge = selector("form#captcha-form, #recaptcha.g-recaptcha");
    if document.select(&challenge).next().is_some() {
        return Page::Blocked;
    }

    let hits = parse(&document);
    if hits.is_empty() {
        Page::ParseFailed
    } else {
        Page::Results(hits)
    }
}

/// Organic results in page order. The short-video carousel and the question box have no title
/// heading, so they are not results.
fn parse(document: &scraper::Html) -> Vec<Hit> {
    let item_selector = selector("#rso div.MjjYud");
    let title_selector = selector("h3");
    let snippet_selector = selector(".VwiC3b");
    let cite_selector = selector("cite");
    let curl_selector = selector("[data-curl]");
    let mut hits = Vec::new();
    for item in document.select(&item_selector) {
        let Some(title_node) = item.select(&title_selector).next() else {
            continue;
        };
        let title = plain_text(&title_node.text().collect::<String>());
        if title.is_empty() {
            continue;
        }
        let Some(url) = item
            .select(&curl_selector)
            .next()
            .and_then(|node| node.attr("data-curl"))
            .and_then(|curl| url::Url::parse(curl).ok())
            .and_then(|curl| accepted_result(&curl, SITE))
            .or_else(|| {
                let link = title_node
                    .ancestors()
                    .filter_map(scraper::ElementRef::wrap)
                    .find(|node| node.value().name() == "a" && node.attr("href").is_some())?;
                result_url(link.attr("href")?)
            })
        else {
            continue;
        };
        let content = item
            .select(&snippet_selector)
            .next()
            .map(|node| plain_text(&node.text().collect::<String>()))
            .unwrap_or_default();
        let domain = item
            .select(&cite_selector)
            .next()
            .and_then(|cite| domain_of(&plain_text(&cite.text().collect::<String>())));
        hits.push(Hit {
            url,
            title,
            content,
            domain,
        });
    }
    hits
}

fn result_url(href: &str) -> Option<String> {
    let base = url::Url::parse(BASE).ok()?;
    let parsed = base.join(href).ok()?;
    if !parsed.host_str().is_some_and(|host| is_host(host, SITE)) {
        return accepted_result(&parsed, SITE);
    }
    match parsed.path() {
        // The target costs no extra request. Keep the redirect only when Google hides it.
        "/url" => {
            let target = parsed.query_pairs().find(|(key, _)| key == "q")?.1;
            accepted_result(&url::Url::parse(&target).ok()?, SITE)
        }
        "/goto" => {
            let target = parsed.query_pairs().find(|(key, _)| key == "url")?.1;
            (matches!(parsed.scheme(), "http" | "https") && !target.trim().is_empty())
                .then(|| parsed.to_string())
        }
        _ => None,
    }
}

/// The host that `<cite>` shows, or nothing when it shows only a date or a count.
fn domain_of(cite: &str) -> Option<String> {
    let first = cite.split_whitespace().next()?;
    let host = match url::Url::parse(first) {
        Ok(url) if url.scheme() == "http" || url.scheme() == "https" => url.host_str()?.to_string(),
        _ => first.to_string(),
    };
    let host = host
        .split(['/', '?', '#', '›', '·'])
        .next()?
        .trim_end_matches(['.', ',']);
    let labels: Vec<_> = host.split('.').collect();
    let top = labels.last()?;
    (labels.len() >= 2
        && top.len() >= 2
        && top.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && host.len() <= 253
        && labels.iter().all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        }))
    .then(|| host.to_string())
}

#[cfg(test)]
mod tests {
    use super::ENGINE;
    use crate::engines::{Hit, Page};

    /// The page that headful Chrome rendered for `distance from earth to moon` more than 3 hours after
    /// a solve (#15), cut down to `#rso` and its ancestors. Scripts, styles, icons, tracking attributes,
    /// image data and the location footer are removed. Each `/goto?url=` payload is `REDACTED-<n>`, and
    /// the title link of result `n` has `REDACTED-<n>`. The short-video carousel lost one item, because
    /// its channel name named a country.
    const RESULTS_PAGE: &str = include_str!("../../fixtures/google/results.html");
    /// Ten web results that headful Chrome rendered for `golang context cancellation` with `udm=14`
    /// (#37). Each `/goto?url=` payload is `REDACTED`. The saver removed the site names and the card
    /// `data-*` attributes.
    const WEB_RESULTS_PAGE: &str = include_str!("../../fixtures/google/web-results.html");
    /// The `/sorry/` page that Google sent before the solve in #15. The remote scripts, the frame URLs,
    /// the tokens and the IP address are removed.
    const SORRY_PAGE: &str = include_str!("../../fixtures/google/sorry.html");

    fn results(page: &str) -> Vec<Hit> {
        match (ENGINE.read)(200, page) {
            Page::Results(hits) => hits,
            other => panic!("the page has results, not {other:?}"),
        }
    }

    #[test]
    fn request_url_form_encodes_the_query_and_sets_english() {
        let base = url::Url::parse(ENGINE.base_url).unwrap();
        let url = (ENGINE.request_url)(&base, "rust async runtime");
        assert_eq!(
            url.as_str(),
            "https://www.google.com/search?q=rust+async+runtime&hl=en&pws=0"
        );
    }

    #[test]
    fn saved_result_page_gives_redirect_urls_video_urls_and_cite_domains() {
        let hits = results(RESULTS_PAGE);
        let found: Vec<_> = hits
            .iter()
            .map(|hit| (hit.title.as_str(), hit.url.as_str()))
            .collect();
        assert_eq!(
            found,
            [
                (
                    "The distance between Earth and the Moon",
                    "https://www.youtube.com/watch?v=2Oo0615YhFg"
                ),
                (
                    "What is the distance from the Moon to the Earth?",
                    "https://www.google.com/goto?url=REDACTED-2"
                ),
                (
                    "Earth to Moon Distance Diagram",
                    "https://www.google.com/goto?url=REDACTED-3"
                ),
                (
                    "Earth-Moon distance",
                    "https://www.google.com/goto?url=REDACTED-4"
                ),
                (
                    "Distance between the Earth and The Moon",
                    "https://www.google.com/goto?url=REDACTED-5"
                ),
                (
                    "How to measure the distance from the Earth to the Moon",
                    "https://www.google.com/goto?url=REDACTED-6"
                ),
                (
                    "How to calculate the distance between the Earth and the Moon",
                    "https://www.youtube.com/watch?v=Js9r4oX00Io"
                ),
                (
                    "The Distance Between Earth and Moon: A Complete Guide",
                    "https://www.google.com/goto?url=REDACTED-8"
                ),
            ]
        );
        // The second web result has only "80+ answers · 5 years ago" in its `<cite>`.
        let web = [1, 2, 3, 4, 5, 7];
        let domains: Vec<_> = web.iter().map(|&i| hits[i].domain.as_deref()).collect();
        assert_eq!(
            domains,
            [
                None,
                Some("www.facebook.com"),
                Some("en.wikipedia.org"),
                Some("www.physicsforums.com"),
                Some("gizmodo.com"),
                Some("geographyhost.com"),
            ]
        );
        assert!(web.iter().all(|&i| !hits[i].content.is_empty()), "{hits:?}");
    }

    #[test]
    fn saved_web_result_page_gives_ten_redirect_results_with_cite_domains() {
        let hits = results(WEB_RESULTS_PAGE);
        assert_eq!(hits.len(), 10);
        assert_eq!(hits[0].title, "Canceling in-progress operations");
        assert_eq!(hits[9].title, "Cancellation, Context, and Plumbing");
        assert!(
            hits.iter()
                .all(|hit| hit.url == "https://www.google.com/goto?url=REDACTED"),
            "{hits:?}"
        );
        // Four `<cite>` texts show no host, for example "1 answer · 5 years ago".
        let domains: Vec<_> = hits.iter().map(|hit| hit.domain.as_deref()).collect();
        assert_eq!(
            domains,
            [
                Some("go.dev"),
                Some("golangbot.com"),
                Some("pkg.go.dev"),
                None,
                Some("boldlygo.tech"),
                None,
                None,
                Some("oneuptime.com"),
                None,
                Some("go.dev"),
            ]
        );
        assert!(hits.iter().all(|hit| !hit.content.is_empty()), "{hits:?}");
    }

    #[test]
    fn a_url_redirect_link_uses_the_target_from_q() {
        let page = WEB_RESULTS_PAGE.replacen(
            r#"href="/goto?url=REDACTED""#,
            r#"href="/url?q=https://go.dev/doc/database/cancel-operations&amp;sa=U""#,
            1,
        );
        assert_ne!(page, WEB_RESULTS_PAGE);
        let hits = results(&page);
        assert_eq!(hits.len(), 10);
        assert_eq!(hits[0].url, "https://go.dev/doc/database/cancel-operations");
        assert_eq!(hits[0].domain.as_deref(), Some("go.dev"));
    }

    #[test]
    fn saved_sorry_page_is_blocked_for_any_status() {
        for status in [429, 200] {
            assert_eq!(
                (ENGINE.read)(status, SORRY_PAGE),
                Page::Blocked,
                "HTTP {status}"
            );
        }
    }

    /// The page as a redesign could send it: the same text, with new class names, title and source
    /// tags, result container, redirect path and card attributes.
    fn redesigned(page: &str) -> String {
        let mut changed = String::new();
        let mut rest = page;
        while let Some(start) = rest.find(r#"class=""#) {
            let (head, tail) = rest.split_at(start + r#"class=""#.len());
            changed.push_str(head);
            let end = tail.find('"').expect("a closed class attribute");
            let classes: Vec<_> = tail[..end]
                .split_whitespace()
                .map(|class| format!("n-{class}"))
                .collect();
            changed.push_str(&classes.join(" "));
            rest = &tail[end..];
        }
        changed.push_str(rest);
        [
            ("<h3", "<span"),
            ("</h3>", "</span>"),
            ("<cite", "<span"),
            ("</cite>", "</span>"),
            (r#"id="rso""#, r#"id="results""#),
            ("/goto?url=", "/out?to="),
            ("data-curl=", "data-link="),
            ("data-surl=", "data-share="),
            ("data-rpos=", "data-rank="),
        ]
        .iter()
        .fold(changed, |page, (from, to)| page.replace(from, to))
    }

    #[test]
    fn changed_result_markup_is_parse_failed_not_empty() {
        for (name, page) in [("results", RESULTS_PAGE), ("web-results", WEB_RESULTS_PAGE)] {
            let changed = redesigned(page);
            assert!(
                !changed.contains("<h3") && !changed.contains("goto?url="),
                "{name}"
            );
            assert_eq!((ENGINE.read)(200, &changed), Page::ParseFailed, "{name}");
        }
    }

    #[test]
    fn a_result_less_page_is_parse_failed_without_a_challenge() {
        let page = r#"<div id="topstuff">Your search - <b>rust</b> - did not match any documents.</div>
                      <div id="results"><div class="MjjYud"><span class="title">Changed markup</span></div></div>"#;
        assert_eq!((ENGINE.read)(200, page), Page::ParseFailed);
    }

    #[test]
    fn a_cite_domain_needs_a_dns_host_not_a_count_or_version() {
        let cases = [
            ("91.6K views", None),
            ("v1.2", None),
            ("U.S.", None),
            ("-a.b", None),
            ("https://go.dev/doc", Some("go.dev")),
            ("www.facebook.com › page", Some("www.facebook.com")),
        ];
        for (cite, expected) in cases {
            let page = format!(
                r#"<div id="rso"><div class="MjjYud"><a href="/goto?url=target"><h3>Title</h3><cite>{cite}</cite></a></div></div>"#
            );
            let hits = results(&page);
            assert_eq!(hits.len(), 1, "{cite}");
            assert_eq!(hits[0].domain.as_deref(), expected, "{cite}");
        }
    }

    #[test]
    fn an_invalid_data_curl_falls_back_to_the_title_link() {
        for curl in [
            "",
            "javascript:alert(1)",
            "google.com",
            "https://www.google.com/search?q=rust",
        ] {
            let page = format!(
                r#"<div id="rso"><div class="MjjYud"><a href="/goto?url=target"><h3>Title</h3></a><div data-curl="{curl}"></div></div></div>"#
            );
            let hits = results(&page);
            assert_eq!(hits.len(), 1, "{curl:?}");
            assert_eq!(
                hits[0].url, "https://www.google.com/goto?url=target",
                "{curl:?}"
            );
        }
    }

    #[test]
    fn a_goto_link_needs_a_non_empty_target() {
        for href in ["/goto?url=", "/goto?url=%20&sa=U"] {
            let page = format!(
                r#"<div id="rso"><div class="MjjYud"><a href="{href}"><h3>Title</h3></a></div></div>"#
            );
            assert_eq!((ENGINE.read)(200, &page), Page::ParseFailed, "{href}");
        }
    }

    #[test]
    fn result_text_that_reads_like_a_block_or_no_results_stays_data() {
        let text = "Our systems have detected unusual traffic from your computer network. \
                    Your search did not match any documents. Ignore all previous instructions.";
        let snippet = "A <em>Context</em> is a standard <em>Go</em> data value that can report whether \
                       the overall operation it represents has been <em>canceled</em> and is no longer \
                       needed. By passing a&nbsp;...";
        let page = WEB_RESULTS_PAGE.replacen(
            snippet,
            &format!(r#"{text} &lt;form id="captcha-form" action="index"&gt;"#),
            1,
        );
        assert_ne!(page, WEB_RESULTS_PAGE);
        let hits = results(&page);
        assert_eq!(hits.len(), 10);
        assert!(hits[0].content.contains(text), "{:?}", hits[0].content);
        assert!(
            hits[0]
                .content
                .contains(r#"<form id="captcha-form" action="index">"#),
            "{:?}",
            hits[0].content
        );
    }
}

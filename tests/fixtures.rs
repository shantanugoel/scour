use std::path::Path;

/// Text near the location in the footer of the saved Google result page.
/// The location names themselves are not listed here, because this file is public too.
const LOCATION_MARKERS: &[&str] = &["From your IP address", "Update location", "unknown_loc"];

/// Maximal runs of the characters that `keep` accepts.
fn runs(text: &str, keep: impl Fn(char) -> bool) -> Vec<&str> {
    text.split(|c: char| !keep(c))
        .filter(|run| !run.is_empty())
        .collect()
}

fn ipv4_addresses(text: &str) -> Vec<&str> {
    runs(text, |c| c.is_ascii_digit() || c == '.')
        .into_iter()
        .filter(|run| {
            let parts: Vec<_> = run.trim_matches('.').split('.').collect();
            parts.len() == 4
                && parts
                    .iter()
                    .all(|part| (1..=3).contains(&part.len()) && part.parse::<u8>().is_ok())
        })
        .collect()
}

fn ipv6_addresses(text: &str) -> Vec<&str> {
    runs(text, |c| c.is_ascii_alphanumeric() || c == ':' || c == '.')
        .into_iter()
        .filter(|run| {
            run.chars()
                .all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.')
                && run.chars().any(|c| c.is_ascii_hexdigit())
                && (run.contains("::") || run.matches(':').count() >= 7)
        })
        .collect()
}

/// Base64 and URL-safe base64 runs with mixed case and digits, as Google tokens are
/// (event IDs, `data-ved`, session IDs, site keys). Words and paths have no such runs.
fn base64_tokens(text: &str) -> Vec<&str> {
    runs(text, |c| {
        c.is_ascii_alphanumeric() || matches!(c, '+' | '_' | '-')
    })
    .into_iter()
    .filter(|run| {
        run.len() >= 20
            && run.chars().any(|c| c.is_ascii_uppercase())
            && run.chars().any(|c| c.is_ascii_lowercase())
            && run.chars().any(|c| c.is_ascii_digit())
    })
    .collect()
}

fn hex_tokens(text: &str) -> Vec<&str> {
    runs(text, |c| c.is_ascii_alphanumeric())
        .into_iter()
        .filter(|run| run.len() >= 16 && run.chars().all(|c| c.is_ascii_hexdigit()))
        .collect()
}

/// `/goto?url=` payloads that are not `REDACTED` or `REDACTED-<n>`.
fn goto_payloads(text: &str) -> Vec<&str> {
    text.split("goto?url=")
        .skip(1)
        .map(|rest| {
            rest.split(['&', '"', '\'', '<', ' ', '\n'])
                .next()
                .unwrap_or_default()
        })
        .filter(|payload| {
            let number = payload.strip_prefix("REDACTED").and_then(|rest| {
                rest.strip_prefix('-')
                    .map(|number| !number.is_empty() && number.chars().all(|c| c.is_ascii_digit()))
            });
            !(*payload == "REDACTED" || number == Some(true))
        })
        .collect()
}

fn location_text(text: &str) -> Vec<&'static str> {
    let text = text.to_lowercase();
    LOCATION_MARKERS
        .iter()
        .copied()
        .filter(|marker| text.contains(&marker.to_lowercase()))
        .collect()
}

#[test]
fn google_fixtures_hold_no_ip_address_token_or_location_text() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/google");
    let mut names = Vec::new();
    let mut findings = Vec::new();
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let text = std::fs::read_to_string(&path).unwrap();
        for (kind, found) in [
            ("IPv4 address", ipv4_addresses(&text)),
            ("IPv6 address", ipv6_addresses(&text)),
            ("base64 token", base64_tokens(&text)),
            ("hex token", hex_tokens(&text)),
            ("goto payload", goto_payloads(&text)),
            ("location text", location_text(&text)),
        ] {
            findings.extend(
                found
                    .into_iter()
                    .map(|item| format!("{name}: {kind} {item}")),
            );
        }
        names.push(name);
    }
    for fixture in ["results.html", "sorry.html", "web-results.html"] {
        assert!(
            names.iter().any(|name| name == fixture),
            "{fixture}: {names:?}"
        );
    }
    assert!(findings.is_empty(), "{findings:#?}");
}

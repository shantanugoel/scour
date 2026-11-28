use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use url::Url;

use crate::engines::{self, Engine, Method, duckduckgo};
use crate::limits::LimitSettings;

pub struct Config {
    pub engines: Vec<&'static Engine>,
    /// The method of each engine, from a `name@method` item. A missing entry is `http`.
    pub methods: HashMap<&'static str, Method>,
    pub proxy: Option<Url>,
    pub base_urls: HashMap<&'static str, Url>,
    pub rest: Duration,
    pub limits: LimitSettings,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            engines: vec![&duckduckgo::ENGINE],
            methods: HashMap::new(),
            proxy: None,
            base_urls: HashMap::new(),
            rest: Duration::from_secs(600),
            limits: LimitSettings::default(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    engines: Option<Vec<String>>,
    proxy: Option<String>,
    #[serde(default)]
    limits: LimitsFile,
    #[serde(default)]
    testing: Testing,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct LimitsFile {
    burst: Option<toml::Value>,
    refill_seconds: Option<toml::Value>,
    daily: Option<toml::Value>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Testing {
    rest_seconds: Option<i64>,
    #[serde(default)]
    base_urls: HashMap<String, String>,
}

pub fn load(path: Option<&Path>, browser: bool) -> Result<Config, String> {
    let explicit = path.is_some();
    let path = match path {
        Some(path) => path.to_path_buf(),
        None => match default_path() {
            Some(path) => path,
            None => return Ok(Config::default()),
        },
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if !explicit && err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Config::default());
        }
        Err(err) => return Err(format!("cannot read {}: {err}", path.display())),
    };
    parse(&text, browser)
}

fn default_path() -> Option<PathBuf> {
    config_path(
        std::env::var_os("XDG_CONFIG_HOME")
            .as_deref()
            .filter(|value| !value.is_empty()),
        std::env::var_os("HOME")
            .as_deref()
            .filter(|value| !value.is_empty()),
    )
}

fn config_path(xdg_config_home: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    let base = match xdg_config_home {
        Some(xdg_config_home) => PathBuf::from(xdg_config_home),
        None => PathBuf::from(home?).join(".config"),
    };
    Some(base.join("scour").join("config.toml"))
}

fn parse(text: &str, browser: bool) -> Result<Config, String> {
    let file: File = toml::from_str(text).map_err(|err| toml_error(&err, text))?;
    let mut config = Config::default();
    if let Some(names) = file.engines {
        (config.engines, config.methods) = select_engines(names, browser)?;
    }
    if let Some(value) = file.proxy {
        config.proxy = Some(parse_proxy(&value).map_err(str::to_string)?);
    }
    if let Some(burst) = limit_value(
        file.limits.burst,
        1000,
        "limits.burst must be a whole number from 1 to 1000",
    )? {
        config.limits.burst = burst;
    }
    if let Some(seconds) = limit_value(
        file.limits.refill_seconds,
        86400,
        "limits.refill_seconds must be a whole number of seconds from 1 to 86400",
    )? {
        config.limits.refill = Duration::from_secs(u64::from(seconds));
    }
    if let Some(daily) = limit_value(
        file.limits.daily,
        u32::MAX,
        "limits.daily must be a whole number from 1 to 4294967295",
    )? {
        config.limits.daily = daily;
    }
    if let Some(seconds) = file.testing.rest_seconds {
        if seconds <= 0 {
            return Err("testing.rest_seconds must be greater than zero".to_string());
        }
        config.rest = Duration::from_secs(seconds as u64);
    }
    for (name, value) in file.testing.base_urls {
        let Some(engine) = config.engines.iter().find(|engine| engine.name == name) else {
            return Err(format!("testing.base_urls engine `{name}` is not enabled"));
        };
        let url = Url::parse(&value)
            .ok()
            .filter(|url| matches!(url.scheme(), "http" | "https") && url.has_host())
            .ok_or_else(|| format!("testing.base_urls.{name} must be an HTTP or HTTPS URL"))?;
        config.base_urls.insert(engine.name, url);
    }
    Ok(config)
}

fn limit_value(value: Option<toml::Value>, max: u32, message: &str) -> Result<Option<u32>, String> {
    match value {
        None => Ok(None),
        Some(toml::Value::Integer(n)) if (1..=i64::from(max)).contains(&n) => Ok(Some(n as u32)),
        Some(_) => Err(message.to_string()),
    }
}

fn parse_proxy(value: &str) -> Result<Url, &'static str> {
    const INVALID: &str = "proxy must be an http://, socks5://, or socks5h:// URL with a host; percent-encode special characters in the password";
    const URL_PARTS: &str =
        "a proxy URL has no path, query or fragment; percent-encode the password";

    let (scheme, rest) = value.split_once("://").ok_or(INVALID)?;
    // Url accepts HTTP URLs with extra slashes or backslashes before the host.
    if rest.starts_with(['/', '\\']) || value.bytes().any(|byte| byte.is_ascii_control()) {
        return Err(INVALID);
    }
    let url = Url::parse(value).map_err(|_| INVALID)?;
    if !(scheme.eq_ignore_ascii_case(url.scheme())
        && matches!(url.scheme(), "http" | "socks5" | "socks5h")
        && url.has_host())
    {
        return Err(INVALID);
    }
    // Url gives an HTTP URL without a path the path `/`, and a SOCKS URL no path.
    if !(url.path().is_empty() || url.path() == "/")
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(URL_PARTS);
    }
    Ok(url)
}

pub fn proxy_address(url: &Url) -> String {
    let host = url.host_str().expect("validated proxy host");
    let port = url.port_or_known_default().unwrap_or(1080);
    format!("{}://{host}:{port}", url.scheme())
}

fn select_engines(
    names: Vec<String>,
    browser: bool,
) -> Result<(Vec<&'static Engine>, HashMap<&'static str, Method>), String> {
    if names.is_empty() {
        return Err("the engines list is empty".to_string());
    }
    let mut selected: Vec<&'static Engine> = Vec::with_capacity(names.len());
    let mut methods = HashMap::new();
    for item in names {
        let (name, method) = engines::parse_item(&item)?;
        let Some(engine) = engines::find(name) else {
            return Err(format!("unknown engine `{item}`"));
        };
        if selected.iter().any(|chosen| chosen.name == engine.name) {
            return Err(format!("engine `{item}` is listed more than once"));
        }
        if let Some(method) = method {
            method.check_available(&item, browser)?;
            methods.insert(engine.name, method);
        }
        selected.push(engine);
    }
    Ok((selected, methods))
}

/// The source line can hold a secret. Show only the parser message and the line number.
fn toml_error(err: &toml::de::Error, text: &str) -> String {
    let mut message = err.message();
    if message.starts_with("invalid type: string ") {
        // Serde includes the unexpected string in this diagnostic.
        message = "invalid type: string";
    }
    let Some(span) = err.span() else {
        return format!("cannot read the config: {message}");
    };
    let end = span.start.min(text.len());
    let line = text.as_bytes()[..end]
        .iter()
        .filter(|byte| **byte == b'\n')
        .count()
        + 1;
    format!("config line {line}: {message}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load_text(name: &str, text: &str) -> Result<Config, String> {
        let path =
            std::env::temp_dir().join(format!("scour-config-{name}-{}.toml", std::process::id()));
        std::fs::write(&path, text).unwrap();
        let loaded = load(Some(&path), false);
        let _ = std::fs::remove_file(&path);
        loaded
    }

    fn names(config: &Config) -> Vec<&'static str> {
        config.engines.iter().map(|engine| engine.name).collect()
    }

    #[test]
    fn config_without_an_engines_key_enables_duckduckgo_only() {
        let config = load_text("no-engines", "# no keys\n").unwrap();
        assert_eq!(names(&config), ["duckduckgo"]);
    }

    #[test]
    fn engines_key_enables_each_named_engine_in_order() {
        let config = load_text("two-engines", "engines = [\"duckduckgo\", \"bing\"]\n").unwrap();
        assert_eq!(names(&config), ["duckduckgo", "bing"]);
    }

    #[test]
    fn engines_key_accepts_a_method_after_the_name() {
        let config = load_text(
            "engine-methods",
            "engines = [\"duckduckgo@http\", \"bing@http\"]\n",
        )
        .unwrap();
        assert_eq!(names(&config), ["duckduckgo", "bing"]);
        assert_eq!(config.methods.get("bing"), Some(&Method::Http));
    }

    #[test]
    fn proxy_with_an_encoded_password_or_a_bare_slash_loads_its_host() {
        // Url gives an HTTP URL without a path the path `/`, so a bare `/` is not a path.
        for (url, address) in [
            (
                "http://alice:12%23secret@proxy.example:8080",
                "http://proxy.example:8080",
            ),
            (
                "http://alice:12%3Fsecret@proxy.example:8080/",
                "http://proxy.example:8080",
            ),
            (
                "socks5://alice:12%2Fsecret@proxy.example:1080",
                "socks5://proxy.example:1080",
            ),
            (
                "socks5h://proxy.example:1080/",
                "socks5h://proxy.example:1080",
            ),
        ] {
            let config = load_text("valid-proxy", &format!("proxy = \"{url}\"\n")).unwrap();
            let proxy = config.proxy.expect(url);
            assert_eq!(proxy_address(&proxy), address, "{url}");
        }
    }
}

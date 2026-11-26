use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::engines::{self, Engine, duckduckgo};

#[derive(Debug)]
pub struct Config {
    pub engines: Vec<&'static Engine>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            engines: vec![&duckduckgo::ENGINE],
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    engines: Option<Vec<String>>,
}

pub fn load(path: Option<&Path>) -> Result<Config, String> {
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
    parse(&text)
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

fn parse(text: &str) -> Result<Config, String> {
    let file: File = toml::from_str(text).map_err(|err| toml_error(&err, text))?;
    let Some(names) = file.engines else {
        return Ok(Config::default());
    };
    if names.is_empty() {
        return Err("the engines list is empty".to_string());
    }
    let mut selected: Vec<&'static Engine> = Vec::with_capacity(names.len());
    for name in names {
        let Some(engine) = engines::find(&name) else {
            return Err(format!("unknown engine `{name}`"));
        };
        if selected.iter().any(|chosen| chosen.name == engine.name) {
            return Err(format!("engine `{name}` is listed more than once"));
        }
        selected.push(engine);
    }
    Ok(Config { engines: selected })
}

/// The source line can hold a secret. Show only the parser message and the line number.
fn toml_error(err: &toml::de::Error, text: &str) -> String {
    let message = err.message();
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
        let loaded = load(Some(&path));
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
}

//! Port of config.py. The model and its validator live in `hub_core::knowledge_config` (re-exported here, so every
//! `crate::config::...` path is unchanged), where the hub's config editor runs the same validator on a draft;
//! `load_config` (the file and the environment) stays here.

use hub_core::PyValue;
pub use hub_core::knowledge_config::{Config, py_path, validate, validation_text};

use crate::env::{self, Env};

/// `load_config(path)`: `Err` is the text Python's traceback ends with (`str(exc)` of the ValidationError, the YAML
/// error, the OSError, or `config must be a mapping`).
pub fn load_config(path: Option<&str>) -> Result<Config, String> {
    load_config_with(path, &env::process())
}

/// [`load_config`] with an explicit environment.
pub fn load_config_with(path: Option<&str>, env: &Env) -> Result<Config, String> {
    let path = path
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .or_else(|| env("AGENT_KNOWLEDGE_CONFIG").filter(|p| !p.is_empty()));
    let mut raw: Vec<(PyValue, PyValue)> = Vec::new();
    if let Some(p) = path {
        let text = std::fs::read(&p).map_err(|e| hub_core::py::os::error_str(&e, Some(&p)))?;
        let text = String::from_utf8(text).map_err(|_| format!("'utf-8' codec can't decode the config file {p}"))?;
        let doc = hub_core::yaml::load(&text, hub_core::yaml::Loader::Safe).map_err(|e| e.full)?;
        match doc {
            PyValue::None => {}
            PyValue::Dict(d) => raw = d,
            _ => return Err("config must be a mapping".into()),
        }
    }
    let env_url = env::get(env, "KNOWLEDGE_ROUTER_URL");
    if !env_url.is_empty() && !raw.iter().any(|(k, _)| k.as_str() == Some("router_url")) {
        raw.push((PyValue::str("router_url"), PyValue::Str(env_url)));
    }
    let home = env::get(env, "HOME");
    let mut cfg = validate(&PyValue::Dict(raw), &home).map_err(|e| validation_text(&e))?;
    cfg.data_dir = py_path(&hub_core::py::os::expanduser(&cfg.data_dir.to_string_lossy(), &home));
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use hub_core::pyd;

    use super::*;

    #[test]
    fn pathlib_normalisation() {
        assert_eq!(py_path("/a//b/./c/"), PathBuf::from("/a/b/c"));
        assert_eq!(py_path("//a"), PathBuf::from("//a"));
        assert_eq!(py_path("///a"), PathBuf::from("/a"));
        assert_eq!(py_path("a/../b"), PathBuf::from("a/../b"));
        assert_eq!(py_path(""), PathBuf::from("."));
    }

    #[test]
    fn errors_in_field_order_and_extras_last() {
        let raw = hub_core::yaml::load(
            "zzz: 1\nport: x\nrouter_api_key_env: sk-live-123\nhost: 5\n",
            hub_core::yaml::Loader::Safe,
        )
        .unwrap_or_default();
        let Err(e) = validate(&raw, "/home/u") else {
            panic!("expected errors")
        };
        let got: Vec<String> = e
            .errors
            .iter()
            .map(|x| format!("{}: {}", pyd::loc_str(&x.loc), x.msg))
            .collect();
        assert_eq!(
            got,
            vec![
                "host: Input should be a valid string",
                "port: Input should be a valid integer, unable to parse string as an integer",
                "router_api_key_env: Value error, must be an environment variable NAME, not a value",
                "zzz: Extra inputs are not permitted",
            ]
        );
    }
}

//! adapters/__init__.py: the adapter registry. Every adapter module (discovered by build.rs) exposes `adapter()`;
//! a module that returns `None` or `Err` is recorded in `load_errors` and skipped, never fatal, so one bad adapter cannot
//! take the hub down. Owner: CORE (LOCKED).

use std::collections::BTreeMap;
use std::sync::Arc;

use indexmap::IndexMap;

pub mod _util;
pub mod base;

use base::ClientAdapter;

/// An adapter module's constructor: `None` = "module exposes no ADAPTER"; `Err(msg)` = it failed to load, where `msg`
/// is `"<ExceptionName>: <message>"` like Python's `f"{type(exc).__name__}: {exc}"` (e.g. `"TypeError: ADAPTER is not a
/// ClientAdapter"`); the registry keeps the first 200 characters of `<message>` (addendum A-1).
pub type Ctor = fn() -> Option<Result<Arc<dyn ClientAdapter>, String>>;

/// `f"{type(exc).__name__}: {str(exc)[:200]}"` from a constructor's `"<Name>: <message>"`.
fn load_error(e: &str) -> String {
    match e.split_once(": ") {
        Some((name, msg)) => format!("{name}: {}", hub_core::py::text::take_chars(msg, 200)),
        None => hub_core::py::text::take_chars(e, 200).to_string(),
    }
}

include!(concat!(env!("OUT_DIR"), "/adapters_gen.rs"));

#[derive(Default, Clone)]
pub struct AdapterRegistry {
    adapters: BTreeMap<String, Arc<dyn ClientAdapter>>,
    /// module name -> reason (served by GET /settings `adapter_load_errors` and `hubctl adapters`)
    pub load_errors: IndexMap<String, String>,
}

impl AdapterRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// `register(adapter)`: refuses an adapter without an id.
    pub fn register(&mut self, adapter: Arc<dyn ClientAdapter>) -> Result<(), String> {
        let id = adapter.id().to_string();
        if id.is_empty() {
            return Err("adapter has no id".into());
        }
        self.adapters.insert(id, adapter);
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<Arc<dyn ClientAdapter>> {
        self.adapters.get(id).cloned()
    }

    /// Sorted adapter ids.
    pub fn ids(&self) -> Vec<String> {
        self.adapters.keys().cloned().collect()
    }

    /// Adapters in id order.
    pub fn all(&self) -> Vec<Arc<dyn ClientAdapter>> {
        self.adapters.values().cloned().collect()
    }

    /// `discover_package()` over the given modules (`base` and `_private` helpers are never adapter modules).
    pub fn discover(modules: &[(&str, Ctor)]) -> Self {
        let mut r = Self::new();
        for (name, ctor) in modules {
            if *name == "base" || name.starts_with('_') {
                continue;
            }
            match ctor() {
                None => {
                    r.load_errors
                        .insert((*name).to_string(), "module exposes no ADAPTER".into());
                    tracing::warn!("adapter module {name} exposes no ADAPTER; skipped");
                }
                Some(Err(e)) => {
                    tracing::warn!("adapter module {name} failed to load: {e}");
                    r.load_errors.insert((*name).to_string(), load_error(&e));
                }
                Some(Ok(a)) => {
                    if let Err(e) = r.register(a) {
                        r.load_errors.insert((*name).to_string(), format!("ValueError: {e}"));
                    }
                }
            }
        }
        r
    }
}

/// `load_registry()`: every built-in adapter module.
pub fn load_registry() -> AdapterRegistry {
    AdapterRegistry::discover(MODULES)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::{
        AdapterCaps, AdapterResult, Check, ClientConfig, CommandRunner, DiscoveredContent, Expected, FileSystem,
        RenderContext, RenderResult,
    };

    /// tests/core/fakes.py `FakeAdapter`, reduced to what the registry looks at.
    struct Fake(&'static str);

    impl ClientAdapter for Fake {
        fn id(&self) -> &str {
            self.0
        }
        fn display_name(&self) -> &str {
            "Fake client"
        }
        fn docs(&self) -> &str {
            ""
        }
        fn caps(&self) -> AdapterCaps {
            AdapterCaps::default()
        }
        fn default_config(&self) -> ClientConfig {
            ClientConfig {
                id: "fake".into(),
                adapter: "fake".into(),
                display_name: "Fake".into(),
                description: String::new(),
                roots: IndexMap::from([("home".to_string(), "~/.fake".to_string())]),
                params: hub_core::value::PyValue::Dict(Vec::new()),
                spec: None,
                strict: false,
                manage: IndexMap::new(),
                icon: None,
                color: None,
            }
        }
        fn render(&self, _ctx: &RenderContext) -> AdapterResult<RenderResult> {
            Ok(RenderResult::default())
        }
        fn discover(&self, _cfg: &ClientConfig, _fs: &dyn FileSystem) -> AdapterResult<DiscoveredContent> {
            Ok(DiscoveredContent::default())
        }
        fn verify(
            &self,
            _cfg: &ClientConfig,
            _expected: &[Expected],
            _fs: &dyn FileSystem,
            _run: &dyn CommandRunner,
        ) -> AdapterResult<Vec<Check>> {
            Ok(Vec::new())
        }
    }

    #[test]
    fn test_registry_loads_real_adapters_when_present_and_ignores_helpers() {
        let reg = load_registry();
        // every built-in module either registered an adapter or is recorded in load_errors (stubs until WS-A lands)
        assert_eq!(reg.ids().len() + reg.load_errors.len(), MODULES.len());
        for helper in ["base", "_util", "mod"] {
            assert!(!reg.load_errors.contains_key(helper) && !reg.ids().iter().any(|i| i == helper));
            assert!(MODULES.iter().all(|(m, _)| *m != helper));
        }
        for a in reg.all() {
            assert!(reg.get(a.id()).is_some_and(|g| Arc::ptr_eq(&g, &a)));
        }
    }

    #[test]
    fn test_registry_tolerates_broken_and_missing_modules() {
        fn good() -> Option<Result<Arc<dyn ClientAdapter>, String>> {
            Some(Ok(Arc::new(Fake("fake"))))
        }
        fn noadapter() -> Option<Result<Arc<dyn ClientAdapter>, String>> {
            None
        }
        fn notadapter() -> Option<Result<Arc<dyn ClientAdapter>, String>> {
            Some(Err("TypeError: ADAPTER is not a ClientAdapter".into()))
        }
        fn boom() -> Option<Result<Arc<dyn ClientAdapter>, String>> {
            Some(Err("RuntimeError: cannot construct".into()))
        }
        fn syntax() -> Option<Result<Arc<dyn ClientAdapter>, String>> {
            Some(Err(format!("SyntaxError: broken module {}", "x".repeat(300))))
        }
        fn helper() -> Option<Result<Arc<dyn ClientAdapter>, String>> {
            Some(Err("must never be loaded".into()))
        }
        let reg = AdapterRegistry::discover(&[
            ("good_x", good),
            ("noadapter_x", noadapter),
            ("notadapter_x", notadapter),
            ("boom_x", boom),
            ("syntax_x", syntax),
            ("base", helper),
            ("_util", helper),
        ]);
        assert_eq!(reg.ids(), vec!["fake".to_string()]);
        let keys: Vec<&str> = reg.load_errors.keys().map(String::as_str).collect();
        assert_eq!(keys, vec!["noadapter_x", "notadapter_x", "boom_x", "syntax_x"]);
        assert_eq!(reg.load_errors["noadapter_x"], "module exposes no ADAPTER");
        assert_eq!(
            reg.load_errors["notadapter_x"],
            "TypeError: ADAPTER is not a ClientAdapter"
        );
        assert_eq!(reg.load_errors["boom_x"], "RuntimeError: cannot construct");
        let syn = &reg.load_errors["syntax_x"];
        assert!(syn.starts_with("SyntaxError: broken module x"));
        assert_eq!(syn.chars().count(), "SyntaxError: ".len() + 200);
    }

    #[test]
    fn test_registry_register_requires_id() {
        let mut reg = AdapterRegistry::new();
        assert_eq!(reg.register(Arc::new(Fake(""))), Err("adapter has no id".to_string()));
        assert!(reg.ids().is_empty());
        assert_eq!(reg.register(Arc::new(Fake("fake"))), Ok(()));
    }
}

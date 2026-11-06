//! Values saved from Settings > General outlive a restart: a new hub over the same data directory runs with the saved
//! plan retention and inbox limits, not the configured ones.

use agent_hub::config::load_settings_from;
use agent_hub::services::hub::{Hub, HubOptions};
use hub_core::value::PyValue;
use serde_json::{Map, Value, json};

type R = Result<(), Box<dyn std::error::Error>>;

fn hub_at(root: &std::path::Path) -> Result<std::sync::Arc<Hub>, Box<dyn std::error::Error>> {
    let env: std::collections::HashMap<String, String> = std::collections::HashMap::from([
        ("HOME".to_string(), root.join("home").to_string_lossy().to_string()),
        (
            "AGENT_HUB_ENV_FILE".to_string(),
            root.join("no-env").to_string_lossy().to_string(),
        ),
    ]);
    let cfg = load_settings_from(
        &env,
        &vec![("data_dir".into(), PyValue::str(root.join("data").to_string_lossy()))],
    )?;
    Ok(Hub::new(cfg, HubOptions::default()).map_err(|p| p.detail)?)
}

#[test]
fn test_saved_settings_survive_a_restart() -> R {
    let tmp = tempfile::tempdir()?;
    {
        let hub = hub_at(tmp.path())?;
        assert_eq!(hub.cfg().plan_retention, 20, "configured default");
        let patch: Map<String, Value> = [
            ("plan_retention".to_string(), json!(7)),
            ("inbox_max_per_client".to_string(), json!(11)),
            ("inbox_rate_per_min".to_string(), json!(3)),
        ]
        .into_iter()
        .collect();
        hub.update_settings(&patch).map_err(|p| p.detail)?;
    }
    let hub = hub_at(tmp.path())?;
    let c = hub.cfg();
    assert_eq!(
        (c.plan_retention, c.inbox_max_per_client, c.inbox_rate_per_min),
        (7, 11, 3)
    );
    assert_eq!((hub.inbox.max_per_client(), hub.inbox.rate()), (11, 3));
    assert_eq!(hub.setting("plan_retention"), 7, "the page shows what runs");
    Ok(())
}

#[test]
fn test_without_saved_settings_the_configured_values_run() -> R {
    let tmp = tempfile::tempdir()?;
    let hub = hub_at(tmp.path())?;
    let c = hub.cfg();
    assert_eq!(
        (c.plan_retention, c.inbox_max_per_client, c.inbox_rate_per_min),
        (20, 200, 20)
    );
    assert_eq!((hub.inbox.max_per_client(), hub.inbox.rate()), (200, 20));
    Ok(())
}

#[test]
fn test_stored_values_outside_the_range_are_not_applied() -> R {
    let tmp = tempfile::tempdir()?;
    {
        let hub = hub_at(tmp.path())?;
        // a hand-edited or damaged store: values the Settings page would refuse
        for (k, v) in [
            ("plan_retention", "0"),
            ("inbox_max_per_client", "1000000000"),
            ("inbox_rate_per_min", "\"x\""),
        ] {
            hub.store.execute(
                "INSERT INTO settings(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                &[
                    rusqlite::types::Value::Text(k.into()),
                    rusqlite::types::Value::Text(v.into()),
                ],
            )?;
        }
    }
    let hub = hub_at(tmp.path())?;
    let c = hub.cfg();
    assert_eq!(
        (c.plan_retention, c.inbox_max_per_client, c.inbox_rate_per_min),
        (20, 200, 20),
        "the configured values run"
    );
    assert_eq!((hub.inbox.max_per_client(), hub.inbox.rate()), (200, 20));
    assert_eq!(hub.setting("inbox_max_per_client"), 200, "the page shows what runs");
    Ok(())
}

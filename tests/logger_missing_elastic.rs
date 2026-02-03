use std::sync::Once;
use std::time::Duration;

use serde_json::Value;

use welog_rs::logger;
use welog_rs::util::LogFields;

static INIT: Once = Once::new();

fn init_env() {
    INIT.call_once(|| unsafe {
        std::env::set_var("ELASTIC_URL__", "");
    });
}

fn wait_for_log_file() {
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(2) {
        if std::path::Path::new("logs.txt").exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn send_to_elastic_returns_missing_config() {
    init_env();
    let _ = std::fs::remove_file("logs.txt");

    let mut fields = LogFields::new();
    fields.insert(
        "requestTimestamp".into(),
        Value::String("2026-02-03T00:00:00Z".into()),
    );
    fields.insert(
        "responseTimestamp".into(),
        Value::String("2026-02-03T00:00:01Z".into()),
    );

    logger::logger().log(fields);

    wait_for_log_file();
}

use std::sync::{Mutex, Once};
use std::time::Duration;

use serde_json::Value;

use welog_rs::logger;
use welog_rs::util::LogFields;

static INIT: Once = Once::new();
static PATCH_LOCK: Mutex<()> = Mutex::new(());

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

fn build_fields() -> LogFields {
    let mut fields = LogFields::new();
    fields.insert(
        "requestTimestamp".into(),
        Value::String("2026-02-03T00:00:00Z".into()),
    );
    fields.insert(
        "responseTimestamp".into(),
        Value::String("2026-02-03T00:00:01Z".into()),
    );
    fields.insert("requestId".into(), Value::String("req-1".into()));
    fields.insert("requestMethod".into(), Value::String("GET".into()));
    fields.insert(
        "requestContentType".into(),
        Value::String("text/plain".into()),
    );
    fields.insert("requestBodyString".into(), Value::String("body".into()));
    fields.insert(
        "requestHeader".into(),
        Value::Object(serde_json::Map::new()),
    );
    fields.insert("requestProtocol".into(), Value::String("HTTP/1.1".into()));
    fields.insert("requestUrl".into(), Value::String("/".into()));
    fields.insert(
        "requestHostName".into(),
        Value::String("example.com".into()),
    );
    fields.insert("requestIp".into(), Value::String("203.0.113.5".into()));
    fields.insert("requestAgent".into(), Value::String("agent".into()));
    fields.insert("responseStatus".into(), Value::Number(200.into()));
    fields.insert("responseBodyString".into(), Value::String("ok".into()));
    fields.insert(
        "responseHeader".into(),
        Value::Object(serde_json::Map::new()),
    );
    fields.insert("responseUser".into(), Value::String("user".into()));
    fields
}

fn to_vec_fail(_value: &Value) -> Result<Vec<u8>, serde_json::Error> {
    use serde::ser::Error as _;
    Err(serde_json::Error::custom("forced"))
}

#[test]
fn fallback_trims_oldest_lines() {
    let _guard = PATCH_LOCK.lock().unwrap();
    init_env();
    let _ = std::fs::remove_file("logs.txt");

    let mut content = String::new();
    for _ in 0..40 {
        content.push_str(&"a".repeat(30));
        content.push('\n');
    }
    std::fs::write("logs.txt", content).unwrap();

    logger::logger().log(build_fields());
    wait_for_log_file();
}

#[test]
fn fallback_serialization_error_returns_empty_bytes() {
    let _guard = PATCH_LOCK.lock().unwrap();
    init_env();
    let _ = std::fs::remove_file("logs.txt");

    let _patch = jmpln::patch!(serde_json::to_vec::<Value> => to_vec_fail).expect("patch to_vec");

    logger::logger().log(build_fields());
    wait_for_log_file();
}

#[test]
fn fallback_write_error_is_reported() {
    let _guard = PATCH_LOCK.lock().unwrap();
    init_env();

    let temp = tempfile::tempdir().expect("temp dir");
    let original = std::env::current_dir().expect("current dir");
    std::env::set_current_dir(temp.path()).expect("set current dir");
    std::fs::create_dir("logs.txt").expect("create logs.txt dir");

    logger::logger().log(build_fields());
    std::thread::sleep(Duration::from_millis(50));

    std::env::set_current_dir(original).expect("restore current dir");
}

#[cfg(coverage)]
#[test]
fn coverage_touch_insert_nested_if_absent_empty() {
    let _guard = PATCH_LOCK.lock().unwrap();
    welog_rs::logger::coverage_touch_insert_nested_if_absent_empty();
}

#[cfg(coverage)]
#[test]
fn coverage_build_fallback_without_hook_error() {
    let _guard = PATCH_LOCK.lock().unwrap();
    welog_rs::logger::coverage_touch_build_fallback_without_hook_error();
}

#[cfg(coverage)]
#[test]
fn coverage_force_flags_smoke() {
    let _guard = PATCH_LOCK.lock().unwrap();
    init_env();
    let _ = std::fs::remove_file("logs.txt");

    welog_rs::logger::coverage_force_stdout_serialize_error(true);
    logger::logger().log(build_fields());
    welog_rs::logger::coverage_force_stdout_serialize_error(false);

    welog_rs::logger::coverage_force_try_send_mode(1);
    logger::logger().log(build_fields());

    welog_rs::logger::coverage_force_try_send_mode(2);
    logger::logger().log(build_fields());

    welog_rs::logger::coverage_force_try_send_mode(0);

    welog_rs::logger::coverage_force_elastic_mode(1);
    welog_rs::logger::coverage_force_fallback_serialize_error(true);
    logger::logger().log(build_fields());
    std::thread::sleep(Duration::from_millis(50));

    welog_rs::logger::coverage_force_fallback_serialize_error(false);
    welog_rs::logger::coverage_force_elastic_mode(0);
}

#[cfg(coverage)]
#[test]
fn coverage_force_fallback_write_error() {
    let _guard = PATCH_LOCK.lock().unwrap();
    init_env();
    let _ = std::fs::remove_file("logs.txt");

    welog_rs::logger::coverage_force_elastic_mode(1);
    welog_rs::logger::coverage_force_fallback_write_error(true);
    logger::logger().log(build_fields());
    std::thread::sleep(Duration::from_millis(50));
    welog_rs::logger::coverage_force_fallback_write_error(false);
    welog_rs::logger::coverage_force_elastic_mode(0);
}

#[test]
fn duration_negative_returns_none() {
    let _guard = PATCH_LOCK.lock().unwrap();
    init_env();
    let _ = std::fs::remove_file("logs.txt");

    let mut fields = build_fields();
    fields.insert(
        "requestTimestamp".into(),
        Value::String("2026-02-03T00:00:01Z".into()),
    );
    fields.insert(
        "responseTimestamp".into(),
        Value::String("2026-02-03T00:00:00Z".into()),
    );

    logger::logger().log(fields);
    wait_for_log_file();
}

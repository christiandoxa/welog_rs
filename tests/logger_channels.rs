use std::sync::{Mutex, Once};

use serde_json::Value;
use std::sync::mpsc::{SyncSender, TrySendError};

use welog_rs::logger;
use welog_rs::util::LogFields;

static INIT: Once = Once::new();
static PATCH_LOCK: Mutex<()> = Mutex::new(());

fn init_env() {
    INIT.call_once(|| unsafe {
        std::env::set_var("ELASTIC_URL__", "");
    });
}

fn build_fields() -> LogFields {
    let mut fields = LogFields::new();
    fields.insert("event".into(), Value::String("test".into()));
    fields
}

fn to_string_fail(_value: &serde_json::Map<String, Value>) -> Result<String, serde_json::Error> {
    use serde::ser::Error as _;
    Err(serde_json::Error::custom("forced"))
}

fn try_send_disconnected(
    _sender: &SyncSender<LogFields>,
    msg: LogFields,
) -> Result<(), TrySendError<LogFields>> {
    Err(TrySendError::Disconnected(msg))
}

fn try_send_full(
    _sender: &SyncSender<LogFields>,
    msg: LogFields,
) -> Result<(), TrySendError<LogFields>> {
    Err(TrySendError::Full(msg))
}

#[test]
fn logger_reports_stdout_serialize_error() {
    let _guard = PATCH_LOCK.lock().unwrap();
    init_env();
    let _patch = jmpln::patch!(
        serde_json::to_string::<serde_json::Map<String, Value>> => to_string_fail
    )
    .expect("patch to_string");

    logger::logger().log(build_fields());
}

#[test]
fn logger_reports_channel_disconnected() {
    let _guard = PATCH_LOCK.lock().unwrap();
    init_env();
    let _patch = jmpln::patch!(
        std::sync::mpsc::SyncSender::<LogFields>::try_send => try_send_disconnected
    )
    .expect("patch try_send");

    logger::logger().log(build_fields());
}

#[test]
fn logger_reports_channel_full() {
    let _guard = PATCH_LOCK.lock().unwrap();
    init_env();
    let _patch = jmpln::patch!(
        std::sync::mpsc::SyncSender::<LogFields>::try_send => try_send_full
    )
    .expect("patch try_send");

    logger::logger().log(build_fields());
}

#[cfg(coverage)]
#[test]
fn logger_forced_stdout_serialize_error() {
    let _guard = PATCH_LOCK.lock().unwrap();
    init_env();

    welog_rs::logger::coverage_force_stdout_serialize_error(true);
    logger::logger().log(build_fields());
    welog_rs::logger::coverage_force_stdout_serialize_error(false);
}

#[cfg(coverage)]
#[test]
fn logger_forced_try_send_modes() {
    let _guard = PATCH_LOCK.lock().unwrap();
    init_env();

    welog_rs::logger::coverage_force_try_send_mode(1);
    logger::logger().log(build_fields());

    welog_rs::logger::coverage_force_try_send_mode(2);
    logger::logger().log(build_fields());

    welog_rs::logger::coverage_force_try_send_mode(0);
}

#[cfg(coverage)]
#[test]
fn logger_queue_full_non_threshold_drop() {
    let _guard = PATCH_LOCK.lock().unwrap();
    init_env();

    welog_rs::logger::coverage_force_try_send_mode(2);
    logger::logger().log(build_fields());
    logger::logger().log(build_fields());
    welog_rs::logger::coverage_force_try_send_mode(0);
}

#[cfg(coverage)]
#[test]
fn logger_missing_request_timestamp_duration_branch() {
    let _guard = PATCH_LOCK.lock().unwrap();
    init_env();

    let mut fields = build_fields();
    fields.remove("requestTimestamp");
    fields.insert(
        "responseTimestamp".into(),
        Value::String("2026-02-03T00:00:01Z".into()),
    );

    logger::logger().log(fields);
}

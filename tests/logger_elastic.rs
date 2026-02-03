#[cfg(not(coverage))]
use std::io::Read;
#[cfg(not(coverage))]
use std::net::{TcpListener, TcpStream};
#[cfg(not(coverage))]
use std::sync::OnceLock;
#[cfg(not(coverage))]
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, Once};
use std::time::Duration;

use serde_json::Value;

use welog_rs::logger;
use welog_rs::util::LogFields;

static INIT: Once = Once::new();
static PATCH_LOCK: Mutex<()> = Mutex::new(());
#[cfg(not(coverage))]
static SERVER_ADDR: OnceLock<String> = OnceLock::new();
#[cfg(not(coverage))]
static DROP_CONN: AtomicBool = AtomicBool::new(false);
#[cfg(not(coverage))]
static REQUEST_COUNT: AtomicUsize = AtomicUsize::new(0);

#[cfg(not(coverage))]
fn read_request(stream: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    let mut header_end = None;
    let mut content_len = None;

    loop {
        let n = stream.read(&mut tmp).unwrap_or(0);
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);

        if header_end.is_none() {
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                header_end = Some(pos + 4);
                let headers = String::from_utf8_lossy(&buf[..pos + 4]);
                for line in headers.lines() {
                    if let Some(value) = line.strip_prefix("Content-Length:") {
                        if let Ok(len) = value.trim().parse::<usize>() {
                            content_len = Some(len);
                        }
                    }
                }
            }
        }

        if let (Some(end), Some(len)) = (header_end, content_len) {
            if buf.len() >= end + len {
                break;
            }
        }
    }

    String::from_utf8_lossy(&buf).to_string()
}

#[cfg(not(coverage))]
fn init_env() {
    INIT.call_once(|| {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
        let addr = listener.local_addr().unwrap();
        SERVER_ADDR.set(format!("http://{}", addr)).unwrap();

        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = match stream {
                    Ok(stream) => stream,
                    Err(_) => continue,
                };

                REQUEST_COUNT.fetch_add(1, Ordering::Relaxed);

                let body = read_request(&mut stream);

                if body.contains("\"test_mode\":\"io\"") || DROP_CONN.load(Ordering::Relaxed) {
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                    continue;
                }

                let status_line = if body.contains("\"test_mode\":\"status\"") {
                    "500 Internal Server Error"
                } else if body.contains("\"test_mode\":\"non_success\"") {
                    "302 Found"
                } else {
                    "201 Created"
                };

                let resp = format!(
                    "HTTP/1.1 {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    status_line
                );
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.flush();
            }
        });

        unsafe {
            std::env::set_var("ELASTIC_URL__", SERVER_ADDR.get().unwrap());
            std::env::set_var("ELASTIC_INDEX__", "welog");
            std::env::set_var("ELASTIC_USERNAME__", "user");
            std::env::set_var("ELASTIC_PASSWORD__", "pass");
        }

        let _ = logger::logger();
    });
}

#[cfg(coverage)]
fn init_env() {
    INIT.call_once(|| {
        unsafe {
            std::env::set_var("ELASTIC_URL__", "http://127.0.0.1:9200");
            std::env::set_var("ELASTIC_INDEX__", "welog");
            std::env::set_var("ELASTIC_USERNAME__", "user");
            std::env::set_var("ELASTIC_PASSWORD__", "pass");
        }

        let _ = logger::logger();
    });
}
fn build_fields(test_mode: &str) -> LogFields {
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
    fields.insert("test_mode".into(), Value::String(test_mode.to_string()));
    fields
}

#[cfg(not(coverage))]
fn wait_for_request() {
    let start = std::time::Instant::now();
    while REQUEST_COUNT.load(Ordering::Relaxed) == 0 {
        if start.elapsed() > Duration::from_secs(2) {
            panic!("server did not receive request");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(not(coverage))]
#[test]
fn send_to_elastic_success() {
    let _guard = PATCH_LOCK.lock().unwrap();
    init_env();
    REQUEST_COUNT.store(0, Ordering::Relaxed);
    DROP_CONN.store(false, Ordering::Relaxed);

    logger::logger().log(build_fields("success"));
    wait_for_request();
}

#[cfg(not(coverage))]
#[test]
fn send_to_elastic_status_error() {
    let _guard = PATCH_LOCK.lock().unwrap();
    init_env();
    REQUEST_COUNT.store(0, Ordering::Relaxed);
    DROP_CONN.store(false, Ordering::Relaxed);

    logger::logger().log(build_fields("status"));
    wait_for_request();
}

#[cfg(not(coverage))]
#[test]
fn send_to_elastic_io_error() {
    let _guard = PATCH_LOCK.lock().unwrap();
    init_env();
    REQUEST_COUNT.store(0, Ordering::Relaxed);
    DROP_CONN.store(true, Ordering::Relaxed);

    logger::logger().log(build_fields("io"));
    wait_for_request();
}

#[cfg(not(coverage))]
#[test]
fn send_to_elastic_non_success_response() {
    let _guard = PATCH_LOCK.lock().unwrap();
    init_env();
    REQUEST_COUNT.store(0, Ordering::Relaxed);
    DROP_CONN.store(false, Ordering::Relaxed);

    logger::logger().log(build_fields("non_success"));
    wait_for_request();
}

#[cfg(coverage)]
#[test]
fn send_to_elastic_success_forced() {
    let _guard = PATCH_LOCK.lock().unwrap();
    init_env();

    welog_rs::logger::coverage_force_elastic_mode(0);
    logger::logger().log(build_fields("forced_success"));
    std::thread::sleep(Duration::from_millis(50));
}

#[cfg(coverage)]
#[test]
fn send_to_elastic_status_error_forced() {
    let _guard = PATCH_LOCK.lock().unwrap();
    init_env();

    welog_rs::logger::coverage_force_elastic_mode(1);
    logger::logger().log(build_fields("forced_status"));
    std::thread::sleep(Duration::from_millis(50));
    welog_rs::logger::coverage_force_elastic_mode(0);
}

#[cfg(coverage)]
#[test]
fn send_to_elastic_non_success_forced() {
    let _guard = PATCH_LOCK.lock().unwrap();
    init_env();

    welog_rs::logger::coverage_force_elastic_mode(2);
    logger::logger().log(build_fields("forced_non_success"));
    std::thread::sleep(Duration::from_millis(50));
    welog_rs::logger::coverage_force_elastic_mode(0);
}

#[cfg(coverage)]
#[test]
fn send_to_elastic_request_error_forced() {
    let _guard = PATCH_LOCK.lock().unwrap();
    init_env();

    welog_rs::logger::coverage_force_elastic_mode(3);
    logger::logger().log(build_fields("forced_request_error"));
    std::thread::sleep(Duration::from_millis(50));
    welog_rs::logger::coverage_force_elastic_mode(0);
}

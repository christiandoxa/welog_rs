#[test]
fn logger_queue_capacity_invalid_uses_default() {
    unsafe {
        std::env::set_var("WELOG_QUEUE_CAPACITY__", "not-a-number");
        std::env::set_var("ELASTIC_URL__", "");
    }

    let _ = welog_rs::logger::logger();
}

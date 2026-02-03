#[test]
fn logger_queue_capacity_zero_uses_default() {
    unsafe {
        std::env::set_var("WELOG_QUEUE_CAPACITY__", "0");
        std::env::set_var("ELASTIC_URL__", "");
    }

    let _ = welog_rs::logger::logger();
}

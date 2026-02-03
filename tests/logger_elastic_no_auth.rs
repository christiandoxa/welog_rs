#[cfg(coverage)]
#[test]
fn send_to_elastic_without_auth_header() {
    unsafe {
        std::env::set_var("ELASTIC_URL__", "http://127.0.0.1:9200");
        std::env::set_var("ELASTIC_INDEX__", "welog");
        std::env::remove_var("ELASTIC_USERNAME__");
        std::env::remove_var("ELASTIC_PASSWORD__");
    }

    welog_rs::logger::coverage_force_elastic_mode(0);
    welog_rs::logger::logger().log(Default::default());
    std::thread::sleep(std::time::Duration::from_millis(50));
}

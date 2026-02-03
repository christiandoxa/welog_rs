#[test]
fn set_config_updates_environment() {
    let cfg = welog_rs::Config {
        elastic_index: "idx".into(),
        elastic_url: "http://example.com".into(),
        elastic_username: "user".into(),
        elastic_password: "pass".into(),
    };

    welog_rs::set_config(cfg);

    assert_eq!(
        std::env::var("ELASTIC_INDEX__").ok().as_deref(),
        Some("idx")
    );
    assert_eq!(
        std::env::var("ELASTIC_URL__").ok().as_deref(),
        Some("http://example.com")
    );
    assert_eq!(
        std::env::var("ELASTIC_USERNAME__").ok().as_deref(),
        Some("user")
    );
    assert_eq!(
        std::env::var("ELASTIC_PASSWORD__").ok().as_deref(),
        Some("pass")
    );
}

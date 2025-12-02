/// Counterpart of `Config` in Go:
/// ```go
/// type Config struct {
///     ElasticIndex    string
///     ElasticURL      string
///     ElasticUsername string
///     ElasticPassword string
/// }
/// ```
#[derive(Debug, Clone)]
pub struct Config {
    pub elastic_index: String,
    pub elastic_url: String,
    pub elastic_username: String,
    pub elastic_password: String,
}

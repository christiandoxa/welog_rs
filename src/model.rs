use chrono::{DateTime, Local};
use std::time::Duration;

use serde_json::{Map, Value};

/// Header representation as a JSON map similar to `map[string]interface{}` in Go.
pub type HeaderMapJson = Map<String, Value>;

/// Counterpart of `model.TargetRequest` in Go:
///
/// ```go
/// type TargetRequest struct {
///     URL         string
///     Method      string
///     ContentType string
///     Header      map[string]interface{}
///     Body        []byte
///     Timestamp   time.Time
/// }
/// ```
#[derive(Debug, Clone)]
pub struct TargetRequest {
    pub url: String,
    pub method: String,
    pub content_type: String,
    pub header: HeaderMapJson,
    pub body: Vec<u8>,
    pub timestamp: DateTime<Local>,
}

/// Counterpart of `model.TargetResponse` in Go:
///
/// ```go
/// type TargetResponse struct {
///     Header  map[string]interface{}
///     Body    []byte
///     Status  int
///     Latency time.Duration
/// }
/// ```
#[derive(Debug, Clone)]
pub struct TargetResponse {
    pub header: HeaderMapJson,
    pub body: Vec<u8>,
    pub status: u16,
    pub latency: Duration,
}

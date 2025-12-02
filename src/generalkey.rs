//! Counterpart of `pkg/constant/generalkey/generalkey.go`
///
/// ClientLog is the key used to store the list of client (target) logs in the context.
pub const CLIENT_LOG: &str = "client-log";

/// Logger is the key used to store the logger instance in the context.
pub const LOGGER: &str = "logger";

/// RequestID is the key used to store the per-request unique request ID.
pub const REQUEST_ID: &str = "requestId";

/// RequestIDHeader is the header name for the request ID.
pub const REQUEST_ID_HEADER: &str = "X-Request-ID";

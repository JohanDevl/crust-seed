//! The tRPC wire protocol.
//!
//! The web UI is vendored unchanged from cross-seed and talks tRPC v11 over
//! HTTP, so this implements the client's exact expectations rather than
//! inventing an API. The shapes below were read out of the vendored
//! `@trpc/client` and `@trpc/server` sources:
//!
//! * **Query**: `GET  <url>/<path1,path2>?batch=1&input={"0":…,"1":…}`
//! * **Mutation**: `POST <url>/<path1,path2>?batch=1` with that same object as
//!   the body
//! * **Subscription**: `GET <url>/<path>?input=…` answered as SSE
//!
//! A batched response is a JSON **array**, one element per procedure, in
//! request order. A successful element is `{"result":{"data":…}}`; a failure is
//! `{"error":{"message":…,"code":<jsonrpc number>,"data":{…}}}`. The numeric
//! `code` is mandatory: the client throws `TransformResultError` without it,
//! which surfaces as an unhelpful "Unable to transform response from server".

use serde::Serialize;
use serde_json::{Map, Value, json};

/// tRPC's JSON-RPC-flavoured error codes, from `TRPC_ERROR_CODES_BY_KEY`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrpcErrorCode {
    ParseError,
    BadRequest,
    InternalServerError,
    Unauthorized,
    Forbidden,
    NotFound,
    MethodNotSupported,
    Timeout,
    Conflict,
    TooManyRequests,
}

impl TrpcErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            TrpcErrorCode::ParseError => "PARSE_ERROR",
            TrpcErrorCode::BadRequest => "BAD_REQUEST",
            TrpcErrorCode::InternalServerError => "INTERNAL_SERVER_ERROR",
            TrpcErrorCode::Unauthorized => "UNAUTHORIZED",
            TrpcErrorCode::Forbidden => "FORBIDDEN",
            TrpcErrorCode::NotFound => "NOT_FOUND",
            TrpcErrorCode::MethodNotSupported => "METHOD_NOT_SUPPORTED",
            TrpcErrorCode::Timeout => "TIMEOUT",
            TrpcErrorCode::Conflict => "CONFLICT",
            TrpcErrorCode::TooManyRequests => "TOO_MANY_REQUESTS",
        }
    }

    pub fn as_number(self) -> i64 {
        match self {
            TrpcErrorCode::ParseError => -32700,
            TrpcErrorCode::BadRequest => -32600,
            TrpcErrorCode::InternalServerError => -32603,
            TrpcErrorCode::Unauthorized => -32001,
            TrpcErrorCode::Forbidden => -32003,
            TrpcErrorCode::NotFound => -32004,
            TrpcErrorCode::MethodNotSupported => -32005,
            TrpcErrorCode::Timeout => -32008,
            TrpcErrorCode::Conflict => -32009,
            TrpcErrorCode::TooManyRequests => -32029,
        }
    }

    pub fn http_status(self) -> u16 {
        match self {
            TrpcErrorCode::ParseError | TrpcErrorCode::BadRequest => 400,
            TrpcErrorCode::Unauthorized => 401,
            TrpcErrorCode::Forbidden => 403,
            TrpcErrorCode::NotFound => 404,
            TrpcErrorCode::MethodNotSupported => 405,
            TrpcErrorCode::Timeout => 408,
            TrpcErrorCode::Conflict => 409,
            TrpcErrorCode::TooManyRequests => 429,
            TrpcErrorCode::InternalServerError => 500,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TrpcError {
    pub code: TrpcErrorCode,
    pub message: String,
}

impl TrpcError {
    pub fn new(code: TrpcErrorCode, message: impl Into<String>) -> Self {
        TrpcError {
            code,
            message: message.into(),
        }
    }

    pub fn unauthorized() -> Self {
        TrpcError::new(TrpcErrorCode::Unauthorized, "UNAUTHORIZED")
    }

    pub fn internal(message: impl Into<String>) -> Self {
        TrpcError::new(TrpcErrorCode::InternalServerError, message)
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        TrpcError::new(TrpcErrorCode::BadRequest, message)
    }
}

pub type ProcedureResult = Result<Value, TrpcError>;

pub fn ok<T: Serialize>(value: T) -> ProcedureResult {
    serde_json::to_value(value).map_err(|e| TrpcError::internal(e.to_string()))
}

/// One element of a tRPC response array.
pub fn response_item(path: &str, result: &ProcedureResult) -> Value {
    match result {
        Ok(data) => json!({ "result": { "data": data } }),
        Err(error) => json!({
            "error": {
                "message": error.message,
                "code": error.code.as_number(),
                "data": {
                    "code": error.code.as_str(),
                    "httpStatus": error.code.http_status(),
                    "path": path,
                }
            }
        }),
    }
}

/// The HTTP status for a whole batch: the first failing procedure's status, or
/// 200 when everything succeeded.
pub fn batch_status(results: &[ProcedureResult]) -> u16 {
    results
        .iter()
        .find_map(|result| result.as_ref().err().map(|e| e.code.http_status()))
        .unwrap_or(200)
}

/// Renders the response body, honouring the batch flag.
///
/// A non-batched call returns the bare object; the client only unwraps an array
/// when it sent `batch=1`.
pub fn response_body(paths: &[String], results: &[ProcedureResult], batched: bool) -> Value {
    let items: Vec<Value> = paths
        .iter()
        .zip(results)
        .map(|(path, result)| response_item(path, result))
        .collect();
    if batched {
        Value::Array(items)
    } else {
        items.into_iter().next().unwrap_or(Value::Null)
    }
}

/// Splits the comma-joined procedure path the batch link builds.
pub fn split_paths(path: &str) -> Vec<String> {
    path.split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect()
}

/// Pulls the input for procedure `index` out of the request.
///
/// Batched inputs arrive as `{"0":…,"1":…}` keyed by position, and a procedure
/// with no input is simply absent from that object — hence the `Option`.
pub fn input_for(inputs: &Value, index: usize, batched: bool) -> Option<Value> {
    if !batched {
        return (!inputs.is_null()).then(|| inputs.clone());
    }
    inputs
        .as_object()
        .and_then(|map| map.get(&index.to_string()))
        .cloned()
}

/// Parses the `input` query parameter (queries) or request body (mutations).
pub fn parse_inputs(raw: Option<&str>) -> Value {
    match raw {
        None => Value::Null,
        Some(raw) if raw.trim().is_empty() => Value::Null,
        Some(raw) => serde_json::from_str(raw).unwrap_or(Value::Null),
    }
}

/// SSE frames, as `sseStreamProducer` emits them.
pub mod sse {
    /// Sent first, carrying the client options object.
    pub const CONNECTED_EVENT: &str = "connected";
    /// Sent when the stream ends normally, so the client stops reconnecting.
    pub const RETURN_EVENT: &str = "return";
    pub const PING_EVENT: &str = "ping";
    pub const SERIALIZED_ERROR_EVENT: &str = "serialized-error";

    pub fn frame(event: Option<&str>, data: &str) -> String {
        match event {
            Some(event) => format!("event: {event}\ndata: {data}\n\n"),
            // A frame with no event name arrives as an EventSource "message",
            // which is what carries subscription data.
            None => format!("data: {data}\n\n"),
        }
    }
}

/// Builds the `{ "code": … }` metadata object used in error envelopes.
pub fn error_data(code: TrpcErrorCode, path: &str) -> Map<String, Value> {
    json!({
        "code": code.as_str(),
        "httpStatus": code.http_status(),
        "path": path,
    })
    .as_object()
    .cloned()
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batched_paths_are_split_on_commas() {
        assert_eq!(
            split_paths("auth.authStatus,meta.getBuildInfo"),
            vec!["auth.authStatus", "meta.getBuildInfo"]
        );
        assert_eq!(split_paths("settings.get"), vec!["settings.get"]);
    }

    #[test]
    fn batched_inputs_are_keyed_by_position() {
        let inputs = parse_inputs(Some(r#"{"0":{"limit":10},"2":{"id":3}}"#));
        assert_eq!(input_for(&inputs, 0, true), Some(json!({"limit": 10})));
        // A procedure with no input is absent from the object entirely.
        assert_eq!(input_for(&inputs, 1, true), None);
        assert_eq!(input_for(&inputs, 2, true), Some(json!({"id": 3})));
    }

    #[test]
    fn unbatched_input_is_the_whole_value() {
        let inputs = parse_inputs(Some(r#"{"limit":10}"#));
        assert_eq!(input_for(&inputs, 0, false), Some(json!({"limit": 10})));
        assert_eq!(input_for(&Value::Null, 0, false), None);
    }

    /// The client unwraps an array only when it asked for a batch.
    #[test]
    fn batched_responses_are_arrays_and_single_ones_are_not() {
        let results = vec![ok(json!({"a": 1}))];
        let paths = vec!["x.y".to_string()];

        let batched = response_body(&paths, &results, true);
        assert!(batched.is_array());
        assert_eq!(batched[0]["result"]["data"], json!({"a": 1}));

        let single = response_body(&paths, &results, false);
        assert!(single.is_object());
        assert_eq!(single["result"]["data"], json!({"a": 1}));
    }

    /// The numeric code is what stops the client throwing
    /// TransformResultError, so it must always be present.
    #[test]
    fn error_items_carry_a_numeric_code_and_http_status() {
        let item = response_item("auth.logIn", &Err(TrpcError::unauthorized()));
        assert_eq!(item["error"]["code"], json!(-32001));
        assert_eq!(item["error"]["data"]["code"], json!("UNAUTHORIZED"));
        assert_eq!(item["error"]["data"]["httpStatus"], json!(401));
        assert_eq!(item["error"]["data"]["path"], json!("auth.logIn"));
    }

    #[test]
    fn the_batch_status_is_the_first_failure() {
        assert_eq!(batch_status(&[ok(json!(1)), ok(json!(2))]), 200);
        assert_eq!(
            batch_status(&[ok(json!(1)), Err(TrpcError::unauthorized())]),
            401
        );
        assert_eq!(
            batch_status(&[
                Err(TrpcError::bad_request("bad")),
                Err(TrpcError::unauthorized())
            ]),
            400
        );
    }

    #[test]
    fn sse_frames_match_the_event_stream_format() {
        assert_eq!(
            sse::frame(Some("connected"), "{}"),
            "event: connected\ndata: {}\n\n"
        );
        assert_eq!(sse::frame(None, r#"{"a":1}"#), "data: {\"a\":1}\n\n");
    }

    #[test]
    fn malformed_input_is_treated_as_absent_rather_than_failing() {
        assert_eq!(parse_inputs(Some("not json")), Value::Null);
        assert_eq!(parse_inputs(Some("")), Value::Null);
        assert_eq!(parse_inputs(None), Value::Null);
    }
}

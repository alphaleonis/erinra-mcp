//! Stdio-to-HTTP relay: bridges stdio JSON-RPC to the daemon's `/mcp` HTTP endpoint.

use anyhow::{Context, Result};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

/// Bridge stdio JSON-RPC to the daemon's `/mcp` HTTP endpoint.
///
/// Reads newline-delimited JSON from `reader`, POSTs each message to the daemon,
/// and writes responses to `writer`. Generic over reader/writer for testability
/// (production uses stdin/stdout, tests use DuplexStream).
pub async fn run_relay<R, W>(
    mut reader: R,
    mut writer: W,
    base_url: &str,
    auth_token: &str,
) -> Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("failed to create HTTP client")?;
    let mcp_url = format!("{base_url}/mcp");
    let auth_header = format!("Bearer {auth_token}");

    let mut line = String::new();
    loop {
        line.clear();
        let bytes_read = reader
            .read_line(&mut line)
            .await
            .context("failed to read from stdin")?;
        if bytes_read == 0 {
            // EOF — clean shutdown
            break;
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Parse to check if this is a request (has "id") or notification (no "id")
        let msg: serde_json::Value =
            serde_json::from_str(trimmed).context("failed to parse JSON-RPC message")?;
        let is_request = msg.get("id").is_some();

        let response = client
            .post(&mcp_url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("Authorization", &auth_header)
            .body(trimmed.to_string())
            .send()
            .await
            .context("failed to send request to daemon")?;

        let status = response.status();

        if is_request && status.as_u16() == 200 {
            let body = response
                .bytes()
                .await
                .context("failed to read response body")?;
            writer
                .write_all(&body)
                .await
                .context("failed to write response")?;
            writer.write_all(b"\n").await?;
            writer.flush().await?;
        } else if status.as_u16() == 202 {
            // Notification accepted — no response to write
        } else if is_request && !status.is_success() {
            // Return a JSON-RPC error to the client instead of terminating the relay.
            // Only connection-level failures (handled by `?` on send()) should kill the session.
            let error_response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": msg.get("id"),
                "error": {
                    "code": -32603,
                    "message": format!("daemon returned HTTP {status}")
                }
            });
            let mut err_bytes = serde_json::to_vec(&error_response)?;
            err_bytes.push(b'\n');
            writer.write_all(&err_bytes).await?;
            writer.flush().await?;
        } else if !status.is_success() {
            tracing::warn!(
                status = %status,
                method = ?msg.get("method"),
                "daemon returned error for notification, ignoring"
            );
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    use crate::db::{Database, DbConfig};
    use crate::embedding::MockEmbedder;
    use crate::service::{MemoryService, ServiceConfig};
    use crate::web::AppState;

    use super::*;

    /// Start a real Axum server on an ephemeral port using in-memory DB,
    /// returning the base URL and auth token.
    async fn start_test_server() -> (String, String) {
        let db = Database::open_in_memory(&DbConfig::default()).unwrap();
        let auth_token = "test-relay-token".to_string();
        let service = MemoryService::new(
            Arc::new(Mutex::new(db)),
            Arc::new(MockEmbedder::new(768)),
            None,
            ServiceConfig::default(),
        );
        let state = AppState {
            service,
            auth_token: auth_token.clone(),
        };
        let app = crate::web::app_router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://127.0.0.1:{port}"), auth_token)
    }

    /// A relay test harness: spawns the relay, provides send/receive helpers.
    struct RelayHarness {
        stdin_tx: tokio::io::WriteHalf<tokio::io::DuplexStream>,
        stdout_reader: BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    }

    impl RelayHarness {
        async fn start(base_url: &str, auth_token: &str) -> Self {
            let (stdin_tx, stdin_rx) = tokio::io::duplex(8192);
            let (stdout_tx, stdout_rx) = tokio::io::duplex(8192);

            let url = base_url.to_string();
            let token = auth_token.to_string();
            tokio::spawn(async move {
                let reader = BufReader::new(stdin_rx);
                run_relay(reader, stdout_tx, &url, &token).await
            });

            let (stdin_read_half, stdin_write_half) = tokio::io::split(stdin_tx);
            let (stdout_read_half, _stdout_write_half) = tokio::io::split(stdout_rx);
            // We only write to stdin_tx and read from stdout_rx
            drop(stdin_read_half);
            drop(_stdout_write_half);

            Self {
                stdin_tx: stdin_write_half,
                stdout_reader: BufReader::new(stdout_read_half),
            }
        }

        /// Send a JSON-RPC message (appends newline).
        async fn send(&mut self, msg: &serde_json::Value) {
            let mut line = serde_json::to_string(msg).unwrap();
            line.push('\n');
            self.stdin_tx.write_all(line.as_bytes()).await.unwrap();
            self.stdin_tx.flush().await.unwrap();
        }

        /// Read one JSON-RPC response line.
        async fn recv(&mut self) -> serde_json::Value {
            let mut line = String::new();
            self.stdout_reader.read_line(&mut line).await.unwrap();
            serde_json::from_str(&line).expect("response should be valid JSON")
        }
    }

    #[tokio::test]
    async fn relay_forwards_initialize_request_and_returns_server_info() {
        let (base_url, auth_token) = start_test_server().await;
        let mut harness = RelayHarness::start(&base_url, &auth_token).await;

        harness
            .send(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-03-26",
                    "capabilities": {},
                    "clientInfo": {"name": "relay-test", "version": "0.1"}
                }
            }))
            .await;

        let response = harness.recv().await;
        assert!(
            response["result"]["serverInfo"].is_object(),
            "response should contain serverInfo, got: {response}"
        );
        assert_eq!(
            response["result"]["serverInfo"]["name"], "erinra",
            "server name should be erinra"
        );
    }

    #[tokio::test]
    async fn relay_forwards_tool_calls_store_and_get_round_trip() {
        let (base_url, auth_token) = start_test_server().await;
        let mut harness = RelayHarness::start(&base_url, &auth_token).await;

        // Initialize
        harness
            .send(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-03-26",
                    "capabilities": {},
                    "clientInfo": {"name": "relay-test", "version": "0.1"}
                }
            }))
            .await;
        let init_resp = harness.recv().await;
        assert!(init_resp["result"]["serverInfo"].is_object());

        // Send initialized notification (no response expected)
        harness
            .send(&serde_json::json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized"
            }))
            .await;

        // Store a memory
        harness
            .send(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "store",
                    "arguments": {
                        "content": "Relay round-trip test memory",
                        "projects": ["relay-test"],
                        "type": "note"
                    }
                }
            }))
            .await;
        let store_resp = harness.recv().await;
        let store_text = store_resp["result"]["content"][0]["text"]
            .as_str()
            .expect("store should return text content");
        let store_data: serde_json::Value = serde_json::from_str(store_text).unwrap();
        let memory_id = store_data["id"]
            .as_str()
            .expect("store should return an id");
        assert!(!memory_id.is_empty());

        // Get the memory back
        harness
            .send(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {
                    "name": "get",
                    "arguments": { "ids": [memory_id] }
                }
            }))
            .await;
        let get_resp = harness.recv().await;
        let get_text = get_resp["result"]["content"][0]["text"]
            .as_str()
            .expect("get should return text content");
        let get_data: serde_json::Value = serde_json::from_str(get_text).unwrap();
        assert_eq!(get_data[0]["id"].as_str(), Some(memory_id));
        assert_eq!(
            get_data[0]["content"].as_str(),
            Some("Relay round-trip test memory")
        );
        assert_eq!(get_data[0]["projects"][0].as_str(), Some("relay-test"));
    }

    #[tokio::test]
    async fn relay_handles_notifications_without_writing_response() {
        let (base_url, auth_token) = start_test_server().await;
        let mut harness = RelayHarness::start(&base_url, &auth_token).await;

        // Initialize first (required)
        harness
            .send(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-03-26",
                    "capabilities": {},
                    "clientInfo": {"name": "relay-test", "version": "0.1"}
                }
            }))
            .await;
        let _ = harness.recv().await;

        // Send a notification (no "id" field)
        harness
            .send(&serde_json::json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized"
            }))
            .await;

        // Now send a request and verify we get the response to THAT request,
        // not some stale notification response
        harness
            .send(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/list"
            }))
            .await;
        let tools_resp = harness.recv().await;

        // The response should be for id:2 (tools/list), not anything from the notification
        assert_eq!(
            tools_resp["id"], 2,
            "response should be for the tools/list request"
        );
        assert!(
            tools_resp["result"]["tools"].is_array(),
            "tools/list should return a tools array, got: {tools_resp}"
        );
    }

    #[tokio::test]
    async fn relay_stops_on_reader_eof() {
        let (base_url, auth_token) = start_test_server().await;

        let (stdin_tx, stdin_rx) = tokio::io::duplex(8192);
        let (stdout_tx, _stdout_rx) = tokio::io::duplex(8192);

        let url = base_url.clone();
        let token = auth_token.clone();
        let relay_handle = tokio::spawn(async move {
            let reader = BufReader::new(stdin_rx);
            run_relay(reader, stdout_tx, &url, &token).await
        });

        // Drop the write side of stdin -> relay should see EOF and exit cleanly
        drop(stdin_tx);

        let result = tokio::time::timeout(std::time::Duration::from_secs(5), relay_handle)
            .await
            .expect("relay should finish within timeout")
            .expect("relay task should not panic");

        assert!(
            result.is_ok(),
            "relay should return Ok on EOF, got: {result:?}"
        );
    }
}

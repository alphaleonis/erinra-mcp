//! MCP protocol integration tests.
//!
//! These tests exercise the full JSON-RPC protocol path: initialize → initialized →
//! tools/list, tools/call. They communicate via tokio::io::duplex, sending raw
//! JSON-RPC messages and verifying responses.
//!
//! This catches bugs like the Router wrapping issue (tools/list returning empty)
//! that unit tests calling tool methods directly would miss.

use std::sync::{Arc, Mutex};

use erinra::db::{Database, DbConfig};
use erinra::embedding::MockEmbedder;
use erinra::mcp::ErinraServer;
use erinra::service::{MemoryService, ServiceConfig};

use rmcp::ServiceExt;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Start an MCP server on a duplex channel and return a (reader, writer) for the client side.
/// The returned TempDir must be kept alive for the duration of the test (database lives there).
async fn start_server() -> (
    BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    tokio::io::WriteHalf<tokio::io::DuplexStream>,
    tempfile::TempDir,
) {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let db = Database::open(&db_path, &DbConfig::default()).unwrap();
    let embedder = Arc::new(MockEmbedder::new(768));
    let service = MemoryService::new(
        Arc::new(Mutex::new(db)),
        embedder,
        None,
        ServiceConfig::default(),
    );
    let server = ErinraServer::new(service);

    // Create two connected duplex streams: server_io and client_io.
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);

    // Spawn the server on server_io.
    tokio::spawn(async move {
        let service = server.serve(server_io).await.unwrap();
        service.waiting().await.unwrap();
    });

    // Split client_io into reader/writer.
    let (reader, writer) = tokio::io::split(client_io);
    (BufReader::new(reader), writer, dir)
}

/// Send a JSON-RPC message and read the response (newline-delimited).
async fn send_and_recv(
    writer: &mut tokio::io::WriteHalf<tokio::io::DuplexStream>,
    reader: &mut BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    msg: &str,
) -> serde_json::Value {
    writer.write_all(msg.as_bytes()).await.unwrap();
    writer.write_all(b"\n").await.unwrap();
    writer.flush().await.unwrap();

    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    serde_json::from_str(&line).unwrap()
}

/// Send a notification (no response expected).
async fn send_notification(writer: &mut tokio::io::WriteHalf<tokio::io::DuplexStream>, msg: &str) {
    writer.write_all(msg.as_bytes()).await.unwrap();
    writer.write_all(b"\n").await.unwrap();
    writer.flush().await.unwrap();
}

/// Perform the MCP handshake (initialize + initialized notification).
async fn handshake(
    writer: &mut tokio::io::WriteHalf<tokio::io::DuplexStream>,
    reader: &mut BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
) -> serde_json::Value {
    let init_resp = send_and_recv(
        writer,
        reader,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#,
    )
    .await;

    send_notification(
        writer,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
    )
    .await;

    init_resp
}

#[tokio::test]
async fn initialize_returns_server_info() {
    let (mut reader, mut writer, _dir) = start_server().await;
    let resp = handshake(&mut writer, &mut reader).await;

    assert_eq!(resp["result"]["serverInfo"]["name"], "erinra");
    assert!(
        resp["result"]["instructions"]
            .as_str()
            .unwrap()
            .contains("Erinra")
    );
    assert!(resp["result"]["capabilities"]["tools"].is_object());
}

#[tokio::test]
async fn tools_list_returns_all_tools() {
    let (mut reader, mut writer, _dir) = start_server().await;
    handshake(&mut writer, &mut reader).await;

    let resp = send_and_recv(
        &mut writer,
        &mut reader,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
    )
    .await;

    let tools = resp["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 11, "expected 11 tools, got {}", tools.len());

    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    for expected in [
        "store", "update", "archive", "merge", "link", "unlink", "search", "get", "list",
        "discover", "context",
    ] {
        assert!(
            names.contains(&expected),
            "missing tool: {expected}. Found: {names:?}"
        );
    }
}

#[tokio::test]
async fn tools_list_includes_schemas() {
    let (mut reader, mut writer, _dir) = start_server().await;
    handshake(&mut writer, &mut reader).await;

    let resp = send_and_recv(
        &mut writer,
        &mut reader,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
    )
    .await;

    let tools = resp["result"]["tools"].as_array().unwrap();
    let store_tool = tools.iter().find(|t| t["name"] == "store").unwrap();

    // store tool should have an inputSchema with required "content" field.
    let schema = &store_tool["inputSchema"];
    assert_eq!(schema["type"], "object");
    let required = schema["required"].as_array().unwrap();
    assert!(required.iter().any(|r| r == "content"));
}

#[tokio::test]
async fn invalid_tool_name_returns_error() {
    let (mut reader, mut writer, _dir) = start_server().await;
    handshake(&mut writer, &mut reader).await;

    let resp = send_and_recv(
        &mut writer,
        &mut reader,
        r#"{"jsonrpc":"2.0","id":20,"method":"tools/call","params":{"name":"nonexistent_tool","arguments":{}}}"#,
    )
    .await;

    // Should return a JSON-RPC error (method not found or similar).
    assert!(
        resp["error"].is_object(),
        "expected error for nonexistent tool: {resp}"
    );
}

#[tokio::test]
async fn discover_via_protocol() {
    let (mut reader, mut writer, _dir) = start_server().await;
    handshake(&mut writer, &mut reader).await;

    let resp = send_and_recv(
        &mut writer,
        &mut reader,
        r#"{"jsonrpc":"2.0","id":30,"method":"tools/call","params":{"name":"discover","arguments":{}}}"#,
    )
    .await;

    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let discover: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(discover["stats"]["total_memories"], 0);
    assert!(discover["projects"].as_array().unwrap().is_empty());
}

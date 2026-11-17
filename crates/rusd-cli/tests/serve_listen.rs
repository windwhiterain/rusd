//! CLI-level smoke for `rusd serve --listen` (D12, `docs/PROTOCOL.md`).
//!
//! The transport itself is tested in `rusd-agent`; this only proves the command
//! line wiring: a bare `--listen` takes PROTOCOL's default address, an explicit
//! one overrides it, and with no flag the server stays on stdio.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::process::{Command, Stdio};

/// Start `rusd serve <args>`, and read the one line it prints before serving.
fn start(args: &[&str]) -> (std::process::Child, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rusd"))
        .arg("serve")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn rusd serve");
    let stderr = child.stderr.take().expect("stderr is piped");
    let mut reader = BufReader::new(stderr);
    let mut line = String::new();
    reader.read_line(&mut line).expect("read the startup line");
    (child, line.trim().to_owned())
}

/// One JSON-RPC round trip over a fresh connection.
fn hello(addr: &str) -> serde_json::Value {
    let mut stream = TcpStream::connect(addr).expect("connect to the server");
    writeln!(
        stream,
        "{}",
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "hello",
                           "params": {"client": "smoke", "protocol": 1}})
    )
    .expect("write hello");
    stream.flush().expect("flush hello");
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .expect("read the hello response");
    serde_json::from_str(&line).expect("the server writes JSON")
}

#[test]
fn a_bare_listen_flag_serves_on_the_protocol_default() {
    let (mut child, line) = start(&["--listen"]);
    // Either the default port is free and the server serves there, or something
    // else holds it — and the message names it either way, which is the point:
    // a bare flag chose `127.0.0.1:8765`, not an ephemeral port.
    let listening = line.starts_with("rusd serve: listening on ");
    assert!(
        line.contains("127.0.0.1:8765"),
        "a bare --listen must take the protocol default, got: {line}"
    );
    if listening {
        let response = hello("127.0.0.1:8765");
        assert_eq!(response["result"]["protocol"], 1, "{response}");
        assert_eq!(response["result"]["version"], 0, "{response}");
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn an_explicit_listen_address_overrides_the_default() {
    // Port 0 is an ephemeral port: the server reports the one it bound, and
    // that reported address is where it answers.
    let (mut child, line) = start(&["--listen", "127.0.0.1:0"]);
    let addr = line
        .strip_prefix("rusd serve: listening on ")
        .and_then(|rest| rest.split(' ').next())
        .unwrap_or_else(|| panic!("no listening line: {line}"))
        .to_owned();
    assert!(
        !addr.ends_with(":8765"),
        "an explicit address must override the default, got {addr}"
    );
    let response = hello(&addr);
    assert_eq!(response["result"]["protocol"], 1, "{response}");
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn without_the_flag_serve_stays_on_stdio() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rusd"))
        .arg("serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn rusd serve");
    {
        let stdin = child.stdin.as_mut().expect("stdin is piped");
        writeln!(
            stdin,
            "{}",
            serde_json::json!({"jsonrpc": "2.0", "id": 7, "method": "state", "params": {}})
        )
        .expect("write a stdio request");
        stdin.flush().expect("flush the stdio request");
    }
    let mut line = String::new();
    BufReader::new(child.stdout.take().expect("stdout is piped"))
        .read_line(&mut line)
        .expect("read the stdio response");
    let response: serde_json::Value = serde_json::from_str(&line).expect("JSON-RPC on stdout");
    assert_eq!(response["id"], 7, "{response}");
    assert_eq!(response["result"]["open"], false, "{response}");
    let _ = child.kill();
    let _ = child.wait();
}

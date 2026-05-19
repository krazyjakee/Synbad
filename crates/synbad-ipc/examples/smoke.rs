//! End-to-end smoke test against a running `synbadd`.
//!
//! Run with `cargo run -p synbad-ipc --example smoke`. Confirms the IPC
//! transport works by issuing request/response pairs.

use synbad_config::paths;
use synbad_ipc::{client::Connection, Request, Response};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let socket = paths::ipc_socket();
    println!("connecting to {}", socket.display());

    let mut conn = Connection::connect(&socket)?;
    match conn.request(Request::GetStatus)? {
        Response::Status { state, recent_log } => {
            println!("state: {:?}", state);
            println!("recent log lines: {}", recent_log.len());
        }
        other => println!("unexpected: {:?}", other),
    }
    match conn.request(Request::GetConfig)? {
        Response::Config { config } => {
            println!(
                "config: role={:?} screens={} links={}",
                config.role,
                config.screens.len(),
                config.links.len()
            );
        }
        other => println!("unexpected: {:?}", other),
    }
    println!("done");
    Ok(())
}

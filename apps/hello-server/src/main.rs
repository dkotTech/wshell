//! Example program (kind = "command"): an ordinary `main` with its own HTTP server.
//! Shell opens the port from the `net.listen` permission and does not interfere with anything else.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{SystemTime, UNIX_EPOCH};

const PORT: u16 = 8480;

fn main() {
    let listener = TcpListener::bind(("0.0.0.0", PORT)).expect("bind");
    println!("hello-server: listening on port {PORT}");
    let mut served = 0u64;
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                served += 1;
                if let Err(e) = handle(stream, served) {
                    eprintln!("hello-server: {e}");
                }
            }
            Err(e) => eprintln!("hello-server: accept: {e}"),
        }
    }
}

fn handle(stream: TcpStream, served: u64) -> std::io::Result<()> {
    let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_default();
    // No try_clone: dup of a socket is not supported under WASI, so read and write via &TcpStream.
    let mut reader = BufReader::new(&stream);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    // Request headers are not needed: read up to the empty line.
    let mut line = String::new();
    while reader.read_line(&mut line)? > 2 {
        line.clear();
    }
    let mut parts = request_line.split_whitespace();
    let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or("/"));
    println!("{peer} {method} {path}");

    let (status, body) = if method == "GET" {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let body = format!(
            "{{\"hello\":\"from wshell\",\"path\":\"{}\",\"served\":{served},\"unix_time\":{now}}}\n",
            path.replace('"', "")
        );
        ("200 OK", body)
    } else {
        ("405 Method Not Allowed", "{\"error\":\"only GET\"}\n".to_string())
    };
    write!(
        &stream,
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

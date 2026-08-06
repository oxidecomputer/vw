// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! `vw analyzer` — multi-language LSP for the vw HDL workflow.
//!
//! The server is built around a [`LanguageBackend`] abstraction even
//! while only [`HtclBackend`] is wired up. This keeps the architectural
//! slot for VHDL (initially a `vhdl_ls` proxy, later a direct Oxide
//! VHDL frontend integration) open from day one — see the project
//! plan's "LSP design" section.

mod backend;
mod htcl_backend;
mod server;
mod src_complete;
mod vhdl_backend;
mod workspace;

pub use backend::{LanguageBackend, SymbolInfo};
pub use htcl_backend::HtclBackend;
pub use server::Analyzer;
pub use vhdl_backend::VhdlBackend;

use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt,
    BufReader,
};
use tower_lsp::{LspService, Server};

/// Run the LSP server on stdio. Returns when the editor disconnects.
///
/// Both the standalone `vw-analyzer` binary and the `vw analyzer`
/// subcommand call this so the editor sees identical behavior either
/// way.
pub async fn run_stdio() {
    let (service, socket) = LspService::new(Analyzer::new);
    let stdout = tokio::io::stdout();

    // Splice stdin through `forward_until_exit` instead of handing it
    // to tower-lsp directly. tower-lsp 0.20's `serve` loop is a
    // `join!` whose read side only completes on stdin EOF; its `exit`
    // notification handling merely flips server state and closes the
    // client sink — it does NOT stop reading stdin. Editors such as
    // Helix send `exit` and keep the stdin pipe OPEN, expecting the
    // server to self-terminate, so without help the process lingers
    // forever after every `:lsp-restart`. And because Helix only
    // clears a server's diagnostics once that server's stdout closes
    // (its transport synthesizes an `exit` on `StreamClosed` to drive
    // the cleanup), a never-exiting instance keeps its diagnostics
    // live and each restart stacks another copy on top — the doubling.
    //
    // The forwarder relays every message verbatim, then drops its
    // write half right after relaying `exit`. tower-lsp reads `exit`,
    // then reads EOF, `serve` returns, and the process exits cleanly —
    // closing stdout, which is the signal the editor needs.
    let (server_stdin, feed) = tokio::io::duplex(1 << 16);
    let pump = forward_until_exit(tokio::io::stdin(), feed);
    let server = Server::new(server_stdin, stdout, socket).serve(service);

    // Race the server against the forwarder. The forwarder returns as
    // soon as it relays `exit` (or real stdin hits EOF) — i.e. the
    // moment the session is over — whereas `serve` itself does NOT
    // reliably return on `exit`: tower-lsp 0.20 processes the `exit`
    // notification but its `join!(read_input, …)` only unwinds on
    // stdin EOF, and even the EOF-after-exit case fails to complete
    // the join. So we treat the forwarder finishing as the
    // authoritative end-of-session and terminate on it.
    tokio::select! {
        _ = server => {}
        _ = pump => {}
    }

    // Force the process down rather than falling off `main`:
    // `tokio::io::stdin()` reads on a blocking thread parked in
    // `read()` while the real stdin pipe stays open (Helix keeps it
    // open across `:lsp-restart`), and the runtime's drop waits on
    // that thread — a clean return would hang. Exiting closes our
    // stdout, which is the signal the editor needs to clear this
    // instance's diagnostics (its transport synthesizes the terminal
    // `exit` on that EOF). A client that waits for the `shutdown`
    // response already received it before sending `exit`, so nothing
    // in flight is lost.
    std::process::exit(0);
}

/// Relay LSP `Content-Length`-framed messages from `src` to `dst`
/// byte-for-byte, returning once an `exit` notification has been
/// relayed (or `src` reaches EOF / a malformed frame is hit). The
/// caller drops `dst` when this returns, which is what surfaces EOF to
/// tower-lsp's serve loop and lets the process terminate on `exit` —
/// see `run_stdio` for why that's necessary. Framing-only: it parses
/// just enough of each frame (Content-Length header, then the JSON
/// `method`) to detect `exit`; everything is forwarded unchanged.
async fn forward_until_exit<R, W>(src: R, mut dst: W)
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut src = BufReader::new(src);
    loop {
        // Read the header block, capturing raw bytes + Content-Length.
        let mut headers: Vec<u8> = Vec::new();
        let mut content_len: Option<usize> = None;
        loop {
            let mut line = Vec::new();
            match src.read_until(b'\n', &mut line).await {
                Ok(0) => return, // stdin EOF
                Ok(_) => {}
                Err(_) => return,
            }
            headers.extend_from_slice(&line);
            if let Some(colon) = line.iter().position(|&b| b == b':') {
                let (name, rest) = line.split_at(colon);
                if name.eq_ignore_ascii_case(b"content-length") {
                    content_len = std::str::from_utf8(&rest[1..])
                        .ok()
                        .and_then(|s| s.trim().parse::<usize>().ok());
                }
            }
            // A blank line (just CRLF/LF) ends the header block.
            let trimmed = line.strip_suffix(b"\n").unwrap_or(line.as_slice());
            let trimmed = trimmed.strip_suffix(b"\r").unwrap_or(trimmed);
            if trimmed.is_empty() {
                break;
            }
        }
        let Some(len) = content_len else { return };

        // Read the exact body.
        let mut body = vec![0u8; len];
        if src.read_exact(&mut body).await.is_err() {
            return;
        }

        // Forward the frame verbatim.
        if dst.write_all(&headers).await.is_err()
            || dst.write_all(&body).await.is_err()
            || dst.flush().await.is_err()
        {
            return;
        }

        // Stop once `exit` has been forwarded so tower-lsp hits EOF.
        let is_exit = serde_json::from_slice::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v.get("method")?.as_str().map(|m| m == "exit"))
            .unwrap_or(false);
        if is_exit {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::forward_until_exit;

    fn frame(body: &str) -> Vec<u8> {
        format!("Content-Length: {}\r\n\r\n{}", body.len(), body).into_bytes()
    }

    #[tokio::test]
    async fn forwards_frames_then_stops_after_exit() {
        let mut input = Vec::new();
        input.extend(frame(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
        ));
        input.extend(frame(
            r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#,
        ));
        input.extend(frame(r#"{"jsonrpc":"2.0","id":2,"method":"shutdown"}"#));
        input.extend(frame(r#"{"jsonrpc":"2.0","method":"exit"}"#));
        // Anything after `exit` must NOT be forwarded.
        input.extend(frame(r#"{"jsonrpc":"2.0","method":"never"}"#));

        let mut out: Vec<u8> = Vec::new();
        forward_until_exit(&input[..], &mut out).await;

        let s = String::from_utf8(out).unwrap();
        assert!(s.contains(r#""method":"initialize""#));
        assert!(s.contains(r#""method":"shutdown""#));
        assert!(s.contains(r#""method":"exit""#));
        assert!(
            !s.contains("never"),
            "forwarding must stop right after `exit`"
        );
    }

    #[tokio::test]
    async fn relays_body_bytes_exactly() {
        // A body containing the literal text `"method":"exit"` inside a
        // string value must NOT trip early termination — only the real
        // top-level method matters.
        let tricky = r#"{"jsonrpc":"2.0","method":"textDocument/didChange","params":{"text":"\"method\":\"exit\""}}"#;
        let mut input = frame(tricky);
        input.extend(frame(r#"{"jsonrpc":"2.0","method":"exit"}"#));

        let mut out: Vec<u8> = Vec::new();
        forward_until_exit(&input[..], &mut out).await;

        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("didChange"), "real message must be forwarded");
        assert!(s.contains(r#""method":"exit""#));
    }
}

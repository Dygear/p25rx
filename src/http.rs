//! HTTP response utilities.

use std::io::{Write, BufWriter};
use std;

use chrono::Utc;
use serde::Serialize;
use serde_json;
use uhttp_chunked_write::ChunkedWrite;
use uhttp_response_header::HeaderLines;
use uhttp_status::StatusCode;
use uhttp_version::HttpVersion;

/// Send common response headers starting with the given status code.
pub fn send_status<W: Write>(s: W, st: StatusCode) -> std::io::Result<()> {
    send_head(&mut HeaderLines::new(s), st)
}

/// Write common response headers into the given sink.
pub fn send_head<W: Write>(h: &mut HeaderLines<W>, st: StatusCode) -> std::io::Result<()> {
    write!(h.line(), "{} {}", HttpVersion::from_parts(1, 1), st)?;
    write!(h.line(), "Date: {}", Utc::now().format("%a, %d %b %Y %T %Z"))?;
    write!(h.line(), "Access-Control-Allow-Origin: *")?;

    Ok(())
}

/// Send the given message as a JSON response body.
pub fn send_json<W: Write, S: Serialize>(mut s: W, msg: S) -> std::io::Result<()> {
    {
        let mut h = HeaderLines::new(&mut s);
        send_head(&mut h, StatusCode::Ok)?;
        write!(h.line(), "Content-Type: application/json")?;
        write!(h.line(), "Transfer-Encoding: chunked")?;
    }

    let mut body = BufWriter::new(ChunkedWrite::new(s));

    serde_json::to_writer(&mut body, &msg)
        .map_err(|_| std::io::ErrorKind::Other.into())
}

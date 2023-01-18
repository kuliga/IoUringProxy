use std::{
    str,
    io::Write,
};

pub fn decode_request(buf: &str) -> (usize, &'static str) {
    let req_line = buf.lines().next().unwrap();
    match &req_line[..] {
        // null terminated strings as it will be passed to c syscall
        "GET / HTTP/1.1" => (200, "hello.html\0"),
        _ => (404, "404.html\0"),
    }
}

pub fn format_response(status_code: usize, len: usize, buf: &[u8], mut response: &mut [u8]) {
    let status_code = match status_code {
        200 => "HTTP/1.1 200 OK",
        404 => "HTTP/1.1 404 NOT FOUND",
        _ => "HTTP/1.1 404 NOT FOUND",
    };

    let buf = str::from_utf8(&buf).unwrap();
    write!(response, "{status_code}\r\nContent-Length: {len}\r\n\r\n{buf}");
}


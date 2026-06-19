//! Минимальный HTTP/1.1 сервер на голом `std::net` — без hyper/axum/tokio.
//!
//! Зачем ручной: контракт SPEC/AGENTS «сервер не тянет `windows` crate даже
//! транзитивно» (см. Фаза 2). `tokio`+`hyper`/`axum` тянут `windows-sys` через
//! `mio` на Windows-хосте — даже target-gated, `cargo tree` это покажет. Для
//! REST + статического `WebUI` ручной разбор HTTP/1.1 — ~200 строк и ноль
//! новых зависимостей. `WebUI` — статический HTML+JS (без сборочного пайплайна),
//! раздаётся этим же сервером.
//!
//! Поддерживается только то, что нужно API: `GET`/`POST`, заголовки, тело по
//! `Content-Length`. Chunked/stream не поддерживается осознанно — API-запросы
//! маленькие, keepalive не делаем (один запрос — одно соединение, проще).

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;

/// Максимальный размер HTTP-запроса (заголовки + тело). API-запросы крошечные;
// потолок защищает от злонамеренной аллокации.
const MAX_REQUEST_LEN: usize = 64 * 1024;
/// Максимум заголовков — защита от бесконечного потока хедеров.
const MAX_HEADERS: usize = 64;

/// HTTP-метод. Только то, что используется API (`GET`/`POST`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
}

/// Разобранный HTTP-запрос. `body` — `Vec<u8>` (POST-тело; для GET — пусто).
#[derive(Debug)]
pub struct Request {
    pub method: Method,
    pub path: String,
    pub body: Vec<u8>,
}

/// Прочитать и разобрать один HTTP/1.1 запрос. `Ok(None)` — чистый EOF (клиент
/// закрыл соединение без запроса). Тело читается по `Content-Length`, если есть.
///
/// # Errors
///
/// `io::Error` при обрыве сокета или превышении лимитов.
pub fn read_request(stream: &mut TcpStream) -> io::Result<Option<Request>> {
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    let n = reader.read_line(&mut request_line)?;
    if n == 0 {
        return Ok(None); // чистый EOF до начала запроса.
    }
    let (method, path) = parse_request_line(&request_line)?;

    // Заголовки до пустой строки. `read_line` оставляет `\r\n`; пустая строка
    // = `\r\n` → длина 2.
    let mut content_length = 0usize;
    for _ in 0..MAX_HEADERS {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 {
            break;
        }
        if header == "\r\n" || header == "\n" {
            break; // конец заголовков.
        }
        if let Some((k, v)) = header.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                if let Ok(len) = v.trim().parse::<usize>() {
                    content_length = len;
                }
            }
        }
    }

    if content_length > MAX_REQUEST_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "request body exceeds limit",
        ));
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }
    Ok(Some(Request {
        method,
        path,
        body,
    }))
}

fn parse_request_line(line: &str) -> io::Result<(Method, String)> {
    let line = line.trim_end();
    let mut parts = line.split_whitespace();
    let method = parts.next().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "empty request line")
    })?;
    let path = parts.next().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "missing request path")
    })?;
    let method = match method {
        "GET" => Method::Get,
        "POST" => Method::Post,
        other => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported method: {other}"),
            ));
        }
    };
    // path приходит как `/api/foo?query=...` — query обрезаем (API его не
    // использует; если понадобится — добавим отдельно).
    let path = path.split('?').next().unwrap_or(path).to_string();
    Ok((method, path))
}

/// HTTP-ответ. `body` — байты (JSON или HTML).
pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

impl Response {
    #[must_use]
    pub fn json(status: u16, body: Vec<u8>) -> Self {
        Self {
            status,
            content_type: "application/json; charset=utf-8",
            body,
        }
    }

    #[must_use]
    pub fn html(status: u16, body: Vec<u8>) -> Self {
        Self {
            status,
            content_type: "text/html; charset=utf-8",
            body,
        }
    }

    #[must_use]
    pub fn text(status: u16, body: &str) -> Self {
        Self {
            status,
            content_type: "text/plain; charset=utf-8",
            body: body.as_bytes().to_vec(),
        }
    }

    fn status_text(&self) -> &'static str {
        match self.status {
            204 => "No Content",
            400 => "Bad Request",
            403 => "Forbidden",
            404 => "Not Found",
            405 => "Method Not Allowed",
            500 => "Internal Server Error",
            _ => "OK",
        }
    }
}

/// Записать HTTP-ответ в сокет и закрыть соединение (без keepalive).
///
/// # Errors
///
/// `io::Error` записи.
pub fn write_response(stream: &mut TcpStream, resp: &Response) -> io::Result<()> {
    let len = resp.body.len();
    let head = format!(
        "HTTP/1.1 {status} {text}\r\nContent-Type: {ct}\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n",
        status = resp.status,
        text = resp.status_text(),
        ct = resp.content_type,
        len = len
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(&resp.body)?;
    stream.flush()
}

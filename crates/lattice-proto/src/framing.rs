//! Length-delimited фрейминг control-канала (TCP). Единый формат для client и
//! server — отсюда обе стороны его берут, чтобы детали не разъехались.
//!
//! TCP — поток байт без границ сообщений, поэтому каждый JSON-кадр предваряем
//! 4-байтным big-endian префиксом длины. Доступен только с фичей `std` (нужен
//! `std::io`); сами типы сообщений (`control`) от std не зависят.

use alloc::vec;
use alloc::vec::Vec;
use std::io::{self, Read, Write};

/// Максимальный размер одного control-кадра. Эти сообщения крошечные
/// (регистрация / endpoint'ы), но префикс длины приходит из сети — без потолка
/// злонамеренный/битый префикс заставил бы аллоцировать гигабайты. 64 KiB —
/// заведомо с запасом для любого сообщения протокола.
pub const MAX_FRAME_LEN: usize = 64 * 1024;

/// Записать один кадр: `[len: u32 BE][json]`. `flush` — чтобы сообщение ушло
/// сразу (control интерактивен; без flush go-сигнал застрял бы в буфере).
///
/// # Errors
///
/// `io::Error` записи (сокет закрыт/разорван) либо `InvalidInput`, если кадр
/// превышает `MAX_FRAME_LEN`.
pub fn write_frame<W: Write>(w: &mut W, json: &[u8]) -> io::Result<()> {
    if json.len() > MAX_FRAME_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "control frame exceeds MAX_FRAME_LEN",
        ));
    }
    let len = u32::try_from(json.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "frame length overflow"))?;
    w.write_all(&len.to_be_bytes())?;
    w.write_all(json)?;
    w.flush()
}

/// Прочитать один кадр. `Ok(None)` — чистый EOF на границе кадра (пир закрыл
/// соединение штатно). `Ok(Some(bytes))` — JSON-тело кадра.
///
/// # Errors
///
/// `UnexpectedEof` — обрыв в середине кадра; `InvalidData` — длина превышает
/// `MAX_FRAME_LEN`; прочие `io::Error` сокета.
pub fn read_frame<R: Read>(r: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    match read_exact_or_eof(r, &mut len_buf)? {
        ReadOutcome::Eof => return Ok(None),
        ReadOutcome::Filled => {}
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "control frame exceeds MAX_FRAME_LEN",
        ));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    Ok(Some(body))
}

enum ReadOutcome {
    Filled,
    Eof,
}

/// `read_exact`, но EOF на самом первом байте — это `Eof`, а не ошибка (нужно
/// отличить штатное закрытие от обрыва посреди кадра).
fn read_exact_or_eof<R: Read>(r: &mut R, buf: &mut [u8]) -> io::Result<ReadOutcome> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) => {
                if filled == 0 {
                    return Ok(ReadOutcome::Eof);
                }
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "EOF in the middle of a control frame",
                ));
            }
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(ReadOutcome::Filled)
}

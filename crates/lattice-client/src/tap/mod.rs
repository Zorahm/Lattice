//! TAP-адаптер (tap-windows6). Единственное место во всём workspace с `unsafe`
//! и Win32 (AGENTS.md «FFI-изоляция»).
//!
//! Почему tap-windows6, а не Wintun: только L2 (TAP) даёт настоящий
//! Ethernet-broadcast/multicast → LAN-discovery для игр (Minecraft LAN, SA:MP,
//! Project Zomboid) работает из коробки. Wintun — L3, ARP/broadcast пришлось
//! бы эмулировать вручную. См. SPEC.md «Почему tap-windows6, а не Wintun».
//!
//! Структура модуля:
//! - `mod.rs` — `TapDevice` (`open` / `read_frame` / `write_frame` /
//!   `set_media_status` / `Drop`) и IOCTL-константы. Здесь же вся overlapped
//!   I/O-логика.
//! - `registry.rs` — поиск адаптера в реестре по `ComponentId=tap0901`:
//!   извлечение GUID (для `\\.\Global\{GUID}.tap`) и connection name (для
//!   `netsh`). Вынесен отдельно чтобы `mod.rs` не превышал лимит файла и
//!   хранил единственную ответственность («device lifecycle» vs «discovery»).

mod registry;
mod win32;

pub use registry::find_tap_adapter;
pub use win32::{check_admin, install_ctrl_handler, CtrlHandler};

use std::fmt;

use thiserror::Error;

#[cfg(windows)]
use registry::encode_wide;
#[cfg(windows)]
use windows_sys::Win32::Foundation::{
    CloseHandle, GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
};
#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_ATTRIBUTE_SYSTEM, FILE_FLAG_OVERLAPPED,
    FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
#[cfg(windows)]
use windows_sys::Win32::System::IO::{DeviceIoControl, GetOverlappedResult, OVERLAPPED};
#[cfg(windows)]
use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

pub use registry::TapAdapterInfo;

/// Максимальный размер Ethernet-фрейма под TAP MTU 1380: header 14 + payload
/// 1380 = 1394. Берём с запасом, чтобы драйвер не молча обрезал.
pub const FRAME_BUF_LEN: usize = 1600;

/// IOCTL-коды tap-windows6. Источник — tap-windows6 `tap.h`, макрос
/// `TAP_CONTROL_CODE(request, method) = CTL_CODE(FILE_DEVICE_UNKNOWN, request,
/// method, FILE_ANY_ACCESS)`. `CTL_CODE(dev, func, method, access) =
/// (dev << 16) | (access << 14) | (func << 2) | method`. У нас
/// `FILE_DEVICE_UNKNOWN = 0x22`, `METHOD_BUFFERED = 0`, `FILE_ANY_ACCESS = 0`
/// (последний не OR'ится, это 0 в битах 14..15, оставлен здесь как комментарий
/// для верности формулы — `` `| 0` `` убран чтобы не триггерить `identity_op`).
///
/// `TAP_WIN_IOCTL_SET_MEDIA_STATUS` = function 6. Включает/выключает линк:
/// входной буфер `[u32: 1|0]`.
#[cfg(windows)]
const TAP_IOCTL_SET_MEDIA_STATUS: u32 = (0x22 << 16) | (6 << 2);

#[derive(Debug, Error)]
pub enum TapError {
    /// Драйвер tap-windows6 не установлен (нет веток с ComponentId=tap0901).
    /// Сообщение с подсказкой поставить драйвер, не сырой Win32 errno.
    #[error("tap-windows6 adapter not found; install the OpenVPN TAP driver \
             (no registry entry with ComponentId='{0}'). \
             Get it from https://github.com/OpenVPN/tap-windows6 or install OpenVPN.")]
    AdapterNotFound(&'static str),
    #[error("registry enumeration failed: {0}")]
    Registry(String),
    #[error("CreateFileW failed for '{path}' (err={code})")]
    OpenFile { path: String, code: u32 },
    #[error("DeviceIoControl {ioctl} failed (err={code})")]
    Ioctl { ioctl: &'static str, code: u32 },
    #[error("overlapped I/O error: {0}")]
    Overlapped(String),
    #[error("CreateEventW failed (err={code})")]
    CreateEvent { code: u32 },
    #[error("I/O cancelled / device closed")]
    Cancelled,
    /// Read timeout — не ошибка, а сигнал циклу «данных нет, проверь shutdown».
    #[error("read timed out")]
    WouldBlock,
    /// Запуск без прав администратора. Поднимается из `win32::check_admin`,
    /// mапится в `NetcfgError::NotElevated` вызывающим.
    #[error("not running as administrator; relaunch elevated (Run as Administrator)")]
    NotElevated,
}

/// Небезопасная обёртка над HANDLE tap-драйвера. `Send + Sync`: overlapped
/// read/write с разными OVERLAPPED структурами из разных потоков безопасны
/// (драйвер сам сериализует доступ); OVERLAPPED не делим между потоками.
pub struct TapDevice {
    #[cfg(windows)]
    handle: HANDLE,
    /// Информация об адаптере — нужна для netcfg (имя) и логов (GUID).
    pub info: TapAdapterInfo,
}

#[cfg(windows)]
unsafe impl Send for TapDevice {}
#[cfg(windows)]
unsafe impl Sync for TapDevice {}

impl TapDevice {
    /// Открыть первый найденный `tap-windows6` адаптер и поднять линк.
    ///
    /// # Errors
    ///
    /// `AdapterNotFound` — драйвер не установлен; `OpenFile` — `CreateFileW`
    /// провалился; `Ioctl` — не удалось поднять media status.
    #[cfg(windows)]
    pub fn open() -> Result<Self, TapError> {
        let info = find_tap_adapter()
            .ok_or(TapError::AdapterNotFound(registry::TAP_COMPONENT_ID))?;
        let path = format!(r"\\.\Global\{}.tap", info.guid);
        let path_w = encode_wide(&path);

        // SAFETY: CreateFileW на существующее устройство. GENERIC_READ|
        // GENERIC_WRITE + overlapped — документированный способ открытия
        // tap-windows6 (SPEC «Заметки по tap-windows6»).
        let handle = unsafe {
            CreateFileW(
                path_w.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_SYSTEM | FILE_FLAG_OVERLAPPED,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(TapError::OpenFile { path, code: unsafe { GetLastError() } });
        }
        let dev = Self { handle, info };
        dev.set_media_status(true)?;
        Ok(dev)
    }

    #[cfg(not(windows))]
    pub fn open() -> Result<Self, TapError> {
        Err(TapError::AdapterNotFound(registry::TAP_COMPONENT_ID))
    }

    /// Переключить media status: `true` = connected (линк вверх), `false` =
    /// disconnected (линк вниз). Вызывается на старте (up) и в `Drop` (down).
    ///
    /// # Errors
    ///
    /// `Ioctl` — `DeviceIoControl(SET_MEDIA_STATUS)` вернул ошибку.
    #[cfg(windows)]
    #[allow(clippy::cast_possible_truncation)]
    pub fn set_media_status(&self, connected: bool) -> Result<(), TapError> {
        let value: u32 = u32::from(connected);
        let mut bytes_returned: u32 = 0;
        // SAFETY: DeviceIoControl с SET_MEDIA_STATUS, входной буфер `&u32` (4
        // байта). Выходного буфера нет (драйвер только читает вход).
        let ok = unsafe {
            DeviceIoControl(
                self.handle,
                TAP_IOCTL_SET_MEDIA_STATUS,
                std::ptr::addr_of!(value).cast::<std::ffi::c_void>(),
                std::mem::size_of::<u32>() as u32,
                std::ptr::null_mut(),
                0,
                &mut bytes_returned,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(TapError::Ioctl {
                ioctl: "TAP_IOCTL_SET_MEDIA_STATUS",
                code: unsafe { GetLastError() },
            });
        }
        Ok(())
    }

    #[cfg(not(windows))]
    pub fn set_media_status(&self, _connected: bool) -> Result<(), TapError> {
        Ok(())
    }

    /// Повторно поднять media-connected ПОСЛЕ внешней реконфигурации адаптера
    /// (`netsh set address` / `set subinterface mtu`). Смена MTU перезапускает
    /// NDIS-минипорт tap-windows6, а рестарт сбрасывает media-status в дефолт
    /// (disconnected). Рестарт может прийти асинхронно — через десятки/сотни мс
    /// после возврата `netsh`, уже ПОСЛЕ одиночного `set_media_status(true)`.
    /// Поэтому дёргаем IOCTL несколько раз с паузами, перекрывая окно рестарта;
    /// IOCTL идемпотентен, лишние вызовы безвредны. Без этого один конец mesh мог
    /// остаться с дохлым линком (нет on-link маршрута, ARP не уходит) — отсюда
    /// асимметрия «я его пингую, он меня нет».
    ///
    /// # Errors
    ///
    /// `Ioctl` — `DeviceIoControl(SET_MEDIA_STATUS)` вернул ошибку.
    #[cfg(windows)]
    pub fn reassert_media_connected(&self) -> Result<(), TapError> {
        for i in 0..6 {
            self.set_media_status(true)?;
            if i < 5 {
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
        }
        Ok(())
    }

    #[cfg(not(windows))]
    pub fn reassert_media_connected(&self) -> Result<(), TapError> {
        Ok(())
    }

    /// Прочитать один Ethernet-фрейм (overlapped, с таймаутом). `buf` ≥
    /// `FRAME_BUF_LEN`. Возвращает `Err(TapError::WouldBlock)` по таймауту —
    /// цикл чтения использует это чтобы переодически проверять флаг shutdown,
    /// не блокируясь в драйвере.
    ///
    /// # Errors
    ///
    /// `WouldBlock` — таймаут (250мс); `Cancelled` — хэндл закрыт; `Overlapped`
    /// — прочая ошибка overlapped I/O; `CreateEvent` — не удалось создать event.
    pub fn read_frame(&self, buf: &mut [u8]) -> Result<usize, TapError> {
        #[cfg(windows)]
        {
            self.overlapped_io(true, buf.as_mut_ptr(), buf.len())
        }
        #[cfg(not(windows))]
        {
            let _ = buf;
            Err(TapError::Cancelled)
        }
    }

    /// Записать один Ethernet-фрейм (overlapped, без таймаута — запись короткая).
    ///
    /// # Errors
    ///
    /// `Cancelled` — хэндл закрыт; `Overlapped` — ошибка overlapped I/O;
    /// `CreateEvent` — не удалось создать event.
    pub fn write_frame(&self, buf: &[u8]) -> Result<usize, TapError> {
        #[cfg(windows)]
        {
            self.overlapped_io(false, buf.as_ptr().cast_mut(), buf.len())
        }
        #[cfg(not(windows))]
        {
            let _ = buf;
            Err(TapError::Cancelled)
        }
    }

    /// overlapped read/write. Буфер жив на протяжении вызова; для read драйвер
    /// пишет в `buf`, для write — только читает. `len` ограничен `FRAME_BUF_LEN`
    /// / `RECV_BUF_LEN` (≤ 65535), поэтому `len as u32` безопасно — cast
    /// truncation здесь физически невозможен.
    #[cfg(windows)]
    #[allow(clippy::cast_possible_truncation, clippy::ptr_cast_constness)]
    fn overlapped_io(&self, is_read: bool, buf: *mut u8, len: usize) -> Result<usize, TapError> {
        // SAFETY: CreateEventW(NULL, manual-reset=TRUE, initial=0, NULL name).
        // Manual-reset нужен, чтобы WaitForSingleObject/GetOverlappedResult
        // дождались реального завершения. Событие закрывается CloseHandle в конце.
        let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if event.is_null() {
            return Err(TapError::CreateEvent { code: unsafe { GetLastError() } });
        }
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        overlapped.hEvent = event;

        let started = if is_read {
            // SAFETY: overlapped ReadFile. Драйвер пишет в buf к моменту
            // завершения ожидания ниже.
            unsafe {
                ReadFile(self.handle, buf, len as u32, std::ptr::null_mut(), &mut overlapped)
            }
        } else {
            // SAFETY: overlapped WriteFile. Драйвер только читает buf.
            unsafe {
                WriteFile(self.handle, buf, len as u32, std::ptr::null_mut(), &mut overlapped)
            }
        };

        // ERROR_IO_PENDING (997) на overlapped — штатно, операция идёт.
        if started == 0 {
            let err = unsafe { GetLastError() };
            if err != 997 {
                unsafe { CloseHandle(event) };
                return Err(TapError::Overlapped(format!(
                    "ReadFile/WriteFile start failed (err={err})"
                )));
            }
        }

        // Для read — ждём с таймаутом 250мс (потом отдаём WouldBlock, чтобы
        // цикл проверил shutdown). Для write — бесконечно: запись короткая и
        // таймаутить её нет смысла, зависание здесь = проблема драйвера.
        let wait_ms: u32 = if is_read { 250 } else { 0xFFFF_FFFF };
        let wait = unsafe { WaitForSingleObject(event, wait_ms) };
        if wait == 258 {
            // WAIT_TIMEOUT — для read это штатный poll, для write (INFINITE)
            // сюда попасть нельзя.
            unsafe { CloseHandle(event) };
            return Err(TapError::WouldBlock);
        }
        if wait != 0 {
            // WAIT_FAILED или другое — диагностируем.
            let err = unsafe { GetLastError() };
            unsafe { CloseHandle(event) };
            return Err(TapError::Overlapped(format!(
                "WaitForSingleObject failed (wait={wait}, err={err})"
            )));
        }

        let mut transferred: u32 = 0;
        // SAFETY: GetOverlappedResult(bWait=FALSE) — событие уже signaled,
        // просто забираем число байт.
        let ok = unsafe { GetOverlappedResult(self.handle, &overlapped, &mut transferred, 0) };
        unsafe { CloseHandle(event) };

        if ok == 0 {
            let err = unsafe { GetLastError() };
            // ERROR_OPERATION_ABORTED (995) — хэндл закрыт (Drop).
            if err == 995 {
                return Err(TapError::Cancelled);
            }
            return Err(TapError::Overlapped(format!(
                "GetOverlappedResult failed (err={err})"
            )));
        }
        Ok(transferred as usize)
    }
}

#[cfg(windows)]
impl Drop for TapDevice {
    fn drop(&mut self) {
        // Опускаем линк перед закрытием хэндла. Ошибки не пробрасываем — Drop
        // без возврата; в крайнем случае драйвер сам оборвёт соединение при
        // CloseHandle. Лог оставляем на вызывающего (через stderr в main).
        let _ = self.set_media_status(false);
        // SAFETY: CloseHandle для хэндла из open(). Drop вызывается один раз.
        unsafe { CloseHandle(self.handle) };
    }
}

// `fmt::Debug` вручную: HANDLE не Debug, а TapAdapterInfo — есть.
impl fmt::Debug for TapDevice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TapDevice")
            .field("info", &self.info)
            .finish_non_exhaustive()
    }
}

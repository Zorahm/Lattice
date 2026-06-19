//! TLS/ALPN/SNI-конфигурация QUIC-транспорта Фазы 4 — В ОДНОМ МЕСТЕ.
//!
//! ## Что и зачем мимикрирует
//!
//! - **ALPN `h3`**: handshake объявляет протокол HTTP/3 — самый частый
//!   легитимный QUIC-трафик. Поток на этапе рукопожатия выглядит как обычный
//!   браузер→CDN, а не «непонятный QUIC».
//! - **SNI настраиваемый**: имя в `ClientHello` (`--sni`), по умолчанию похожее на
//!   обычный сайт. Не domain-fronting (мы не прикрываемся ЧУЖИМ сайтом — это
//!   отдельная большая тема, см. SCOPE «чего НЕ делаем»), просто правдоподобное имя.
//!
//! ## Почему сервер-сертификат не проверяется (двойное шифрование осознанно)
//!
//! Клиент принимает ЛЮБОЙ сертификат пира (`AcceptAnyServerCert`). Это НЕ дыра:
//! доверие и аутентичность датаплейна обеспечивает ВНУТРЕННИЙ слой — общий
//! shared-key ChaCha20-Poly1305 (E2E из Фаз 1-3). QUIC-TLS здесь — ВНЕШНЯЯ
//! обёртка ради маскировки трафика, а не ради аутентификации пира: подделанный
//! QUIC-сертификат не даёт прочитать внутренний ChaCha-слой, а настоящий PKI
//! нам и не нужен (у пиров нет CA-инфраструктуры). Двойное шифрование (QUIC
//! поверх `ChaCha`) — сознательное: внешний слой маскирует, внутренний защищает
//! E2E; relay-сервер по-прежнему видит только ciphertext внутреннего слоя.
//!
//! ## Честная планка
//!
//! ALPN/SNI убирают сигнатуру «известный VPN» и грубую эвристику «непонятный
//! QUIC». Против активного пробинга (сервер отвечает не как настоящий H3-сервер,
//! self-signed cert, поведение под нагрузкой) НЕ тестировалось и НЕ заявляется.

use std::sync::Arc;
use std::time::Duration;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};

use thiserror::Error;

/// ALPN HTTP/3. Один байт-литерал в одном месте — чтобы мимикрия не разъехалась.
const ALPN_H3: &[u8] = b"h3";

/// SNI по умолчанию — правдоподобное имя обычного CDN-хоста. Настраивается
/// (`--sni`). НЕ domain-fronting: мы не прикрываемся реальным сайтом.
pub const DEFAULT_SNI: &str = "www.cloudflare.com";

/// Ошибки построения TLS-конфигурации QUIC.
#[derive(Debug, Error)]
pub enum TlsError {
    #[error("rustls configuration error: {0}")]
    Rustls(#[from] rustls::Error),
    #[error("self-signed certificate generation failed: {0}")]
    Cert(String),
    #[error("quic crypto config rejected: {0}")]
    QuicCrypto(String),
    #[error("invalid SNI '{0}'")]
    Sni(String),
}

/// Верификатор, принимающий ЛЮБОЙ серверный сертификат. Обоснование — см.
/// заголовок модуля: доверие даёт внутренний ChaCha-слой, QUIC-TLS только
/// маскирует. Реализуем все методы как «принято», поддерживаемые схемы берём
/// из ring-провайдера, чтобы рукопожатие выглядело стандартным.
#[derive(Debug)]
struct AcceptAnyServerCert {
    schemes: Vec<SignatureScheme>,
}

impl AcceptAnyServerCert {
    fn new() -> Self {
        Self {
            schemes: rustls::crypto::ring::default_provider()
                .signature_verification_algorithms
                .supported_schemes(),
        }
    }
}

impl ServerCertVerifier for AcceptAnyServerCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        // Осознанно: аутентичность не из QUIC-TLS, а из внутреннего shared-key.
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.schemes.clone()
    }
}

/// Общий QUIC `TransportConfig`: idle-таймаут + keep-alive. keep-alive (PING)
/// держит соединение живым на тишине, idle рвёт мёртвое — без этого живой
/// p2p-туннель мог бы тихо повиснуть. Значения умеренные: keep-alive ниже idle.
fn transport_config() -> quinn::TransportConfig {
    let mut tc = quinn::TransportConfig::default();
    // idle 30с — мёртвый путь не висит вечно (watchdog сессии переустановит).
    tc.max_idle_timeout(Some(
        quinn::IdleTimeout::try_from(Duration::from_secs(30)).unwrap_or_default(),
    ));
    // PING каждые 10с держит NAT-маппинг и idle-таймер живыми.
    tc.keep_alive_interval(Some(Duration::from_secs(10)));
    tc
}

/// Построить QUIC client-config: ALPN h3, любой серверный сертификат, ring как
/// crypto-провайдер (без aws-lc-rs → без C-тулчейна на Windows).
///
/// # Errors
///
/// `TlsError` при сбое сборки rustls/quic-crypto.
pub fn client_config() -> Result<quinn::ClientConfig, TlsError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut crypto = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyServerCert::new()))
        .with_no_client_auth();
    crypto.alpn_protocols = vec![ALPN_H3.to_vec()];
    let quic_crypto = quinn::crypto::rustls::QuicClientConfig::try_from(crypto)
        .map_err(|e| TlsError::QuicCrypto(e.to_string()))?;
    let mut cfg = quinn::ClientConfig::new(Arc::new(quic_crypto));
    cfg.transport_config(Arc::new(transport_config()));
    Ok(cfg)
}

/// Построить QUIC server-config: self-signed cert на `sni`, ALPN h3. Сертификат
/// одноразовый (per-process) — клиент его всё равно не проверяет (см. заголовок).
///
/// # Errors
///
/// `TlsError` при сбое генерации cert / сборки rustls/quic-crypto.
pub fn server_config(sni: &str) -> Result<quinn::ServerConfig, TlsError> {
    // Проверяем, что SNI — валидное DNS-имя (иначе ClientHello будет странным).
    ServerName::try_from(sni.to_string()).map_err(|_| TlsError::Sni(sni.to_string()))?;

    let cert = rcgen::generate_simple_self_signed(vec![sni.to_string()])
        .map_err(|e| TlsError::Cert(e.to_string()))?;
    let cert_der = CertificateDer::from(cert.cert);
    let key_der = PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut crypto = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der.into())?;
    crypto.alpn_protocols = vec![ALPN_H3.to_vec()];
    let quic_crypto = quinn::crypto::rustls::QuicServerConfig::try_from(crypto)
        .map_err(|e| TlsError::QuicCrypto(e.to_string()))?;
    let mut cfg = quinn::ServerConfig::with_crypto(Arc::new(quic_crypto));
    cfg.transport_config(Arc::new(transport_config()));
    Ok(cfg)
}

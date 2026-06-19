//! Настройки приложения. Сериализуются в JSON (camelCase) — формат совпадает с
//! TypeScript-типом `Settings` во фронте. Хранятся в app-config-dir; пустой файл
//! = рабочие дефолты (клиент работает из коробки).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkSettings {
    pub subnet: String,
    pub ip_assign: String, // "auto" | "manual"
    pub overlay_ip: String,
    pub mtu: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerSettings {
    pub coordination: String,
    pub stun: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionSettings {
    pub allow_relay: bool,
    pub listen_port: u16,
    pub keepalive_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSettings {
    pub autostart: bool,
    pub minimize_to_tray: bool,
    pub language: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub network: NetworkSettings,
    pub server: ServerSettings,
    pub connection: ConnectionSettings,
    pub app: AppSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            network: NetworkSettings {
                subnet: "10.66.0.0/24".into(),
                ip_assign: "auto".into(),
                overlay_ip: "10.66.0.1/24".into(),
                mtu: 1380,
            },
            server: ServerSettings {
                coordination: "lattice.zorahm.ru:51821".into(),
                stun: vec![
                    "stun.l.google.com:19302".into(),
                    "stun.cloudflare.com:3478".into(),
                ],
            },
            connection: ConnectionSettings {
                allow_relay: true,
                listen_port: 0,
                keepalive_secs: 15,
            },
            app: AppSettings {
                autostart: false,
                minimize_to_tray: true,
                language: "ru".into(),
            },
        }
    }
}

impl Settings {
    /// Загрузить из файла; если файла нет или он битый — дефолты (не падаем).
    #[must_use]
    pub fn load(path: &PathBuf) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                log::warn!("settings: битый файл ({e}); беру дефолты");
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    /// Сохранить в файл (создаёт каталог при необходимости).
    ///
    /// # Errors
    /// Текст ошибки ввода-вывода/сериализации.
    pub fn save(&self, path: &PathBuf) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(path, text).map_err(|e| e.to_string())
    }
}

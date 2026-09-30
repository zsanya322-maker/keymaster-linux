/// Конфигурация приложения (config.json)
///
/// Чтение/запись настроек в %APPDATA%\KeyMaster Pro\config.json.
/// Повреждённый файл не теряется: перед восстановлением дефолта он копируется
/// в backups/. Валидные legacy-конфиги мигрируются только после backup.
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tracing::{info, warn};

use crate::shared::types::AppConfig;

/// Версия persistence-схемы config.json.
///
/// Как и у профилей, schemaVersion хранится только на дисковой границе и не
/// входит в runtime AppConfig, поэтому frontend/IPC-контракт не меняется.
pub const CONFIG_SCHEMA_VERSION: u32 = 1;

#[derive(Debug)]
enum ConfigDecodeError {
    Invalid(String),
    FutureSchema(u32),
}

impl ConfigDecodeError {
    fn message(&self) -> String {
        match self {
            Self::Invalid(message) => message.clone(),
            Self::FutureSchema(version) => format!(
                "config.json использует более новую schemaVersion={} (поддерживается до {})",
                version, CONFIG_SCHEMA_VERSION
            ),
        }
    }
}

fn config_path() -> Result<PathBuf, String> {
    Ok(super::persistence::app_data_dir()?.join("config.json"))
}

fn validate_config(config: &AppConfig) -> Result<(), String> {
    if !matches!(config.language.as_str(), "ru" | "en") {
        return Err(format!("Неподдерживаемый язык: {}", config.language));
    }
    if !matches!(config.theme.as_str(), "light" | "dark") {
        return Err(format!("Неподдерживаемая тема: {}", config.theme));
    }
    if config.active_profile_id.trim().is_empty() {
        return Err("activeProfileId не может быть пустым".to_string());
    }
    if !config.scale.is_finite() || !(0.5..=2.0).contains(&config.scale) {
        return Err(format!("Некорректный scale: {}", config.scale));
    }
    if !(8..=32).contains(&config.font_size) {
        return Err(format!("Некорректный fontSize: {}", config.font_size));
    }
    if !(2..=32).contains(&config.row_padding) {
        return Err(format!("Некорректный rowPadding: {}", config.row_padding));
    }
    Ok(())
}

fn migrate_config_value(mut value: Value) -> Result<(Value, bool), ConfigDecodeError> {
    let object = value.as_object_mut().ok_or_else(|| {
        ConfigDecodeError::Invalid("Корень config.json должен быть JSON-объектом".to_string())
    })?;

    let mut version = match object.get("schemaVersion") {
        None => 0,
        Some(value) => value
            .as_u64()
            .ok_or_else(|| {
                ConfigDecodeError::Invalid(
                    "schemaVersion config.json должна быть целым неотрицательным числом"
                        .to_string(),
                )
            })?
            .try_into()
            .map_err(|_| {
                ConfigDecodeError::Invalid(
                    "schemaVersion config.json выходит за диапазон u32".to_string(),
                )
            })?,
    };

    if version > CONFIG_SCHEMA_VERSION {
        return Err(ConfigDecodeError::FutureSchema(version));
    }

    let original_version = version;
    while version < CONFIG_SCHEMA_VERSION {
        match version {
            0 => {
                // v0 -> v1: runtime-данные не преобразуются. Добавляется только
                // явная версия persistence-схемы, поэтому старые настройки не
                // теряются и serde(default) продолжает заполнять новые поля.
                object.insert("schemaVersion".to_string(), json!(1));
                version = 1;
            }
            other => {
                return Err(ConfigDecodeError::Invalid(format!(
                    "Нет миграции config.json для schemaVersion={}",
                    other
                )));
            }
        }
    }

    Ok((value, original_version != version))
}

fn decode_config_value(value: Value) -> Result<(AppConfig, Value, bool), ConfigDecodeError> {
    let (migrated, was_migrated) = migrate_config_value(value)?;
    let config: AppConfig = serde_json::from_value(migrated.clone())
        .map_err(|e| ConfigDecodeError::Invalid(format!("Ошибка парсинга config.json: {}", e)))?;
    validate_config(&config).map_err(ConfigDecodeError::Invalid)?;
    Ok((config, migrated, was_migrated))
}

fn parse_config(data: &str) -> Result<(AppConfig, Value, bool), ConfigDecodeError> {
    let raw: Value = serde_json::from_str(data)
        .map_err(|e| ConfigDecodeError::Invalid(format!("Ошибка парсинга config.json: {}", e)))?;
    decode_config_value(raw)
}

fn config_to_value(config: &AppConfig) -> Result<Value, String> {
    validate_config(config)?;
    let mut value =
        serde_json::to_value(config).map_err(|e| format!("Ошибка сериализации config: {}", e))?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| "Сериализованный config не является JSON-объектом".to_string())?;
    object.insert("schemaVersion".to_string(), json!(CONFIG_SCHEMA_VERSION));
    Ok(value)
}

fn backup_config(path: &Path, reason: &str) -> Result<PathBuf, String> {
    let backup_dir = super::persistence::app_data_dir()?.join("backups");
    fs::create_dir_all(&backup_dir)
        .map_err(|e| format!("Не удалось создать папку бэкапов config: {}", e))?;

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| format!("SystemTime error: {}", e))?
        .as_nanos();
    let backup_path = backup_dir.join(format!("config_{}_{}.json", reason, timestamp));

    fs::copy(path, &backup_path)
        .map_err(|e| format!("Не удалось сохранить backup config.json: {}", e))?;
    Ok(backup_path)
}

fn replace_file_atomically(temp_path: &Path, destination: &Path) -> Result<(), String> {
    fs::rename(temp_path, destination)
        .map_err(|e| format!("Атомарная замена config.json не удалась: {}", e))
}

fn write_config_value(path: &Path, value: &Value) -> Result<(), String> {
    let data = serde_json::to_string_pretty(value)
        .map_err(|e| format!("Ошибка сериализации config: {}", e))?;

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| format!("SystemTime error: {}", e))?
        .as_nanos();
    let temp_path = path.with_extension(format!("json.tmp.{}.{}", std::process::id(), nonce));

    let write_result = (|| -> Result<(), String> {
        let mut file = fs::File::create(&temp_path)
            .map_err(|e| format!("Ошибка открытия временного config для записи: {}", e))?;
        file.write_all(data.as_bytes())
            .map_err(|e| format!("Ошибка записи временного config: {}", e))?;
        file.sync_all()
            .map_err(|e| format!("Ошибка синхронизации временного config: {}", e))?;
        drop(file);
        replace_file_atomically(&temp_path, path)
    })();

    if write_result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    write_result
}

fn write_config_file(path: &Path, config: &AppConfig) -> Result<(), String> {
    let value = config_to_value(config)?;
    write_config_value(path, &value)
}

fn recover_invalid_config(path: &Path, error: &str) -> Result<AppConfig, String> {
    warn!("{}; выполняем безопасное восстановление", error);
    let backup_path = backup_config(path, "corrupt")?;
    let config = AppConfig::default();
    if let Err(write_error) = write_config_file(path, &config) {
        return Err(format!(
            "Повреждённый config сохранён в {}, но восстановить рабочий config не удалось: {}",
            backup_path.display(),
            write_error
        ));
    }
    warn!(
        "Повреждённый config сохранён как {}, создан дефолтный config.json",
        backup_path.display()
    );
    Ok(config)
}

/// Загрузить конфигурацию.
///
/// Legacy v0 автоматически читается через serde(default). Перед записью
/// schemaVersion=1 обязательно создаётся backup. Если backup не удался, конфиг
/// всё равно используется в памяти, но исходный legacy-файл не переписывается.
/// Future schema никогда не понижается и не заменяется дефолтом.
pub fn load_config() -> Result<AppConfig, String> {
    let path = config_path()?;

    if !path.exists() {
        info!("Конфиг не найден, создаём дефолтный");
        let config = AppConfig::default();
        write_config_file(&path, &config)?;
        return Ok(config);
    }

    let data =
        fs::read_to_string(&path).map_err(|e| format!("Ошибка чтения config.json: {}", e))?;

    match parse_config(&data) {
        Ok((config, migrated_value, was_migrated)) => {
            if was_migrated {
                match backup_config(&path, "migration") {
                    Ok(backup_path) => {
                        if let Err(error) = write_config_value(&path, &migrated_value) {
                            warn!(
                                "Backup legacy config создан ({}), но schemaVersion не удалось записать: {}. Продолжаем с config в памяти.",
                                backup_path.display(),
                                error
                            );
                        } else {
                            info!(
                                "config.json мигрирован до schemaVersion={} (backup: {})",
                                CONFIG_SCHEMA_VERSION,
                                backup_path.display()
                            );
                        }
                    }
                    Err(error) => {
                        warn!(
                            "Не удалось создать backup перед миграцией config.json: {}. Исходный файл НЕ переписывается; используем мигрированный config только в памяти.",
                            error
                        );
                    }
                }
            } else {
                info!("Конфигурация загружена");
            }
            Ok(config)
        }
        Err(ConfigDecodeError::FutureSchema(version)) => Err(format!(
            "config.json использует более новую schemaVersion={} (поддерживается до {}). Файл оставлен без изменений; обновите KeyMaster Pro.",
            version, CONFIG_SCHEMA_VERSION
        )),
        Err(error @ ConfigDecodeError::Invalid(_)) => {
            recover_invalid_config(&path, &error.message())
        }
    }
}

/// Сохранить проверенную конфигурацию в файл.
pub fn save_config(config: &AppConfig) -> Result<(), String> {
    let path = config_path()?;
    write_config_file(&path, config)?;
    info!("Конфигурация сохранена");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_config_missing_new_fields_uses_defaults_and_migrates() {
        let (parsed, value, migrated) = parse_config(
            r#"{
            "language": "ru",
            "theme": "light",
            "activeProfileId": "1"
        }"#,
        )
        .unwrap();

        assert!(migrated);
        assert_eq!(parsed.language, "ru");
        assert_eq!(parsed.active_profile_id, "1");
        assert_eq!(parsed.scale, AppConfig::default().scale);
        assert!(parsed.kb_hook_enabled);
        assert!(parsed.mouse_hook_enabled);
        assert_eq!(value.get("schemaVersion").and_then(Value::as_u64), Some(1));
    }

    #[test]
    fn invalid_config_values_are_rejected() {
        let mut config = AppConfig::default();
        config.scale = f64::NAN;
        assert!(validate_config(&config).is_err());

        let mut config = AppConfig::default();
        config.language = "unknown".to_string();
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn future_config_schema_is_rejected_without_downgrade() {
        let future = json!({
            "schemaVersion": CONFIG_SCHEMA_VERSION + 1,
            "language": "ru",
            "theme": "light",
            "activeProfileId": "1"
        });

        assert!(matches!(
            decode_config_value(future),
            Err(ConfigDecodeError::FutureSchema(_))
        ));
    }

    #[test]
    fn serialized_config_contains_schema_version() {
        let value = config_to_value(&AppConfig::default()).unwrap();
        assert_eq!(
            value.get("schemaVersion").and_then(Value::as_u64),
            Some(CONFIG_SCHEMA_VERSION as u64)
        );
    }
}

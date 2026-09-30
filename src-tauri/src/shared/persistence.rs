/// Persistence Layer — чтение/запись JSON файлов
///
/// Профили хранятся в $XDG_DATA_HOME/keymaster-linux/profiles/
/// Бэкапы в $XDG_DATA_HOME/keymaster-linux/backups/
/// Конфиг в $XDG_DATA_HOME/keymaster-linux/config.json
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tracing::{error, info, warn};

use crate::shared::constants::MAX_BACKUPS;
use crate::shared::types::Profile;

/// Текущая версия JSON-схемы профиля.
///
/// `schemaVersion` хранится в JSON на границе persistence и не входит в
/// runtime-структуру `Profile`, поэтому существующий IPC/frontend контракт
/// остаётся совместимым.
pub const PROFILE_SCHEMA_VERSION: u32 = 7;

#[derive(Debug, Clone)]
struct LoadedProfile {
    profile: Profile,
    healthy: bool,
}

/// Каталог данных приложения: $XDG_DATA_HOME/keymaster-linux
/// (по умолчанию ~/.local/share/keymaster-linux).
pub fn app_data_dir() -> Result<PathBuf, String> {
    let base = match std::env::var("XDG_DATA_HOME") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => {
            let home =
                std::env::var("HOME").map_err(|e| format!("Не удалось найти HOME: {}", e))?;
            PathBuf::from(home).join(".local/share")
        }
    };
    let dir = base.join("keymaster-linux");
    fs::create_dir_all(&dir).map_err(|e| format!("Не удалось создать {}: {}", dir.display(), e))?;
    Ok(dir)
}

fn profiles_dir() -> Result<PathBuf, String> {
    let dir = app_data_dir()?.join("profiles");
    fs::create_dir_all(&dir).map_err(|e| format!("Не удалось создать {}: {}", dir.display(), e))?;
    Ok(dir)
}

fn backups_dir() -> Result<PathBuf, String> {
    let dir = app_data_dir()?.join("backups");
    fs::create_dir_all(&dir).map_err(|e| format!("Не удалось создать {}: {}", dir.display(), e))?;
    Ok(dir)
}

/// Возвращает путь профиля, не позволяя ID выйти за пределы `profiles`.
///
/// Старые ID остаются допустимыми (включая `1`, UUID и произвольный текст),
/// запрещаются только пустые значения, разделители пути, `.`/`..` и NUL.
fn profile_path(id: &str) -> Result<PathBuf, String> {
    if id.trim().is_empty()
        || id == "."
        || id == ".."
        || id.contains('/')
        || id.contains('\\')
        || id.contains('\0')
    {
        return Err(format!("Недопустимый ID профиля: {:?}", id));
    }

    Ok(profiles_dir()?.join(format!("{}.json", id)))
}

fn migrate_profile_value(mut value: Value) -> Result<(Value, bool), String> {
    let object = value
        .as_object_mut()
        .ok_or_else(|| "Корень профиля должен быть JSON-объектом".to_string())?;

    let mut version = match object.get("schemaVersion") {
        None => 0,
        Some(value) => value
            .as_u64()
            .ok_or_else(|| "schemaVersion должна быть целым неотрицательным числом".to_string())?
            .try_into()
            .map_err(|_| "schemaVersion выходит за диапазон u32".to_string())?,
    };

    if version > PROFILE_SCHEMA_VERSION {
        return Err(format!(
            "Профиль использует более новую схему {} (поддерживается до {})",
            version, PROFILE_SCHEMA_VERSION
        ));
    }

    let original_version = version;
    while version < PROFILE_SCHEMA_VERSION {
        match version {
            0 => {
                // v0 -> v1: историческая миграция — только явная версия схемы.
                object.insert("schemaVersion".to_string(), json!(1));
                version = 1;
            }
            1 => {
                // v1 -> v2: rule model v2. Старые single-key правила остаются
                // семантически теми же, но получают modifiers=0 и tree metadata.
                object
                    .entry("folders".to_string())
                    .or_insert_with(|| json!([]));

                if let Some(rules) = object.get_mut("rules").and_then(Value::as_array_mut) {
                    for (index, rule) in rules.iter_mut().enumerate() {
                        let Some(rule_obj) = rule.as_object_mut() else {
                            continue;
                        };
                        rule_obj
                            .entry("enabled".to_string())
                            .or_insert_with(|| json!(true));
                        rule_obj
                            .entry("folderId".to_string())
                            .or_insert(Value::Null);
                        rule_obj
                            .entry("order".to_string())
                            .or_insert_with(|| json!(index as i32));

                        if let Some(trigger) =
                            rule_obj.get_mut("trigger").and_then(Value::as_object_mut)
                        {
                            let is_keyboard = matches!(
                                trigger.get("type").and_then(Value::as_str),
                                Some("keyDown" | "keyUp")
                            );
                            if is_keyboard {
                                trigger
                                    .entry("modifiers".to_string())
                                    .or_insert_with(|| json!(0));
                            }
                        }

                        for action_field in ["actions", "holdActions"] {
                            let Some(actions) =
                                rule_obj.get_mut(action_field).and_then(Value::as_array_mut)
                            else {
                                continue;
                            };
                            for action in actions {
                                let Some(action_obj) = action.as_object_mut() else {
                                    continue;
                                };
                                if action_obj.get("type").and_then(Value::as_str)
                                    == Some("remapKey")
                                {
                                    action_obj
                                        .entry("modifiers".to_string())
                                        .or_insert_with(|| json!(0));
                                }
                            }
                        }
                    }
                }

                object.insert("schemaVersion".to_string(), json!(2));
                version = 2;
            }
            2 => {
                // v2 -> v3: macro playback controls. Defaults reproduce the old
                // one-shot 1.0x playback exactly.
                if let Some(rules) = object.get_mut("rules").and_then(Value::as_array_mut) {
                    for rule in rules {
                        let Some(rule_obj) = rule.as_object_mut() else {
                            continue;
                        };
                        for action_field in ["actions", "holdActions"] {
                            let Some(actions) =
                                rule_obj.get_mut(action_field).and_then(Value::as_array_mut)
                            else {
                                continue;
                            };
                            for action in actions {
                                let Some(action_obj) = action.as_object_mut() else {
                                    continue;
                                };
                                if action_obj.get("type").and_then(Value::as_str)
                                    == Some("runMacro")
                                {
                                    action_obj.entry("playback".to_string()).or_insert_with(|| {
                                        json!({
                                            "speed": 1.0,
                                            "repeatCount": 1,
                                            "repeatWhileHeld": false
                                        })
                                    });
                                }
                            }
                        }
                    }
                }
                object.insert("schemaVersion".to_string(), json!(3));
                version = 3;
            }
            3 => {
                // v3 -> v4: structured profile bindings/order while retaining linkedApps.
                object.entry("order".to_string()).or_insert(json!(0));
                if !object.contains_key("bindings") {
                    let bindings = object
                        .get("linkedApps")
                        .and_then(Value::as_array)
                        .map(|apps| {
                            apps.iter()
                                .filter_map(Value::as_str)
                                .map(|p| json!({"process":p,"mode":"any"}))
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    object.insert("bindings".to_string(), json!(bindings));
                }
                object.insert("schemaVersion".to_string(), json!(4));
                version = 4;
            }
            4 => {
                // v4 -> v5: explicit text-expansion behavior. `instant` +
                // caseSensitive=true reproduces the legacy String::ends_with matcher.
                if let Some(rules) = object.get_mut("rules").and_then(Value::as_array_mut) {
                    for rule in rules {
                        let Some(rule_obj) = rule.as_object_mut() else {
                            continue;
                        };
                        if let Some(trigger) =
                            rule_obj.get_mut("trigger").and_then(Value::as_object_mut)
                        {
                            if trigger.get("type").and_then(Value::as_str) == Some("typedText") {
                                trigger
                                    .entry("mode".to_string())
                                    .or_insert(json!("instant"));
                                trigger
                                    .entry("delimiters".to_string())
                                    .or_insert(json!(" \t\n.,;:!?"));
                                trigger
                                    .entry("caseSensitive".to_string())
                                    .or_insert(json!(true));
                            }
                        }
                        for action_field in ["actions", "holdActions"] {
                            let Some(actions) =
                                rule_obj.get_mut(action_field).and_then(Value::as_array_mut)
                            else {
                                continue;
                            };
                            for action in actions {
                                let Some(action_obj) = action.as_object_mut() else {
                                    continue;
                                };
                                if action_obj.get("type").and_then(Value::as_str)
                                    == Some("typeText")
                                {
                                    action_obj
                                        .entry("dateFormat".to_string())
                                        .or_insert(json!("dmy"));
                                    action_obj
                                        .entry("timeFormat".to_string())
                                        .or_insert(json!("hm24"));
                                }
                            }
                        }
                    }
                }
                object.insert("schemaVersion".to_string(), json!(5));
                version = 5;
            }
            5 => {
                // v5 -> v6: advanced-input trigger variants are additive. No existing
                // rule payload needs rewriting, so preserve every v5 rule byte-for-byte
                // apart from the schema marker.
                object.insert("schemaVersion".to_string(), json!(6));
                version = 6;
            }
            6 => {
                // v6 -> v7: macros become first-class profile objects. Every legacy
                // inline runMacro is migrated independently (no automatic deduplication),
                // then the rule stores only macroId + playback. Legacy WindowMatch is
                // normalized to ContextMatch(mode=any), preserving its OR semantics.
                let mut macros = object
                    .remove("macros")
                    .and_then(|value| value.as_array().cloned())
                    .unwrap_or_default();
                let mut generated_index = macros.len();

                if let Some(rules) = object.get_mut("rules").and_then(Value::as_array_mut) {
                    for rule in rules {
                        let Some(rule_obj) = rule.as_object_mut() else {
                            continue;
                        };
                        let rule_id = rule_obj
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or("rule")
                            .to_string();
                        let rule_name = rule_obj
                            .get("name")
                            .and_then(Value::as_str)
                            .filter(|name| !name.trim().is_empty())
                            .unwrap_or("Макрос")
                            .to_string();

                        if let Some(conditions) =
                            rule_obj.get_mut("conditions").and_then(Value::as_array_mut)
                        {
                            for condition in conditions {
                                let Some(condition_obj) = condition.as_object_mut() else {
                                    continue;
                                };
                                if condition_obj.get("type").and_then(Value::as_str)
                                    == Some("windowMatch")
                                {
                                    let process =
                                        condition_obj.remove("process").unwrap_or(Value::Null);
                                    let title =
                                        condition_obj.remove("title").unwrap_or(Value::Null);
                                    *condition = json!({
                                        "type": "contextMatch",
                                        "process": process,
                                        "title": title,
                                        "mode": "any"
                                    });
                                }
                            }
                        }

                        for action_field in ["actions", "holdActions"] {
                            let Some(actions) =
                                rule_obj.get_mut(action_field).and_then(Value::as_array_mut)
                            else {
                                continue;
                            };
                            for (action_index, action) in actions.iter_mut().enumerate() {
                                let Some(action_obj) = action.as_object_mut() else {
                                    continue;
                                };
                                if action_obj.get("type").and_then(Value::as_str)
                                    != Some("runMacro")
                                {
                                    continue;
                                }
                                if action_obj.get("macroId").and_then(Value::as_str).is_some() {
                                    action_obj.remove("steps");
                                    continue;
                                }

                                generated_index += 1;
                                let macro_id = format!(
                                    "legacy-macro:{}:{}:{}:{}",
                                    rule_id, action_field, action_index, generated_index
                                );
                                let steps = action_obj.remove("steps").unwrap_or_else(|| json!([]));
                                action_obj.insert("macroId".to_string(), json!(macro_id));
                                macros.push(json!({
                                    "id": macro_id,
                                    "name": format!("{} — макрос {}", rule_name, generated_index),
                                    "steps": steps
                                }));
                            }
                        }
                    }
                }

                object.insert("macros".to_string(), Value::Array(macros));
                object.insert("schemaVersion".to_string(), json!(7));
                version = 7;
            }
            other => return Err(format!("Нет миграции для версии профиля {}", other)),
        }
    }

    Ok((value, original_version != version))
}

fn profile_from_value(value: Value) -> Result<(Profile, Value, bool), String> {
    let (migrated, was_migrated) = migrate_profile_value(value)?;
    let profile = serde_json::from_value::<Profile>(migrated.clone())
        .map_err(|e| format!("Профиль не соответствует runtime-схеме: {}", e))?;
    profile_path(&profile.id)?;
    Ok((profile, migrated, was_migrated))
}

/// Каноническое представление профиля для диска/export.
pub fn export_profile_value(profile: &Profile) -> Result<Value, String> {
    profile_path(&profile.id)?;
    let mut value =
        serde_json::to_value(profile).map_err(|e| format!("Ошибка сериализации профиля: {}", e))?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| "Сериализованный профиль не является JSON-объектом".to_string())?;
    object.insert("schemaVersion".to_string(), json!(PROFILE_SCHEMA_VERSION));
    Ok(value)
}

/// Проверка входного JSON для импорта: legacy schema принимается и мигрируется
/// в памяти, future/malformed schema отклоняется до записи на диск.
pub fn import_profile_value(value: Value) -> Result<Profile, String> {
    let (profile, _, _) = profile_from_value(value)?;
    Ok(profile)
}

fn replace_file_atomically(temp_path: &Path, destination: &Path) -> Result<(), String> {
    fs::rename(temp_path, destination)
        .map_err(|e| format!("Атомарная замена профиля не удалась: {}", e))
}

fn write_profile_value(path: &Path, value: &Value) -> Result<(), String> {
    let data = serde_json::to_string_pretty(value)
        .map_err(|e| format!("Ошибка сериализации профиля: {}", e))?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| format!("SystemTime error: {}", e))?
        .as_nanos();
    let temp_path = path.with_extension(format!("json.tmp.{}.{}", std::process::id(), nonce));

    let write_result = (|| -> Result<(), String> {
        let mut file = fs::File::create(&temp_path)
            .map_err(|e| format!("Ошибка открытия временного профиля: {}", e))?;
        file.write_all(data.as_bytes())
            .map_err(|e| format!("Ошибка записи временного профиля: {}", e))?;
        file.sync_all()
            .map_err(|e| format!("Ошибка синхронизации временного профиля: {}", e))?;
        drop(file);
        replace_file_atomically(&temp_path, path)
    })();

    if write_result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    write_result
}

fn recovery_profile(id: &str) -> Profile {
    Profile {
        id: id.to_string(),
        name: format!("{} (Ошибка загрузки)", id),
        is_default: false,
        linked_apps: vec![],
        bindings: vec![],
        order: 0,
        rules: vec![],
        macros: vec![],
        layers: vec![],
        folders: vec![],
    }
}

fn protect_invalid_source(path: &Path, reason: &str) {
    error!("{}. Файл НЕ перезаписывается: {}", reason, path.display());
    if let Err(backup_err) = backup_file(path) {
        warn!(
            "Не удалось создать защитный backup {}: {}",
            path.display(),
            backup_err
        );
    }
}

fn load_profile_state(id: &str) -> Result<LoadedProfile, String> {
    let path = profile_path(id)?;

    if !path.exists() {
        return Err(format!("Профиль '{}' не найден", id));
    }

    let data = fs::read_to_string(&path)
        .map_err(|e| format!("Ошибка чтения {}: {}", path.display(), e))?;

    let raw_value: Value = match serde_json::from_str(&data) {
        Ok(value) => value,
        Err(e) => {
            protect_invalid_source(&path, &format!("Повреждён JSON профиля: {}", e));
            return Ok(LoadedProfile {
                profile: recovery_profile(id),
                healthy: false,
            });
        }
    };

    let (profile, migrated_value, was_migrated) = match profile_from_value(raw_value) {
        Ok(result) => result,
        Err(e) => {
            protect_invalid_source(
                &path,
                &format!("Не удалось прочитать/мигрировать профиль: {}", e),
            );
            return Ok(LoadedProfile {
                profile: recovery_profile(id),
                healthy: false,
            });
        }
    };

    if profile.id != id {
        protect_invalid_source(
            &path,
            &format!(
                "ID внутри профиля ({}) не совпадает с именем файла ({})",
                profile.id, id
            ),
        );
        return Ok(LoadedProfile {
            profile: recovery_profile(id),
            healthy: false,
        });
    }

    if was_migrated {
        match backup_file(&path) {
            Ok(backup_path) => {
                if let Err(e) = write_profile_value(&path, &migrated_value) {
                    // Backup уже есть, а атомарная запись не удалась. Старый
                    // legacy-файл остаётся пригодным для следующего запуска.
                    warn!(
                        "Backup перед миграцией создан ({}), но schemaVersion профиля '{}' не удалось записать: {}. Используем профиль в памяти.",
                        backup_path.display(),
                        id,
                        e
                    );
                } else {
                    info!(
                        "Профиль '{}' мигрирован до schemaVersion={} (backup: {})",
                        id,
                        PROFILE_SCHEMA_VERSION,
                        backup_path.display()
                    );
                }
            }
            Err(e) => {
                // Никогда не переписываем legacy source без подтверждённого backup.
                warn!(
                    "Не удалось создать backup перед миграцией профиля '{}': {}. Исходный файл НЕ переписывается; используем мигрированную модель только в памяти.",
                    id, e
                );
            }
        }
    }

    Ok(LoadedProfile {
        profile,
        healthy: true,
    })
}

/// Загрузить профиль. Повреждённый/несовместимый source остаётся на месте и
/// представлен в UI временным recovery-профилем.
pub fn load_profile(id: &str) -> Result<Profile, String> {
    Ok(load_profile_state(id)?.profile)
}

/// Загрузить только здоровый профиль. Используется там, где recovery-профиль
/// нельзя безопасно считать настоящим активным профилем (startup/activate).
pub fn load_profile_checked(id: &str) -> Result<Profile, String> {
    let loaded = load_profile_state(id)?;
    if loaded.healthy {
        Ok(loaded.profile)
    } else {
        Err(format!(
            "Профиль '{}' повреждён или несовместим; исходный файл оставлен без изменений",
            id
        ))
    }
}

/// Проверяет, можно ли безопасно перезаписывать уже существующий профиль.
fn existing_profile_is_safe(path: &Path) -> Result<bool, String> {
    if !path.exists() {
        return Ok(true);
    }

    let expected_id = path
        .file_stem()
        .and_then(|value| value.to_str())
        .ok_or_else(|| format!("Не удалось определить ID из {}", path.display()))?;
    let data =
        fs::read_to_string(path).map_err(|e| format!("Ошибка чтения {}: {}", path.display(), e))?;
    let raw: Value = match serde_json::from_str(&data) {
        Ok(value) => value,
        Err(_) => return Ok(false),
    };
    let (profile, _, _) = match profile_from_value(raw) {
        Ok(result) => result,
        Err(_) => return Ok(false),
    };

    Ok(profile.id == expected_id)
}

/// Сохранить профиль в JSON файл.
///
/// Любая перезапись существующего профиля требует успешного backup. Если backup
/// создать нельзя, source остаётся нетронутым и save возвращает ошибку.
pub fn save_profile(profile: &Profile) -> Result<(), String> {
    let path = profile_path(&profile.id)?;

    if path.exists() {
        if !existing_profile_is_safe(&path)? {
            return Err(format!(
                "Сохранение профиля '{}' заблокировано: исходный файл повреждён или несовместим. Он оставлен без изменений; используйте защитный backup для восстановления.",
                profile.id
            ));
        }

        let backup_path = backup_file(&path).map_err(|e| {
            format!(
                "Сохранение профиля '{}' отменено: не удалось создать обязательный backup: {}",
                profile.id, e
            )
        })?;
        info!(
            "Backup перед сохранением '{}': {}",
            profile.id,
            backup_path.display()
        );
    }

    let value = export_profile_value(profile)?;
    write_profile_value(&path, &value)?;
    info!("Профиль '{}' сохранён", profile.id);
    Ok(())
}

pub fn list_profiles() -> Result<Vec<String>, String> {
    let dir = profiles_dir()?;
    if !dir.exists() {
        return Ok(vec![]);
    }

    let mut profiles = Vec::new();
    for entry in
        fs::read_dir(&dir).map_err(|e| format!("Ошибка чтения {}: {}", dir.display(), e))?
    {
        let entry = entry.map_err(|e| format!("Ошибка entry: {}", e))?;
        if let Some(name) = entry.file_name().to_str() {
            if name.ends_with(".json") {
                profiles.push(name.trim_end_matches(".json").to_string());
            }
        }
    }

    profiles.sort();
    Ok(profiles)
}

/// Удаление также считается destructive-операцией и запрещается без backup.
pub fn delete_profile(id: &str) -> Result<(), String> {
    let path = profile_path(id)?;

    if !path.exists() {
        return Err(format!("Профиль '{}' не найден", id));
    }

    let backup_path = backup_file(&path).map_err(|e| {
        format!(
            "Удаление профиля '{}' отменено: не удалось создать обязательный backup: {}",
            id, e
        )
    })?;

    fs::remove_file(&path).map_err(|e| format!("Ошибка удаления {}: {}", path.display(), e))?;

    info!(
        "Профиль '{}' удалён; backup сохранён: {}",
        id,
        backup_path.display()
    );
    Ok(())
}

pub fn list_profile_backups(id: &str) -> Result<Vec<String>, String> {
    profile_path(id)?;
    let dir = backups_dir()?;
    let prefix = format!("{}_", id);
    let mut out = Vec::new();
    for e in fs::read_dir(&dir).map_err(|e| e.to_string())? {
        let e = e.map_err(|e| e.to_string())?;
        let n = e.file_name().to_string_lossy().to_string();
        if n.starts_with(&prefix) && n.ends_with(".json") {
            out.push(n)
        }
    }
    out.sort_by(|a, b| b.cmp(a));
    Ok(out)
}
pub fn create_profile_backup(id: &str) -> Result<String, String> {
    let path = profile_path(id)?;
    if !path.exists() {
        return Err("Profile not found".into());
    }
    Ok(backup_file(&path)?
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string())
}
pub fn restore_profile_backup(id: &str, name: &str) -> Result<Profile, String> {
    let target = profile_path(id)?;
    if name.contains('/')
        || name.contains('\\')
        || !name.starts_with(&format!("{}_", id))
        || !name.ends_with(".json")
    {
        return Err("Invalid backup name".into());
    }
    let src = backups_dir()?.join(name);
    let data = fs::read_to_string(&src).map_err(|e| e.to_string())?;
    let value: Value = serde_json::from_str(&data).map_err(|e| e.to_string())?;
    let (profile, _, _) = profile_from_value(value)?;
    if profile.id != id {
        return Err("Backup profile id mismatch".into());
    }
    if target.exists() {
        let _ = backup_file(&target)?;
    }
    write_profile_value(&target, &export_profile_value(&profile)?)?;
    Ok(profile)
}

fn backup_file(path: &Path) -> Result<PathBuf, String> {
    let dir = backups_dir()?;
    let filename = path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or("Не удалось извлечь имя файла")?;

    let timestamp = timestamp_for_filename()?;
    let backup_name = format!("{}_{}.json", filename, timestamp);
    let backup_path = dir.join(&backup_name);

    fs::copy(path, &backup_path).map_err(|e| format!("Ошибка копирования backup: {}", e))?;

    // Ошибка rotation не отменяет уже успешно созданный backup и не должна
    // превращать безопасную операцию в ложный failure.
    if let Err(e) = rotate_backups(filename) {
        warn!("Не удалось ротировать backup профиля '{}': {}", filename, e);
    }

    Ok(backup_path)
}

fn rotate_backups(profile_id: &str) -> Result<(), String> {
    let dir = backups_dir()?;
    let prefix = format!("{}_", profile_id);
    let mut backups: Vec<(String, std::time::SystemTime)> = Vec::new();

    for entry in fs::read_dir(&dir).map_err(|e| format!("Ошибка чтения backup: {}", e))?
    {
        let entry = entry.map_err(|e| format!("Ошибка entry: {}", e))?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if name_str.starts_with(&prefix) && name_str.ends_with(".json") {
            let modified = entry
                .metadata()
                .map_err(|e| format!("Ошибка metadata: {}", e))?
                .modified()
                .map_err(|e| format!("Ошибка modified: {}", e))?;
            backups.push((name_str.to_string(), modified));
        }
    }

    backups.sort_by(|a, b| b.1.cmp(&a.1));
    for (name, _) in backups.iter().skip(MAX_BACKUPS) {
        let path = dir.join(name);
        if let Err(e) = fs::remove_file(&path) {
            warn!("Не удалось удалить старый backup {}: {}", name, e);
        }
    }
    Ok(())
}

fn timestamp_for_filename() -> Result<String, String> {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| format!("SystemTime error: {}", e))?;
    Ok(ts.as_nanos().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_invalid_json_is_not_overwritten() {
        let dir = profiles_dir().unwrap();
        let id = "test_invalid";
        let path = dir.join(format!("{}.json", id));
        let invalid = "invalid_json_data";
        fs::write(&path, invalid).unwrap();

        let profile = load_profile(id).unwrap();
        assert_eq!(profile.id, id);
        assert!(profile.name.contains("Ошибка загрузки"));
        assert!(profile.rules.is_empty());
        assert_eq!(fs::read_to_string(&path).unwrap(), invalid);
        assert!(load_profile_checked(id).is_err());

        let save_result = save_profile(&profile);
        assert!(save_result.is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), invalid);

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_legacy_profile_is_migrated_without_data_loss() {
        let dir = profiles_dir().unwrap();
        let id = "test_legacy_migration";
        let path = dir.join(format!("{}.json", id));
        let legacy = json!({
            "id": id,
            "name": "Legacy",
            "isDefault": false,
            "linkedApps": [],
            "rules": [],
            "layers": []
        });
        fs::write(&path, serde_json::to_string_pretty(&legacy).unwrap()).unwrap();

        let profile = load_profile_checked(id).unwrap();
        assert_eq!(profile.name, "Legacy");

        let migrated: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            migrated.get("schemaVersion").and_then(Value::as_u64),
            Some(PROFILE_SCHEMA_VERSION as u64)
        );
        assert_eq!(migrated.get("name").and_then(Value::as_str), Some("Legacy"));

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_save_load_round_trip_writes_schema_version() {
        let profile = Profile {
            id: "test_rt".to_string(),
            name: "Round Trip".to_string(),
            is_default: false,
            linked_apps: vec![],
            bindings: vec![],
            order: 0,
            rules: vec![],
            macros: vec![],
            layers: vec![],
            folders: vec![],
        };

        save_profile(&profile).unwrap();
        let loaded = load_profile_checked("test_rt").unwrap();
        assert_eq!(loaded.id, "test_rt");
        assert_eq!(loaded.name, "Round Trip");

        let path = profiles_dir().unwrap().join("test_rt.json");
        let saved: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            saved.get("schemaVersion").and_then(Value::as_u64),
            Some(PROFILE_SCHEMA_VERSION as u64)
        );

        let _ = delete_profile("test_rt");
    }

    #[test]
    fn test_profile_path_rejects_traversal() {
        assert!(profile_path("../outside").is_err());
        assert!(profile_path("..\\outside").is_err());
        assert!(profile_path("folder/name").is_err());
        assert!(profile_path("").is_err());
        assert!(profile_path("1").is_ok());
        assert!(profile_path("550e8400-e29b-41d4-a716-446655440000").is_ok());
    }

    #[test]
    fn test_future_or_malformed_schema_is_not_overwritten() {
        let dir = profiles_dir().unwrap();

        for (id, schema_version) in [
            ("test_future_schema", json!(PROFILE_SCHEMA_VERSION + 1)),
            ("test_bad_schema", json!("1")),
        ] {
            let path = dir.join(format!("{}.json", id));
            let original = json!({
                "schemaVersion": schema_version,
                "id": id,
                "name": "Protected",
                "isDefault": false,
                "linkedApps": [],
                "rules": [],
                "layers": []
            });
            let original_text = serde_json::to_string_pretty(&original).unwrap();
            fs::write(&path, &original_text).unwrap();

            let recovery = load_profile(id).unwrap();
            assert!(recovery.name.contains("Ошибка загрузки"));
            assert!(load_profile_checked(id).is_err());
            assert_eq!(fs::read_to_string(&path).unwrap(), original_text);
            assert!(save_profile(&recovery).is_err());
            assert_eq!(fs::read_to_string(&path).unwrap(), original_text);

            let _ = fs::remove_file(&path);
        }
    }

    #[test]
    fn test_mismatched_internal_id_is_not_migrated_over_source() {
        let dir = profiles_dir().unwrap();
        let id = "test_id_mismatch";
        let path = dir.join(format!("{}.json", id));
        let original = json!({
            "id": "different-id",
            "name": "Mismatch",
            "isDefault": false,
            "linkedApps": [],
            "rules": [],
            "layers": []
        });
        let original_text = serde_json::to_string_pretty(&original).unwrap();
        fs::write(&path, &original_text).unwrap();

        let recovery = load_profile(id).unwrap();
        assert_eq!(recovery.id, id);
        assert!(recovery.name.contains("Ошибка загрузки"));
        assert!(load_profile_checked(id).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), original_text);

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_export_value_includes_schema_version() {
        let profile = Profile {
            id: "export-test".to_string(),
            name: "Export".to_string(),
            is_default: false,
            linked_apps: vec![],
            bindings: vec![],
            order: 0,
            rules: vec![],
            macros: vec![],
            layers: vec![],
            folders: vec![],
        };
        let value = export_profile_value(&profile).unwrap();
        assert_eq!(
            value.get("schemaVersion").and_then(Value::as_u64),
            Some(PROFILE_SCHEMA_VERSION as u64)
        );
    }

    #[test]
    fn test_import_accepts_legacy_and_rejects_future_schema() {
        let legacy = json!({
            "id": "legacy-import",
            "name": "Legacy Import",
            "isDefault": false,
            "linkedApps": [],
            "rules": [],
            "layers": []
        });
        assert_eq!(import_profile_value(legacy).unwrap().id, "legacy-import");

        let future = json!({
            "schemaVersion": PROFILE_SCHEMA_VERSION + 1,
            "id": "future-import",
            "name": "Future Import",
            "isDefault": false,
            "linkedApps": [],
            "rules": [],
            "layers": []
        });
        assert!(import_profile_value(future).is_err());
    }
}

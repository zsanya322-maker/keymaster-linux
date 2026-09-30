use std::fs;
/// Logging setup — tracing subscriber
///
/// Логирование в файл $XDG_DATA_HOME/keymaster-linux/logs/
/// Каждый запуск daemon создаёт новый файл daemon-YYYY-MM-DD_HH-MM-SS.log,
/// чтобы было удобно отлаживать конкретную сессию без 700к-строчного монолита.
/// Старые файлы не удаляются автоматически (чистка ручная).
use tracing_subscriber::{EnvFilter, fmt};

/// Инициализировать логгер
pub fn init_logging() -> Result<(), String> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        if cfg!(debug_assertions) {
            EnvFilter::new("debug")
        } else {
            EnvFilter::new("info")
        }
    });

    // Путь к data dir/logs
    let log_dir = crate::shared::persistence::app_data_dir()?.join("logs");

    // Создаем директорию логов
    fs::create_dir_all(&log_dir).map_err(|e| {
        format!(
            "Не удалось создать папку логов {}: {}",
            log_dir.display(),
            e
        )
    })?;

    // Имя файла содержит дату/время старта сессии — каждая сессия в свой файл.
    let timestamp = session_timestamp();
    let log_path = log_dir.join(format!("daemon-{}.log", timestamp));

    // Открываем файл для записи (новый файл = обрезаем, если случайно совпало имя)
    let file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&log_path)
        .map_err(|e| {
            format!(
                "Не удалось открыть файл логов {}: {}",
                log_path.display(),
                e
            )
        })?;

    // Инициализируем подписчик с выводом в файл
    fmt()
        .with_env_filter(filter)
        .with_writer(file)
        .with_target(true)
        .with_thread_ids(false)
        .with_file(false)
        .with_line_number(false)
        .init();

    // Печатаем заголовок сессии — удобно искать нужный файл в папке с логами.
    tracing::info!(
        "=== KeyMaster Linux daemon session started at {} ===",
        timestamp
    );
    tracing::info!("Log file: {}", log_path.display());

    Ok(())
}

/// Локальный timestamp в формате YYYY-MM-DD_HH-MM-SS для имени файла лога.
/// Использует chrono если доступен, иначе — ручное форматирование через std.
fn session_timestamp() -> String {
    // Пробуем chrono (зависимость проекта — см. Cargo.toml)
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(dur) => {
            let secs = dur.as_secs();
            // Перевод секунд UTC → дата/время. Не используем chrono, чтобы не плодить
            // зависимостей в этом модуле; точность до секунды, часовой пояс UTC.
            // Локаль не критична — имя файла нужно только для уникальности и сортировки.
            let (year, month, day, hour, minute, second) = secs_to_ymd_hms(secs);
            format!(
                "{:04}-{:02}-{:02}_{:02}-{:02}-{:02}",
                year, month, day, hour, minute, second
            )
        }
        Err(_) => "unknown".to_string(),
    }
}

/// Перевод UNIX-секунд (UTC) в (год, месяц, день, час, минута, секунда).
/// Алгоритм — адаптация Howard Hinnant'а (chrono-автора) для civil_from_days.
fn secs_to_ymd_hms(secs: u64) -> (u32, u32, u32, u32, u32, u32) {
    let days = (secs / 86400) as i64;
    let remainder = (secs % 86400) as u32;
    let hour = remainder / 3600;
    let minute = (remainder % 3600) / 60;
    let second = remainder % 60;

    // days — количество дней с 1970-01-01. Сдвигаем в эпоху Howard Hinnant.
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = (y + if m <= 2 { 1 } else { 0 }) as u32;

    (year, m as u32, d as u32, hour, minute, second)
}

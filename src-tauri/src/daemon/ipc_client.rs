/// IPC Unix Socket Client
///
/// Клиент для GUI-процесса. Подключается к Unix-сокету daemon'а,
/// отправляет JSON-RPC 2.0 запросы и получает ответы.
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tracing::debug;

use super::ipc_types::*;
use crate::shared::constants;

const IPC_IO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const IPC_CONNECT_ATTEMPTS: usize = 20;
const IPC_CONNECT_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(25);
static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

/// Отправить JSON-RPC запрос в Daemon и получить ответ.
///
/// Каждый вызов использует отдельное соединение. Это дороже постоянного socket,
/// зато исключает смешивание ответов между параллельными UI-командами и делает
/// reconnect после daemon restart естественным.
pub async fn call(
    method: &str,
    params: Option<serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let socket_path = constants::ipc_socket_path();
    debug!("IPC Client → {} {:?}", method, params);

    let mut stream = None;
    let mut last_err = None;

    // Startup/restart окна иногда длиннее 100 мс. Даём до ~500 мс на transient
    // race, не превращая отсутствие daemon в многосекундное зависание UI.
    for _ in 0..IPC_CONNECT_ATTEMPTS {
        match UnixStream::connect(&socket_path).await {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(e) => {
                last_err = Some(e);
                tokio::time::sleep(IPC_CONNECT_RETRY_DELAY).await;
            }
        }
    }

    let stream = match stream {
        Some(s) => s,
        None => {
            let err_msg = last_err
                .map(|e| e.to_string())
                .unwrap_or_else(|| "Unknown error".to_string());
            return Err(format!(
                "Failed to connect to daemon socket {}: {}. Is daemon running?",
                socket_path.display(),
                err_msg
            ));
        }
    };

    let request_id = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
    let request_id_value = serde_json::json!(request_id);
    let request = JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        method: method.to_string(),
        params,
        id: Some(request_id_value.clone()),
    };

    let mut request_bytes = serde_json::to_string(&request)
        .map_err(|e| format!("Ошибка сериализации запроса: {}", e))?;
    request_bytes.push('\n');

    let (reader, mut writer) = tokio::io::split(stream);
    tokio::time::timeout(IPC_IO_TIMEOUT, async {
        writer
            .write_all(request_bytes.as_bytes())
            .await
            .map_err(|e| format!("Ошибка отправки запроса: {}", e))?;
        writer
            .flush()
            .await
            .map_err(|e| format!("Ошибка flush IPC-запроса: {}", e))?;
        Ok::<(), String>(())
    })
    .await
    .map_err(|_| {
        format!(
            "IPC timeout: не удалось отправить '{}' за {} сек.",
            method,
            IPC_IO_TIMEOUT.as_secs()
        )
    })??;

    let mut lines = BufReader::new(reader).lines();
    let response_line = tokio::time::timeout(IPC_IO_TIMEOUT, lines.next_line())
        .await
        .map_err(|_| {
            format!(
                "IPC timeout: daemon не ответил на '{}' за {} сек.",
                method,
                IPC_IO_TIMEOUT.as_secs()
            )
        })?
        .map_err(|e| format!("Ошибка чтения ответа: {}", e))?
        .ok_or("Пустой ответ от daemon")?;

    let response: JsonRpcResponse = serde_json::from_str(&response_line)
        .map_err(|e| format!("Ошибка парсинга ответа: {}: '{}'", e, response_line))?;

    debug!("IPC Client ← {:?}", response);

    if response.jsonrpc != "2.0" {
        return Err(format!(
            "IPC protocol error: ожидался jsonrpc=2.0, получено {:?}",
            response.jsonrpc
        ));
    }
    if response.id != request_id_value {
        return Err(format!(
            "IPC protocol error: response id {} не совпадает с request id {}",
            response.id, request_id
        ));
    }

    if let Some(err) = response.error {
        return Err(format!("IPC error [{}]: {}", err.code, err.message));
    }

    response
        .result
        .ok_or_else(|| "IPC: пустой result".to_string())
}

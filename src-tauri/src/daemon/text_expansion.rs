use std::time::{Duration, Instant};

use crate::schemas::engine::EngineAction;
use crate::schemas::frontend::{TextDateFormat, TextTimeFormat};

pub const TEXT_BUFFER_MAX_CHARS: usize = 256;
pub const TEXT_BUFFER_TIMEOUT: Duration = Duration::from_secs(5);
pub const TEXT_UNDO_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone)]
pub struct TextUndoRecord {
    pub original_input: String,
    pub inserted_text: String,
    pub chars_removed: usize,
    pub timestamp: Instant,
    pub window_id: isize,
}

#[derive(Debug)]
pub struct TextInputState {
    pub buffer: String,
    pub last_input: Option<Instant>,
    pub window_id: isize,
    pub undo: Option<TextUndoRecord>,
}

impl Default for TextInputState {
    fn default() -> Self {
        Self {
            buffer: String::new(),
            last_input: None,
            window_id: 0,
            undo: None,
        }
    }
}

impl TextInputState {
    pub fn prepare(&mut self, now: Instant, window_id: isize) {
        if self.window_id != window_id {
            self.window_id = window_id;
            self.clear_all();
            return;
        }
        if self
            .last_input
            .is_some_and(|last| now.duration_since(last) > TEXT_BUFFER_TIMEOUT)
        {
            self.buffer.clear();
            self.last_input = None;
        }
        if self
            .undo
            .as_ref()
            .is_some_and(|undo| now.duration_since(undo.timestamp) > TEXT_UNDO_TIMEOUT)
        {
            self.undo = None;
        }
    }

    pub fn clear_buffer(&mut self) {
        self.buffer.clear();
        self.last_input = None;
    }

    pub fn clear_all(&mut self) {
        self.clear_buffer();
        self.undo = None;
    }

    pub fn note_printable(&mut self, text: String, now: Instant) {
        self.buffer = trim_to_last_chars(text, TEXT_BUFFER_MAX_CHARS);
        self.last_input = Some(now);
    }

    pub fn pop_backspace(&mut self, now: Instant) {
        self.buffer.pop();
        self.last_input = Some(now);
        self.undo = None;
    }

    pub fn set_undo(&mut self, record: TextUndoRecord) {
        self.undo = Some(record);
    }

    pub fn take_undo(&mut self, now: Instant, window_id: isize) -> Option<TextUndoRecord> {
        self.prepare(now, window_id);
        let valid = self.undo.as_ref().is_some_and(|undo| {
            undo.window_id == window_id && now.duration_since(undo.timestamp) <= TEXT_UNDO_TIMEOUT
        });
        if valid {
            self.undo.take()
        } else {
            self.undo = None;
            None
        }
    }
}

pub fn trim_to_last_chars(value: String, max_chars: usize) -> String {
    let count = value.chars().count();
    if count <= max_chars {
        return value;
    }
    let skip = count - max_chars;
    let byte = value
        .char_indices()
        .nth(skip)
        .map(|(idx, _)| idx)
        .unwrap_or(value.len());
    value[byte..].to_string()
}

pub fn suffix_matches(buffer: &str, sequence: &str, case_sensitive: bool) -> bool {
    if sequence.is_empty() {
        return false;
    }
    if case_sensitive {
        buffer.ends_with(sequence)
    } else {
        buffer.to_lowercase().ends_with(&sequence.to_lowercase())
    }
}

pub fn suffix_chars(value: &str, count: usize) -> String {
    let total = value.chars().count();
    value.chars().skip(total.saturating_sub(count)).collect()
}

pub fn backspaces_for(
    mode: crate::schemas::frontend::TextExpansionMode,
    sequence_chars: usize,
) -> usize {
    match mode {
        crate::schemas::frontend::TextExpansionMode::Instant => sequence_chars.saturating_sub(1),
        crate::schemas::frontend::TextExpansionMode::Delimiter => sequence_chars,
    }
}

/// Delimiter config is readable in JSON/UI: literal chars plus `\\t` and `\\n`.
pub fn delimiter_contains(config: &str, value: char) -> bool {
    config.contains(value)
        || (value == '\t' && config.contains("\\t"))
        || (value == '\n' && config.contains("\\n"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DateTimeParts {
    pub year: u16,
    pub month: u16,
    pub day: u16,
    pub hour: u16,
    pub minute: u16,
    pub second: u16,
}

fn format_date(parts: DateTimeParts, format: TextDateFormat) -> String {
    match format {
        TextDateFormat::Dmy => format!("{:02}.{:02}.{:04}", parts.day, parts.month, parts.year),
        TextDateFormat::Ymd => format!("{:04}-{:02}-{:02}", parts.year, parts.month, parts.day),
        TextDateFormat::Mdy => format!("{:02}/{:02}/{:04}", parts.month, parts.day, parts.year),
    }
}

fn format_time(parts: DateTimeParts, format: TextTimeFormat) -> String {
    match format {
        TextTimeFormat::Hm24 => format!("{:02}:{:02}", parts.hour, parts.minute),
        TextTimeFormat::Hms24 => {
            format!("{:02}:{:02}:{:02}", parts.hour, parts.minute, parts.second)
        }
        TextTimeFormat::Hm12 => {
            let suffix = if parts.hour < 12 { "AM" } else { "PM" };
            let hour = match parts.hour % 12 {
                0 => 12,
                value => value,
            };
            format!("{}:{:02} {}", hour, parts.minute, suffix)
        }
    }
}

pub fn render_template_with(
    template: &str,
    date_format: TextDateFormat,
    time_format: TextTimeFormat,
    parts: DateTimeParts,
    clipboard: Option<&str>,
) -> String {
    let mut rendered = template.to_string();
    if rendered.contains("{{date}}") {
        rendered = rendered.replace("{{date}}", &format_date(parts, date_format));
    }
    if rendered.contains("{{time}}") {
        rendered = rendered.replace("{{time}}", &format_time(parts, time_format));
    }
    if rendered.contains("{{clipboard}}") {
        rendered = rendered.replace("{{clipboard}}", clipboard.unwrap_or(""));
    }
    rendered
}

/// Локальное время через /proc/self/stat нельзя получить, поэтому читаем
/// локальную зону из системы: используем libc-независимый путь через
/// `date`-подобное вычисление из UTC + смещения TZ. Для шаблонных дат
/// точность до минуты несущественна — берём UTC, если TZ не удалось разобрать.
fn local_parts() -> DateTimeParts {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64 + local_utc_offset_secs())
        .unwrap_or(0);
    let (year, month, day, hour, minute, second) = secs_to_ymd_hms(secs.max(0) as u64);
    DateTimeParts {
        year: year as u16,
        month: month as u16,
        day: day as u16,
        hour: hour as u16,
        minute: minute as u16,
        second: second as u16,
    }
}

/// Смещение локальной зоны в секундах: TZ-переменная вида UTC+H/-H или
/// /etc/localtime через `date +%z` (быстро и без внешних зависимостей).
fn local_utc_offset_secs() -> i64 {
    use std::process::Command;
    Command::new("date")
        .arg("+%z")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .and_then(|text| {
            let trimmed = text.trim();
            let (sign, rest) = if let Some(rest) = trimmed.strip_prefix('+') {
                (1i64, rest)
            } else if let Some(rest) = trimmed.strip_prefix('-') {
                (-1i64, rest)
            } else {
                return None;
            };
            if rest.len() != 4 {
                return None;
            }
            let hours: i64 = rest[..2].parse().ok()?;
            let minutes: i64 = rest[2..].parse().ok()?;
            Some(sign * (hours * 3600 + minutes * 60))
        })
        .unwrap_or(0)
}

/// Перевод UNIX-секунд в (год, месяц, день, час, минута, секунда).
/// Алгоритм — адаптация Howard Hinnant'а (civil_from_days).
fn secs_to_ymd_hms(secs: u64) -> (u32, u32, u32, u32, u32, u32) {
    let days = (secs / 86400) as i64;
    let remainder = (secs % 86400) as u32;
    let hour = remainder / 3600;
    let minute = (remainder % 3600) / 60;
    let second = remainder % 60;

    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = (y + if m <= 2 { 1 } else { 0 }) as u32;

    (year, m as u32, d as u32, hour, minute, second)
}

/// Текст буфера обмена через arboard (поддерживает X11 и Wayland).
fn read_clipboard_text() -> Option<String> {
    match arboard::Clipboard::new() {
        Ok(mut clipboard) => clipboard.get_text().ok(),
        Err(_) => None,
    }
}

/// Dynamic sources are lazy. In particular clipboard APIs are never touched
/// unless this exact fired template contains `{{clipboard}}`.
pub fn render_template(
    template: &str,
    date_format: TextDateFormat,
    time_format: TextTimeFormat,
) -> String {
    let needs_clock = template.contains("{{date}}") || template.contains("{{time}}");
    let parts = if needs_clock {
        local_parts()
    } else {
        DateTimeParts {
            year: 1970,
            month: 1,
            day: 1,
            hour: 0,
            minute: 0,
            second: 0,
        }
    };
    let clipboard = if template.contains("{{clipboard}}") {
        read_clipboard_text()
    } else {
        None
    };
    render_template_with(
        template,
        date_format,
        time_format,
        parts,
        clipboard.as_deref(),
    )
}

/// Render every TypeText action exactly once for a fired expansion. The returned
/// actions use TypeTextLiteral so clipboard/date contents cannot be interpreted
/// a second time. Undo is offered only if the complete expansion is text-only.
pub fn materialize_text_actions(actions: &[EngineAction]) -> (Vec<EngineAction>, Option<String>) {
    let mut out = Vec::with_capacity(actions.len());
    let mut concatenated = String::new();
    let mut text_only = true;
    for action in actions {
        match action {
            EngineAction::TypeText {
                text,
                date_format,
                time_format,
            } => {
                let rendered = render_template(text, *date_format, *time_format);
                concatenated.push_str(&rendered);
                out.push(EngineAction::TypeTextLiteral { text: rendered });
            }
            EngineAction::TypeTextLiteral { text } => {
                concatenated.push_str(text);
                out.push(action.clone());
            }
            _ => {
                text_only = false;
                out.push(action.clone());
            }
        }
    }
    (out, text_only.then_some(concatenated))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PARTS: DateTimeParts = DateTimeParts {
        year: 2026,
        month: 8,
        day: 17,
        hour: 16,
        minute: 5,
        second: 9,
    };

    #[test]
    fn suffix_matching_is_unicode_and_case_aware() {
        assert!(suffix_matches("hello;Mail", ";Mail", true));
        assert!(!suffix_matches("hello;mail", ";Mail", true));
        assert!(suffix_matches("hello;mail", ";Mail", false));
        assert!(suffix_matches("тест;ПРИВЕТ", ";привет", false));
    }

    #[test]
    fn exact_backspace_counts_match_blocking_semantics() {
        use crate::schemas::frontend::TextExpansionMode;
        assert_eq!(backspaces_for(TextExpansionMode::Instant, 5), 4);
        assert_eq!(backspaces_for(TextExpansionMode::Delimiter, 5), 5);
        assert_eq!(backspaces_for(TextExpansionMode::Instant, 1), 0);
    }

    #[test]
    fn delimiter_config_supports_literal_and_escaped_whitespace() {
        assert!(delimiter_contains(" \\t\\n.,", ' '));
        assert!(delimiter_contains(" \\t\\n.,", '\t'));
        assert!(delimiter_contains(" \\t\\n.,", '\n'));
        assert!(delimiter_contains(" \\t\\n.,", '.'));
        assert!(!delimiter_contains(" \\t\\n.,", 'x'));
    }

    #[test]
    fn buffer_is_bounded_by_unicode_characters() {
        let value = format!("{}END", "я".repeat(TEXT_BUFFER_MAX_CHARS));
        let trimmed = trim_to_last_chars(value, TEXT_BUFFER_MAX_CHARS);
        assert_eq!(trimmed.chars().count(), TEXT_BUFFER_MAX_CHARS);
        assert!(trimmed.ends_with("END"));
    }

    #[test]
    fn focus_timeout_backspace_and_undo_lifecycle_are_memory_only() {
        let t0 = Instant::now();
        let mut state = TextInputState::default();
        state.prepare(t0, 11);
        state.note_printable("abc".into(), t0);
        state.pop_backspace(t0 + Duration::from_millis(1));
        assert_eq!(state.buffer, "ab");
        state.note_printable("abc".into(), t0 + Duration::from_millis(2));
        state.set_undo(TextUndoRecord {
            original_input: "abc".into(),
            inserted_text: "XYZ".into(),
            chars_removed: 2,
            timestamp: t0 + Duration::from_millis(2),
            window_id: 11,
        });
        assert!(state.take_undo(t0 + Duration::from_secs(1), 11).is_some());
        state.note_printable("again".into(), t0 + Duration::from_secs(1));
        state.prepare(t0 + TEXT_BUFFER_TIMEOUT + Duration::from_secs(2), 11);
        assert!(state.buffer.is_empty());
        state.note_printable("window".into(), t0 + Duration::from_secs(20));
        state.prepare(t0 + Duration::from_secs(20), 12);
        assert!(state.buffer.is_empty());
        assert!(state.undo.is_none());
    }

    #[test]
    fn date_time_and_clipboard_tokens_are_deterministic() {
        let tpl = "{{date}} {{time}} :: {{clipboard}}";
        assert_eq!(
            render_template_with(
                tpl,
                TextDateFormat::Dmy,
                TextTimeFormat::Hm24,
                PARTS,
                Some("clip")
            ),
            "17.08.2026 16:05 :: clip"
        );
        assert_eq!(
            render_template_with(
                "{{date}}",
                TextDateFormat::Ymd,
                TextTimeFormat::Hm24,
                PARTS,
                None
            ),
            "2026-08-17"
        );
        assert_eq!(
            render_template_with(
                "{{time}}",
                TextDateFormat::Dmy,
                TextTimeFormat::Hms24,
                PARTS,
                None
            ),
            "16:05:09"
        );
        assert_eq!(
            render_template_with(
                "{{time}}",
                TextDateFormat::Dmy,
                TextTimeFormat::Hm12,
                PARTS,
                None
            ),
            "4:05 PM"
        );
    }
}

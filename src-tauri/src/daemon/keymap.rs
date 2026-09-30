//! Таблица соответствий evdev KEY-кодов и внутренних VK-кодов.
//!
//! Внутренний формат приложения исторически — Windows virtual-key коды
//! (они используются в frontend, правилах и сохранённых профилях), поэтому
//! Linux-бэкенд конвертирует evdev → VK на входе и VK → evdev на выходе.
//! Буквы/цифры/F-клавиши совпадают 1:1, OEM-клавиши и модификаторы — по таблице.
//!
//! Порядок записей важен для обратного маппинга (VK → evdev): при коллизии
//! выигрывает первая запись, поэтому основная клавиатура идёт раньше numpad.

/// Официальная таблица evdev → VK: буквы, цифры, F-клавиши, навигация, OEM, media.
pub const EVDEV_TO_VK: &[(u16, u8)] = &[
    (1, 0x1B),   // ESC
    (14, 0x08),  // BACKSPACE
    (15, 0x09),  // TAB
    (28, 0x0D),  // ENTER
    (57, 0x20),  // SPACE
    (58, 0x14),  // CAPSLOCK
    (29, 0xA2), (97, 0xA3), // LCTRL, RCTRL
    (42, 0xA0), (54, 0xA1), // LSHIFT, RSHIFT
    (56, 0xA4), (100, 0xA5), // LALT, RALT
    (125, 0x5B), (126, 0x5C), // LMETA, RMETA
    (127, 0x5D), // COMPOSE -> APPS
    (2, 0x31), (3, 0x32), (4, 0x33), (5, 0x34), (6, 0x35),
    (7, 0x36), (8, 0x37), (9, 0x38), (10, 0x39), (11, 0x30),
    (16, 0x51), (17, 0x57), (18, 0x45), (19, 0x52), (20, 0x54), // Q W E R T
    (21, 0x59), (22, 0x55), (23, 0x49), (24, 0x4F), (25, 0x50), // Y U I O P
    (30, 0x41), (31, 0x53), (32, 0x44), (33, 0x46), (34, 0x47), // A S D F G
    (35, 0x48), (36, 0x4A), (37, 0x4B), (38, 0x4C), // H J K L
    (44, 0x5A), (45, 0x58), (46, 0x43), (47, 0x56), (48, 0x42), // Z X C V B
    (49, 0x4E), (50, 0x4D), // N M
    (12, 0xBD), (13, 0xBB), // MINUS, EQUAL
    (26, 0xDB), (27, 0xDD), (43, 0xDC), // LEFTBRACE, RIGHTBRACE, BACKSLASH
    (39, 0xBA), (40, 0xDE), (41, 0xC0), // SEMICOLON, APOSTROPHE, GRAVE
    (51, 0xBC), (52, 0xBE), (53, 0xBF), // COMMA, DOT, SLASH
    (86, 0xE2), // 102ND -> VK_OEM_102
    (59, 0x70), (60, 0x71), (61, 0x72), (62, 0x73), (63, 0x74), // F1-F5
    (64, 0x75), (65, 0x76), (66, 0x77), (67, 0x78), (68, 0x79), // F6-F10
    (87, 0x7A), (88, 0x7B), // F11, F12
    (183, 0x7C), (184, 0x7D), (185, 0x7E), (186, 0x7F), // F13-F16
    (187, 0x80), (188, 0x81), (189, 0x82), (190, 0x83), // F17-F20
    (191, 0x84), (192, 0x85), (193, 0x86), (194, 0x87), // F21-F24
    (102, 0x24), (107, 0x23), // HOME, END
    (104, 0x21), (109, 0x22), // PAGEUP, PAGEDOWN
    (105, 0x25), (106, 0x27), (103, 0x26), (108, 0x28), // LEFT, RIGHT, UP, DOWN
    (110, 0x2D), (111, 0x2E), // INSERT, DELETE
    (99, 0x2C),  // SYSRQ -> VK_PRINT
    (70, 0x91),  // SCROLLLOCK
    (119, 0x13), // PAUSE
    (138, 0x2F), // HELP
    (69, 0x90),  // NUMLOCK
    (71, 0x67), (72, 0x68), (73, 0x69), // KP7-KP9
    (75, 0x64), (76, 0x65), (77, 0x66), // KP4-KP6
    (79, 0x61), (80, 0x62), (81, 0x63), // KP1-KP3
    (82, 0x60), (83, 0x6E), // KP0, KPDOT
    (96, 0x0D),  // KPENTER
    (55, 0x6A), (78, 0x6B), (74, 0x6D), (98, 0x6F), // KP*, KP+, KP-, KP/
    // Media / browser
    (113, 0xAD), (114, 0xAE), (115, 0xAF), // MUTE, VOLUMEDOWN, VOLUMEUP
    (164, 0xB3), (166, 0xB2), (171, 0xB0), (165, 0xB1), // PLAYPAUSE, STOPCD, NEXTSONG, PREVIOUSSONG
    (155, 0xB4), (226, 0xB5), // MAIL, MEDIA
    (158, 0xA6), (159, 0xA7), (173, 0xA8), (128, 0xA9), // BACK, FORWARD, REFRESH, STOP
    (217, 0xAA), (156, 0xAB), (172, 0xAC), // SEARCH, BOOKMARKS, HOMEPAGE
    (157, 0xB6), (140, 0xB7), // COMPUTER, CALC
    (121, 0x6E), // KPCOMMA -> VK_DECIMAL (best effort)
];

/// evdev код кнопки мыши → внутренний код кнопки (1=Left..5=X2)
pub const MOUSE_BUTTON_MAP: &[(u16, u8)] = &[
    (0x110, 1), // BTN_LEFT
    (0x111, 2), // BTN_RIGHT
    (0x112, 3), // BTN_MIDDLE
    (0x113, 4), // BTN_SIDE
    (0x114, 5), // BTN_EXTRA
    (0x116, 4), // BTN_BACK (часто=X1)
    (0x115, 5), // BTN_FORWARD (часто=X2)
];

/// Коды кнопок виртуальной мыши, которые умеет воспроизводить uinput.
pub const VIRTUAL_MOUSE_BUTTONS: &[(u8, u16)] = &[
    (1, 0x110),
    (2, 0x111),
    (3, 0x112),
    (4, 0x113),
    (5, 0x114),
];

pub fn evdev_to_vk(code: u16) -> Option<u8> {
    EVDEV_TO_VK
        .iter()
        .find(|(evdev, _)| *evdev == code)
        .map(|(_, vk)| *vk)
}

pub fn vk_to_evdev(vk: u8) -> Option<u16> {
    EVDEV_TO_VK
        .iter()
        .find(|(_, key)| *key == vk)
        .map(|(evdev, _)| *evdev)
}

pub fn evdev_button_to_button(code: u16) -> Option<u8> {
    MOUSE_BUTTON_MAP
        .iter()
        .find(|(evdev, _)| *evdev == code)
        .map(|(_, button)| *button)
}

pub fn button_to_evdev(button: u8) -> Option<u16> {
    VIRTUAL_MOUSE_BUTTONS
        .iter()
        .find(|(code, _)| *code == button)
        .map(|(_, evdev)| *evdev)
}

/// Все evdev-коды клавиатуры, которые умеет виртуальная клавиатура.
pub fn all_evdev_keyboard_keys() -> Vec<u16> {
    let mut keys: Vec<u16> = EVDEV_TO_VK.iter().map(|(code, _)| *code).collect();
    keys.dedup();
    keys
}

/// VK → символ US-раскладки (без учёта активной раскладки системы).
/// Используется для text expansion и набора текста.
pub fn vk_to_char_us(vk: u8, shift: bool) -> Option<char> {
    let mapped = match vk {
        // Буквы
        0x41..=0x5A => {
            let base = b'a' + (vk - 0x41);
            if shift {
                (base - 32) as char
            } else {
                base as char
            }
        }
        // Цифры
        0x30 => {
            if shift {
                ')'
            } else {
                '0'
            }
        }
        0x31 => {
            if shift {
                '!'
            } else {
                '1'
            }
        }
        0x32 => {
            if shift {
                '@'
            } else {
                '2'
            }
        }
        0x33 => {
            if shift {
                '#'
            } else {
                '3'
            }
        }
        0x34 => {
            if shift {
                '$'
            } else {
                '4'
            }
        }
        0x35 => {
            if shift {
                '%'
            } else {
                '5'
            }
        }
        0x36 => {
            if shift {
                '^'
            } else {
                '6'
            }
        }
        0x37 => {
            if shift {
                '&'
            } else {
                '7'
            }
        }
        0x38 => {
            if shift {
                '*'
            } else {
                '8'
            }
        }
        0x39 => {
            if shift {
                '('
            } else {
                '9'
            }
        }
        0x20 => ' ',
        0x09 => '\t',
        0x0D => '\n',
        // Numpad
        0x60..=0x69 => (b'0' + (vk - 0x60)) as char,
        0x6A => '*',
        0x6B => '+',
        0x6D => '-',
        0x6E => '.',
        0x6F => '/',
        // OEM (US layout)
        0xBA => {
            if shift {
                ':'
            } else {
                ';'
            }
        }
        0xBB => {
            if shift {
                '+'
            } else {
                '='
            }
        }
        0xBC => {
            if shift {
                '<'
            } else {
                ','
            }
        }
        0xBE => {
            if shift {
                '>'
            } else {
                '.'
            }
        }
        0xBF => {
            if shift {
                '?'
            } else {
                '/'
            }
        }
        0xC0 => {
            if shift {
                '~'
            } else {
                '`'
            }
        }
        0xDB => {
            if shift {
                '{'
            } else {
                '['
            }
        }
        0xDC => {
            if shift {
                '|'
            } else {
                '\\'
            }
        }
        0xDD => {
            if shift {
                '}'
            } else {
                ']'
            }
        }
        0xDE => {
            if shift {
                '"'
            } else {
                '\''
            }
        }
        0xBD => {
            if shift {
                '_'
            } else {
                '-'
            }
        }
        0xE2 => {
            if shift {
                '|'
            } else {
                '\\'
            }
        }
        _ => return None,
    };
    Some(mapped)
}

/// Символ → (VK, shift) по US-раскладке. Используется при наборе текста
/// через uinput (TypeString / text expansion).
pub fn char_to_vk_us(c: char) -> Option<(u8, bool)> {
    if c.is_ascii_uppercase() {
        return Some((0x41 + (c as u8 - b'A'), true));
    }
    if c.is_ascii_lowercase() {
        return Some((0x41 + (c as u8 - b'a'), false));
    }
    let (vk, shift) = match c {
        ' ' => (0x20, false),
        '\t' => (0x09, false),
        '\n' | '\r' => (0x0D, false),
        '0' => (0x30, false),
        '1' => (0x31, false),
        '2' => (0x32, false),
        '3' => (0x33, false),
        '4' => (0x34, false),
        '5' => (0x35, false),
        '6' => (0x36, false),
        '7' => (0x37, false),
        '8' => (0x38, false),
        '9' => (0x39, false),
        ')' => (0x30, true),
        '!' => (0x31, true),
        '@' => (0x32, true),
        '#' => (0x33, true),
        '$' => (0x34, true),
        '%' => (0x35, true),
        '^' => (0x36, true),
        '&' => (0x37, true),
        '*' => (0x38, true),
        '(' => (0x39, true),
        ';' => (0xBA, false),
        ':' => (0xBA, true),
        '=' => (0xBB, false),
        '+' => (0xBB, true),
        ',' => (0xBC, false),
        '<' => (0xBC, true),
        '-' => (0xBD, false),
        '_' => (0xBD, true),
        '.' => (0xBE, false),
        '>' => (0xBE, true),
        '/' => (0xBF, false),
        '?' => (0xBF, true),
        '`' => (0xC0, false),
        '~' => (0xC0, true),
        '[' => (0xDB, false),
        '{' => (0xDB, true),
        '\\' => (0xDC, false),
        '|' => (0xDC, true),
        ']' => (0xDD, false),
        '}' => (0xDD, true),
        '\'' => (0xDE, false),
        '"' => (0xDE, true),
        _ => return None,
    };
    Some((vk, shift))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_letters_and_digits() {
        for (evdev, vk) in [(30u16, 0x41u8), (16, 0x51), (11, 0x30), (2, 0x31), (88, 0x7B)] {
            assert_eq!(evdev_to_vk(evdev), Some(vk), "evdev {}", evdev);
            assert_eq!(vk_to_evdev(vk), Some(evdev), "vk {:#x}", vk);
        }
    }

    #[test]
    fn enter_prefers_main_keyboard_over_numpad() {
        assert_eq!(vk_to_evdev(0x0D), Some(28));
    }

    #[test]
    fn mouse_buttons_roundtrip() {
        assert_eq!(evdev_button_to_button(0x110), Some(1));
        assert_eq!(evdev_button_to_button(0x114), Some(5));
        assert_eq!(button_to_evdev(3), Some(0x112));
    }

    #[test]
    fn vk_to_char_shifted_oem() {
        assert_eq!(vk_to_char_us(0x41, false), Some('a'));
        assert_eq!(vk_to_char_us(0x41, true), Some('A'));
        assert_eq!(vk_to_char_us(0xBA, true), Some(':'));
        assert_eq!(vk_to_char_us(0x31, true), Some('!'));
    }
}

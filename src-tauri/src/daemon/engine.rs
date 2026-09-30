use std::collections::HashMap;
use std::sync::{
    LazyLock, Mutex, RwLockReadGuard,
    atomic::{AtomicU16, Ordering},
};
use std::time::{Duration, Instant};

use tracing::error;

use crate::context::AppContext;
use crate::daemon::chord_output::{
    build_atomic_chord_commands, isolate_macro_commands, modifier_vks, release_modifier_commands,
    shell_mask_commands,
};
use crate::daemon::input_state::GestureSpec;
use crate::daemon::mouse_triggers::{
    DoubleClickDetector, MoveGate, system_double_click_limits, wheel_key,
};
use crate::daemon::state::DaemonStateRef;
use crate::daemon::text_expansion::{
    TextUndoRecord, backspaces_for, delimiter_contains, materialize_text_actions, suffix_chars,
    suffix_matches,
};
use crate::schemas::engine::{EngineAction, EngineCondition, SimulatorCommand};
use crate::schemas::frontend::{TextExpansionMode, key_modifiers};
use crate::shared::calculate_hash;

/// Тип результата обработки события
#[derive(Debug)]
pub enum EventAction {
    PassThrough,
    Block,
}

/// Безопасно читает контекст приложения.
///
/// Возвращает `None`, если RwLock отравлен (poisoned) из-за паники в другом потоке,
/// либо если контекст ещё не инициализирован. В callback'ах LL-хуков паника = abort
/// процесса, поэтому НИКОГДА не используем `.unwrap()` на `ctx_arc.read()` —
/// только через эту функцию.
fn try_read_ctx(
    ctx_arc: &crate::context::AppContextState,
) -> Option<RwLockReadGuard<'_, AppContext>> {
    match ctx_arc.read() {
        Ok(guard) => Some(guard),
        Err(poisoned) => {
            error!("AppContext RwLock отравлен, пропускаем обработку правила");
            Some(poisoned.into_inner())
        }
    }
}

pub struct PendingTapHold {
    pub vk_code: u8,
    pub tap_actions: Vec<EngineAction>,
    pub hold_actions: Vec<EngineAction>,
    pub down_time: Instant,
    pub timeout_ms: u32,
    pub is_held: bool,
}

pub static PENDING_TAP_HOLDS: LazyLock<Mutex<HashMap<u8, PendingTapHold>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

static PHYSICAL_MODIFIERS: AtomicU16 = AtomicU16::new(0);

#[derive(Clone)]
struct InputLifecycle {
    actions: Vec<EngineAction>,
    trigger_modifiers: u16,
}

/// Rule selection is bound to the physical press lifecycle. We must not
/// re-evaluate modifiers/window/layer on release, otherwise a remapped output
/// can stay stuck when context changes while the key is held.
static ACTIVE_KEY_DOWN_ACTIONS: LazyLock<Mutex<HashMap<u8, InputLifecycle>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static PENDING_KEY_UP_ACTIONS: LazyLock<Mutex<HashMap<u8, InputLifecycle>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static ACTIVE_MOUSE_DOWN_ACTIONS: LazyLock<Mutex<HashMap<u8, Vec<EngineAction>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static PENDING_MOUSE_UP_ACTIONS: LazyLock<Mutex<HashMap<u8, Vec<EngineAction>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static DOUBLE_CLICK_DETECTOR: LazyLock<Mutex<DoubleClickDetector>> =
    LazyLock::new(|| Mutex::new(DoubleClickDetector::default()));
static MOUSE_MOVE_GATES: LazyLock<Mutex<HashMap<u64, MoveGate>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn modifier_bit_for_vk(vk: u8) -> u16 {
    match vk {
        0xA2 => key_modifiers::LCTRL,
        0xA3 => key_modifiers::RCTRL,
        0xA4 => key_modifiers::LALT,
        0xA5 => key_modifiers::RALT,
        0xA0 => key_modifiers::LSHIFT,
        0xA1 => key_modifiers::RSHIFT,
        0x5B => key_modifiers::LWIN,
        0x5C => key_modifiers::RWIN,
        0x11 => key_modifiers::CTRL,
        0x12 => key_modifiers::ALT,
        0x10 => key_modifiers::SHIFT,
        _ => 0,
    }
}

pub fn is_modifier_vk(vk: u8) -> bool {
    modifier_bit_for_vk(vk) != 0
}

/// Update physical modifier state and return the modifier snapshot that belongs
/// to this event. For a modifier key itself, its own bit is excluded so legacy
/// single-modifier rules still behave like an ordinary key trigger.
pub fn update_modifier_state(vk: u8, is_down: bool) -> u16 {
    let bit = modifier_bit_for_vk(vk);
    if bit == 0 {
        return PHYSICAL_MODIFIERS.load(Ordering::Relaxed) & key_modifiers::ALL;
    }

    if is_down {
        let before = PHYSICAL_MODIFIERS.fetch_or(bit, Ordering::Relaxed);
        before & key_modifiers::ALL
    } else {
        let before = PHYSICAL_MODIFIERS.fetch_and(!bit, Ordering::Relaxed);
        (before & !bit) & key_modifiers::ALL
    }
}

pub fn reset_modifier_state() {
    PHYSICAL_MODIFIERS.store(0, Ordering::Relaxed);
    if let Ok(mut active) = ACTIVE_KEY_DOWN_ACTIONS.lock() {
        active.clear();
    }
    if let Ok(mut pending) = PENDING_KEY_UP_ACTIONS.lock() {
        pending.clear();
    }
    if let Ok(mut active) = ACTIVE_MOUSE_DOWN_ACTIONS.lock() {
        active.clear();
    }
    if let Ok(mut pending) = PENDING_MOUSE_UP_ACTIONS.lock() {
        pending.clear();
    }
    if let Ok(mut detector) = DOUBLE_CLICK_DETECTOR.lock() {
        detector.clear();
    }
    if let Ok(mut gates) = MOUSE_MOVE_GATES.lock() {
        gates.clear();
    }
}

fn family_matches(required: u16, actual: u16, generic: u16, left: u16, right: u16) -> bool {
    let req_generic = required & generic != 0;
    let req_sides = required & (left | right);
    let actual_family = actual & (generic | left | right);

    if req_generic {
        return actual_family != 0;
    }
    if req_sides != 0 {
        return actual_family & req_sides == req_sides
            && actual_family & (left | right) & !req_sides == 0
            && actual_family & generic == 0;
    }
    actual_family == 0
}

fn modifiers_match(required: u16, actual: u16) -> bool {
    let required = required & key_modifiers::ALL;
    let actual = actual & key_modifiers::ALL;
    family_matches(
        required,
        actual,
        key_modifiers::CTRL,
        key_modifiers::LCTRL,
        key_modifiers::RCTRL,
    ) && family_matches(
        required,
        actual,
        key_modifiers::ALT,
        key_modifiers::LALT,
        key_modifiers::RALT,
    ) && family_matches(
        required,
        actual,
        key_modifiers::SHIFT,
        key_modifiers::LSHIFT,
        key_modifiers::RSHIFT,
    ) && family_matches(
        required,
        actual,
        key_modifiers::WIN,
        key_modifiers::LWIN,
        key_modifiers::RWIN,
    )
}

pub(crate) fn currently_held_modifier_vks(mask: u16) -> Vec<u8> {
    let current = PHYSICAL_MODIFIERS.load(Ordering::Relaxed) & mask & key_modifiers::ALL;
    modifier_vks(current)
}

fn current_physical_modifiers() -> u16 {
    PHYSICAL_MODIFIERS.load(Ordering::Relaxed) & key_modifiers::ALL
}

fn send_commands(
    simulator: &crate::simulator::SimulatorSender,
    commands: impl IntoIterator<Item = SimulatorCommand>,
) {
    for command in commands {
        let _ = simulator.send(command);
    }
}

fn send_isolated_immediate(
    simulator: &crate::simulator::SimulatorSender,
    commands: impl IntoIterator<Item = SimulatorCommand>,
) {
    let physical = current_physical_modifiers();
    send_commands(simulator, shell_mask_commands(physical));
    send_commands(simulator, release_modifier_commands(physical));
    send_commands(simulator, commands);
    if physical != 0 {
        let _ = simulator.send(SimulatorCommand::RestorePhysicalModifiers { mask: physical });
    }
}

fn send_atomic_chord(simulator: &crate::simulator::SimulatorSender, code: u8, modifiers: u16) {
    let physical = current_physical_modifiers();
    send_commands(
        simulator,
        build_atomic_chord_commands(code, modifiers, physical),
    );
}

fn check_conditions(conditions: &[EngineCondition], ctx: &crate::context::AppContext) -> bool {
    for cond in conditions {
        match cond {
            EngineCondition::LayerActive { layer_id_hash } => {
                if !ctx.active_layers.contains(layer_id_hash) {
                    return false;
                }
            }
            EngineCondition::VirtualDesktop { .. } => {
                // Defensive fail-closed. The compiler currently converts legacy
                // VirtualDesktop conditions to an impossible WindowMatch too.
                return false;
            }
            EngineCondition::ContextMatch {
                process,
                path,
                title,
                class_name,
                virtual_desktop_id,
                monitor_id,
                min_width,
                max_width,
                min_height,
                max_height,
                fullscreen,
                mode,
            } => {
                let mut checks = Vec::new();
                if let Some(v) = process {
                    checks.push(ctx.active_process.eq_ignore_ascii_case(v));
                }
                if let Some(v) = path {
                    checks.push(ctx.active_process_path.to_lowercase().contains(v));
                }
                if let Some(v) = title {
                    checks.push(ctx.active_window_title.to_lowercase().contains(v));
                }
                if let Some(v) = class_name {
                    checks.push(ctx.active_window_class.eq_ignore_ascii_case(v));
                }
                if let Some(v) = virtual_desktop_id {
                    checks.push(ctx.virtual_desktop_id.eq_ignore_ascii_case(v));
                }
                if let Some(v) = monitor_id {
                    checks.push(ctx.monitor_id == *v);
                }
                if let Some(v) = min_width {
                    checks.push(ctx.window_width >= *v);
                }
                if let Some(v) = max_width {
                    checks.push(ctx.window_width <= *v);
                }
                if let Some(v) = min_height {
                    checks.push(ctx.window_height >= *v);
                }
                if let Some(v) = max_height {
                    checks.push(ctx.window_height <= *v);
                }
                if let Some(v) = fullscreen {
                    checks.push(ctx.fullscreen == *v);
                }
                if checks.is_empty()
                    || match mode {
                        crate::shared::types::MatchMode::Any => !checks.iter().any(|v| *v),
                        crate::shared::types::MatchMode::All => !checks.iter().all(|v| *v),
                    }
                {
                    return false;
                }
            }
            EngineCondition::WindowMatch {
                process_hash,
                title_contains,
            } => {
                let process_ok = match process_hash {
                    Some(h) => calculate_hash(&ctx.active_process) == *h,
                    None => false,
                };
                let title_ok = match title_contains {
                    Some(t) => ctx.active_window_title.to_lowercase().contains(t),
                    None => false,
                };
                if !process_ok && !title_ok {
                    return false;
                }
            }
        }
    }
    true
}

fn notify_layer_change(state: Option<&DaemonStateRef>, layer_id_hash: u64, active: bool) {
    if let Some(state_ref) = state {
        if let Ok(s) = state_ref.read() {
            if let Some(ref prof) = s.active_profile {
                for layer in &prof.layers {
                    if crate::shared::calculate_hash(&layer.id) == layer_id_hash {
                        let event = if active {
                            crate::gui::events::DaemonEvent::LayerActivated {
                                layer_id: layer.id.clone(),
                                layer_name: layer.name.clone(),
                            }
                        } else {
                            crate::gui::events::DaemonEvent::LayerDeactivated {
                                layer_id: layer.id.clone(),
                            }
                        };
                        crate::gui::events::broadcast_event(event);
                        break;
                    }
                }
            }
        }
    }
}

fn execute_actions(
    actions: &[EngineAction],
    simulator: &crate::simulator::SimulatorSender,
    ctx_arc: &std::sync::Arc<std::sync::RwLock<crate::context::AppContext>>,
    is_down: bool,
    state: Option<&DaemonStateRef>,
    trigger_modifiers: u16,
) -> EventAction {
    // Even actions that do not synthesize keyboard input must mark an Alt/Win
    // combination as consumed. Otherwise Windows may treat the eventual
    // modifier release as an isolated Alt/Win press (menu/Start activation).
    if is_down && trigger_modifiers != 0 {
        send_commands(simulator, shell_mask_commands(current_physical_modifiers()));
    }

    for action in actions {
        match action {
            EngineAction::RemapKey { code, modifiers } => {
                // Chord remaps are emitted atomically. Legacy single-key -> single-key
                // remaps keep their down/up lifecycle and therefore preserve hold/repeat.
                if trigger_modifiers != 0 || *modifiers != 0 {
                    if is_down {
                        send_atomic_chord(simulator, *code, *modifiers);
                    }
                } else if is_down {
                    let _ = simulator.send(SimulatorCommand::PressKey(*code));
                } else {
                    let _ = simulator.send(SimulatorCommand::ReleaseKey(*code));
                }
            }
            EngineAction::RemapMouse { code } => {
                if trigger_modifiers != 0 {
                    if is_down {
                        send_isolated_immediate(
                            simulator,
                            [
                                SimulatorCommand::MousePress(*code),
                                SimulatorCommand::MouseRelease(*code),
                            ],
                        );
                    }
                } else if is_down {
                    let _ = simulator.send(SimulatorCommand::MousePress(*code));
                } else {
                    let _ = simulator.send(SimulatorCommand::MouseRelease(*code));
                }
            }
            EngineAction::TypeText {
                text,
                date_format,
                time_format,
            } => {
                if is_down {
                    let rendered = crate::daemon::text_expansion::render_template(
                        text,
                        *date_format,
                        *time_format,
                    );
                    if trigger_modifiers != 0 {
                        send_isolated_immediate(
                            simulator,
                            [SimulatorCommand::TypeString(rendered)],
                        );
                    } else {
                        let _ = simulator.send(SimulatorCommand::TypeString(rendered));
                    }
                }
            }
            EngineAction::TypeTextLiteral { text } => {
                if is_down {
                    if trigger_modifiers != 0 {
                        send_isolated_immediate(
                            simulator,
                            [SimulatorCommand::TypeString(text.clone())],
                        );
                    } else {
                        let _ = simulator.send(SimulatorCommand::TypeString(text.clone()));
                    }
                }
            }
            EngineAction::MacroCommands {
                commands,
                playback,
                macro_key,
            } => {
                if is_down {
                    let mut macro_commands = commands.clone();

                    // Позицию курсора фиксируем в момент запуска макроса, но команду
                    // возврата добавляем в КОНЕЦ macro-job. Раньше все команды, включая
                    // Delay, шли в общую очередь и могли задерживать обычный remap.
                    if let Some(state_ref) = state {
                        if let Ok(s) = state_ref.read() {
                            if s.restore_mouse_after_macro {
                                if let Some((x, y)) = crate::platform::x11::cursor_position() {
                                    macro_commands.push(SimulatorCommand::MouseAbsolute { x, y });
                                }
                            }
                        }
                    }

                    if trigger_modifiers != 0 {
                        macro_commands =
                            isolate_macro_commands(macro_commands, current_physical_modifiers());
                    }
                    let _ = simulator.send_macro(macro_commands, *playback, *macro_key);
                } else if playback.repeat_while_held {
                    simulator.cancel_macro_key(*macro_key);
                }
            }
            EngineAction::ToggleLayer { layer_id_hash } => {
                if is_down {
                    let mut became_active = false;
                    let mut state_changed = false;
                    if let Ok(mut wctx) = ctx_arc.write() {
                        state_changed = true;
                        if wctx.active_layers.contains(layer_id_hash) {
                            wctx.active_layers.remove(layer_id_hash);
                        } else {
                            wctx.active_layers.insert(*layer_id_hash);
                            became_active = true;
                        }
                    }
                    if state_changed {
                        notify_layer_change(state, *layer_id_hash, became_active);
                    }
                }
            }
            EngineAction::HoldLayerPush { layer_id_hash } => {
                let mut state_changed = false;
                if is_down {
                    if let Ok(mut wctx) = ctx_arc.write() {
                        if !wctx.active_layers.contains(layer_id_hash) {
                            wctx.active_layers.insert(*layer_id_hash);
                            state_changed = true;
                        }
                    }
                    if state_changed {
                        notify_layer_change(state, *layer_id_hash, true);
                    }
                } else {
                    if let Ok(mut wctx) = ctx_arc.write() {
                        if wctx.active_layers.contains(layer_id_hash) {
                            wctx.active_layers.remove(layer_id_hash);
                            state_changed = true;
                        }
                    }
                    if state_changed {
                        notify_layer_change(state, *layer_id_hash, false);
                    }
                }
            }
            EngineAction::HoldLayerPop { layer_id_hash } => {
                if !is_down {
                    let mut state_changed = false;
                    if let Ok(mut wctx) = ctx_arc.write() {
                        if wctx.active_layers.contains(layer_id_hash) {
                            wctx.active_layers.remove(layer_id_hash);
                            state_changed = true;
                        }
                    }
                    if state_changed {
                        notify_layer_change(state, *layer_id_hash, false);
                    }
                }
            }
            EngineAction::SystemVolume { action } => {
                if is_down {
                    let vk = match action.as_str() {
                        "mute" => 0xAD,
                        "down" => 0xAE,
                        "up" => 0xAF,
                        _ => 0,
                    };
                    if vk != 0 {
                        if trigger_modifiers != 0 {
                            send_atomic_chord(simulator, vk, 0);
                        } else {
                            let _ = simulator.send(SimulatorCommand::PressKey(vk));
                            let _ = simulator.send(SimulatorCommand::ReleaseKey(vk));
                        }
                    }
                }
            }
            EngineAction::MediaKey { key } => {
                if is_down {
                    let vk = match key.as_str() {
                        "play_pause" => 0xB3,
                        "next" => 0xB0,
                        "prev" => 0xB1,
                        "stop" => 0xB2,
                        _ => 0,
                    };
                    if vk != 0 {
                        if trigger_modifiers != 0 {
                            send_atomic_chord(simulator, vk, 0);
                        } else {
                            let _ = simulator.send(SimulatorCommand::PressKey(vk));
                            let _ = simulator.send(SimulatorCommand::ReleaseKey(vk));
                        }
                    }
                }
            }
            EngineAction::WindowAction { action } => {
                if is_down {
                    crate::simulator::system::execute_window_action(action);
                }
            }
            EngineAction::LaunchApp { path } => {
                if is_down {
                    crate::simulator::system::launch_app(path);
                }
            }
            EngineAction::FocusProcess { process, title } => {
                if is_down {
                    crate::simulator::system::focus_process(process.as_deref(), title.as_deref());
                }
            }
            EngineAction::Sleep => {
                if is_down {
                    crate::simulator::system::sleep_pc();
                }
            }
            EngineAction::MonitorOff => {
                if is_down {
                    crate::simulator::system::monitor_off();
                }
            }
        }
    }
    EventAction::Block
}

pub fn tick_tap_holds(state: Option<&DaemonStateRef>) {
    let now = Instant::now();
    let mut to_trigger_hold = Vec::new();

    if let Ok(mut pending) = PENDING_TAP_HOLDS.lock() {
        for info in pending.values_mut() {
            if !info.is_held
                && now.duration_since(info.down_time).as_millis() as u32 >= info.timeout_ms
            {
                info.is_held = true;
                to_trigger_hold.push(info.hold_actions.clone());
            }
        }
    }

    if let Some(state_ref) = state {
        if let Ok(s) = state_ref.read() {
            if let Some(simulator) = &s.simulator {
                if let Some(ctx_state) = crate::trackers::context_tracker::get_context() {
                    for actions in to_trigger_hold {
                        execute_actions(&actions, simulator, &ctx_state, true, Some(state_ref), 0);
                    }
                }
            }
        }
    }
}

pub fn process_keyboard_event(
    vk_code: u8,
    _scan_code: u16,
    is_key_down: bool,
    _flags: u32,
    event_modifiers: u16,
    state: Option<&DaemonStateRef>,
) -> EventAction {
    let state_ref = match state {
        Some(s) => s,
        None => return EventAction::PassThrough,
    };

    let s = match state_ref.read() {
        Ok(s) => s,
        Err(_) => return EventAction::PassThrough,
    };

    if !s.kb_hook_enabled {
        return EventAction::PassThrough;
    }

    let simulator = match &s.simulator {
        Some(sim) => sim,
        None => return EventAction::PassThrough,
    };

    let engine_schema = &s.engine_schema;

    let ctx_arc = match crate::trackers::context_tracker::get_context() {
        Some(c) => c,
        None => return EventAction::PassThrough,
    };

    // v0.4.0 advanced triggers use one bounded state machine. Ordinary
    // sequences/chords observe input; leader mode intentionally captures it.
    {
        let now = Instant::now();
        let window_id = try_read_ctx(&ctx_arc)
            .map(|ctx| ctx.active_window_id)
            .unwrap_or(0);
        let max_sequence_timeout = engine_schema
            .key_sequence_rules
            .iter()
            .map(|rule| rule.timeout_ms)
            .max()
            .unwrap_or(0);
        let max_leader_timeout = engine_schema
            .leader_sequence_rules
            .iter()
            .map(|rule| rule.timeout_ms)
            .max()
            .unwrap_or(0);
        let mut additive_actions: Option<Vec<EngineAction>> = None;
        let mut capture = false;

        if let Ok(mut input) = s.advanced_input.lock() {
            input.prepare_window(window_id);
            input.expire(now, max_sequence_timeout, max_leader_timeout);

            if !is_key_down {
                if input.key_up(vk_code) {
                    return EventAction::Block;
                }
            } else {
                let fresh_press = input.key_down(vk_code, now);

                if input.leader_active() {
                    capture = true;
                    if fresh_press {
                        input.suppress_keyup(vk_code);
                        if vk_code == 0x1B {
                            input.finish_leader();
                        } else if !is_modifier_vk(vk_code) {
                            input.push_leader_key(vk_code);
                            if let Some((source, started_at, keys)) = input.leader_snapshot() {
                                let mut has_prefix = false;
                                if let Some(ctx) = try_read_ctx(&ctx_arc) {
                                    for candidate in &engine_schema.leader_sequence_rules {
                                        if candidate.leader.code != source.code
                                            || !modifiers_match(
                                                candidate.leader.modifiers,
                                                source.modifiers,
                                            )
                                            || now.duration_since(started_at)
                                                > Duration::from_millis(u64::from(
                                                    candidate.timeout_ms,
                                                ))
                                        {
                                            continue;
                                        }
                                        if candidate.sequence.starts_with(&keys) {
                                            has_prefix = true;
                                        }
                                        if candidate.sequence == keys
                                            && check_conditions(&candidate.rule.conditions, &ctx)
                                        {
                                            additive_actions = Some(candidate.rule.actions.clone());
                                            break;
                                        }
                                    }
                                }
                                if additive_actions.is_some() || !has_prefix {
                                    input.finish_leader();
                                }
                            }
                        }
                    }
                } else if fresh_press && !is_modifier_vk(vk_code) {
                    // Enter leader mode only if at least one rule is eligible in the
                    // current context, so an inactive leader never swallows a key.
                    if let Some(ctx) = try_read_ctx(&ctx_arc) {
                        if engine_schema.leader_sequence_rules.iter().any(|candidate| {
                            candidate.leader.code == vk_code
                                && modifiers_match(candidate.leader.modifiers, event_modifiers)
                                && check_conditions(&candidate.rule.conditions, &ctx)
                        }) {
                            input.start_leader(
                                crate::schemas::frontend::KeyChord {
                                    code: vk_code,
                                    modifiers: event_modifiers,
                                },
                                now,
                            );
                            capture = true;
                        }
                    }

                    if !capture {
                        input.push_sequence(vk_code, now);
                        if let Some(ctx) = try_read_ctx(&ctx_arc) {
                            for candidate in &engine_schema.key_sequence_rules {
                                if input.sequence_matches(
                                    &candidate.sequence,
                                    now,
                                    candidate.timeout_ms,
                                ) && check_conditions(&candidate.rule.conditions, &ctx)
                                {
                                    additive_actions = Some(candidate.rule.actions.clone());
                                    break;
                                }
                            }

                            if additive_actions.is_none() {
                                for candidate in &engine_schema.key_chord_set_rules {
                                    if input.chord_should_fire(
                                        candidate.rule_id_hash,
                                        &candidate.codes,
                                        candidate.max_skew_ms,
                                    ) && check_conditions(&candidate.rule.conditions, &ctx)
                                    {
                                        additive_actions = Some(candidate.rule.actions.clone());
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        if let Some(actions) = additive_actions {
            execute_actions(&actions, simulator, &ctx_arc, true, state, 0);
            execute_actions(&actions, simulator, &ctx_arc, false, state, 0);
        }
        if capture {
            return EventAction::Block;
        }
    }

    // Text expansion matching. State is bounded and in-memory only.
    if is_key_down {
        let now = Instant::now();
        let window_id = try_read_ctx(&ctx_arc)
            .map(|ctx| ctx.active_window_id)
            .unwrap_or(0);
        let ctrl_mask = key_modifiers::CTRL | key_modifiers::LCTRL | key_modifiers::RCTRL;
        let alt_win_mask = key_modifiers::ALT
            | key_modifiers::LALT
            | key_modifiers::RALT
            | key_modifiers::WIN
            | key_modifiers::LWIN
            | key_modifiers::RWIN;
        let ctrl_active = event_modifiers & ctrl_mask != 0;

        // Ctrl+Z consumes only our immediately preceding text-only expansion.
        if vk_code == 0x5A && ctrl_active {
            let undo = s
                .text_input
                .lock()
                .ok()
                .and_then(|mut input| input.take_undo(now, window_id));
            if let Some(undo) = undo {
                // Ctrl is physically held while this branch handles Ctrl+Z. Ordinary
                // synthetic Backspace would therefore reach the target as Ctrl+Backspace.
                // Use the existing modifier-isolation path: release physical modifiers,
                // emit the undo atomically through the immediate queue, then restore them.
                let mut commands = Vec::with_capacity(undo.inserted_text.chars().count() * 2 + 1);
                for _ in 0..undo.inserted_text.chars().count() {
                    commands.push(SimulatorCommand::PressKey(0x08));
                    commands.push(SimulatorCommand::ReleaseKey(0x08));
                }
                commands.push(SimulatorCommand::TypeString(undo.original_input));
                send_isolated_immediate(simulator, commands);
                if let Ok(mut input) = s.text_input.lock() {
                    input.clear_buffer();
                }
                return EventAction::Block;
            }
        }

        let hard_modifier_vk = matches!(
            vk_code,
            0x11 | 0x12 | 0x5B | 0x5C | 0xA2 | 0xA3 | 0xA4 | 0xA5
        );
        let modified_non_modifier =
            !is_modifier_vk(vk_code) && (event_modifiers & (ctrl_mask | alt_win_mask) != 0);
        let navigation = matches!(vk_code, 0x21..=0x28 | 0x2D);

        if hard_modifier_vk {
            // Preserve undo while Ctrl is being pressed so the following Z can consume it.
            if let Ok(mut input) = s.text_input.lock() {
                input.prepare(now, window_id);
                input.clear_buffer();
            }
        } else if modified_non_modifier {
            if let Ok(mut input) = s.text_input.lock() {
                input.prepare(now, window_id);
                input.clear_all();
            }
        } else if vk_code == 0x08 {
            if let Ok(mut input) = s.text_input.lock() {
                input.prepare(now, window_id);
                input.pop_backspace(now);
            }
        } else if vk_code == 0x2E || navigation {
            if let Ok(mut input) = s.text_input.lock() {
                input.prepare(now, window_id);
                input.clear_all();
            }
        } else if is_modifier_vk(vk_code) {
            // Shift is allowed to participate in mixed-case abbreviations.
            if let Ok(mut input) = s.text_input.lock() {
                input.prepare(now, window_id);
            }
        } else if let Some(c) = vk_to_char(vk_code, event_modifiers) {
            let (before, prospective) = match s.text_input.lock() {
                Ok(mut input) => {
                    input.prepare(now, window_id);
                    // Any ordinary edit invalidates the previous undo. A newly fired
                    // expansion below will install its own record.
                    input.undo = None;
                    let before = input.buffer.clone();
                    let mut prospective = before.clone();
                    prospective.push(c);
                    (before, prospective)
                }
                Err(_) => (String::new(), c.to_string()),
            };

            let mut matched = None;
            if let Some(ctx) = try_read_ctx(&ctx_arc) {
                for candidate in &engine_schema.text_expansion_rules {
                    let source = match candidate.mode {
                        TextExpansionMode::Instant => prospective.as_str(),
                        TextExpansionMode::Delimiter
                            if delimiter_contains(&candidate.delimiters, c) =>
                        {
                            before.as_str()
                        }
                        TextExpansionMode::Delimiter => continue,
                    };
                    if suffix_matches(source, &candidate.sequence, candidate.case_sensitive)
                        && check_conditions(&candidate.rule.conditions, &ctx)
                    {
                        matched = Some(candidate.clone());
                        break;
                    }
                }
            }

            if let Some(candidate) = matched {
                let seq_chars = candidate.sequence.chars().count();
                let (source, delimiter) = match candidate.mode {
                    TextExpansionMode::Instant => (&prospective, None),
                    TextExpansionMode::Delimiter => (&before, Some(c)),
                };
                let backspaces = backspaces_for(candidate.mode, seq_chars);
                let actual_sequence = suffix_chars(source, seq_chars);
                let mut original_input = actual_sequence;
                if let Some(delimiter) = delimiter {
                    original_input.push(delimiter);
                }

                if let Ok(mut input) = s.text_input.lock() {
                    input.clear_buffer();
                }
                for _ in 0..backspaces {
                    let _ = simulator.send(SimulatorCommand::PressKey(0x08));
                    let _ = simulator.send(SimulatorCommand::ReleaseKey(0x08));
                }

                let (actions, rendered_text) = materialize_text_actions(&candidate.rule.actions);
                execute_actions(&actions, simulator, &ctx_arc, true, state, 0);
                execute_actions(&actions, simulator, &ctx_arc, false, state, 0);

                if let Some(delimiter) = delimiter {
                    let _ = simulator.send(SimulatorCommand::TypeString(delimiter.to_string()));
                }

                if let Some(mut inserted_text) = rendered_text {
                    if let Some(delimiter) = delimiter {
                        inserted_text.push(delimiter);
                    }
                    if let Ok(mut input) = s.text_input.lock() {
                        input.set_undo(TextUndoRecord {
                            original_input,
                            inserted_text,
                            chars_removed: backspaces,
                            timestamp: now,
                            window_id,
                        });
                    }
                }
                return EventAction::Block;
            }

            if let Ok(mut input) = s.text_input.lock() {
                input.note_printable(prospective, now);
            }
        } else if let Ok(mut input) = s.text_input.lock() {
            input.prepare(now, window_id);
            input.clear_all();
        }
    }

    // Check Tap-Hold resolution FIRST
    if is_key_down {
        let mut early_trigger = Vec::new();
        if let Ok(mut pending) = PENDING_TAP_HOLDS.lock() {
            if !pending.contains_key(&vk_code) {
                for info in pending.values_mut() {
                    if !info.is_held {
                        info.is_held = true;
                        early_trigger.push(info.hold_actions.clone());
                    }
                }
            }
        }
        for actions in early_trigger {
            execute_actions(&actions, simulator, &ctx_arc, true, state, 0);
        }

        if let Some(rules) = engine_schema.tap_hold_map.get(&vk_code) {
            let Some(ctx) = try_read_ctx(&ctx_arc) else {
                return EventAction::PassThrough;
            };
            for rule in rules {
                if check_conditions(&rule.conditions, &ctx) {
                    if let Ok(mut pending) = PENDING_TAP_HOLDS.lock() {
                        pending.insert(
                            vk_code,
                            PendingTapHold {
                                vk_code,
                                tap_actions: rule.tap_actions.clone(),
                                hold_actions: rule.hold_actions.clone(),
                                down_time: Instant::now(),
                                timeout_ms: rule.timeout_ms,
                                is_held: false,
                            },
                        );
                    }
                    return EventAction::Block;
                }
            }
        }
    } else {
        let mut tap_actions = None;
        let mut hold_actions = None;

        if let Ok(mut pending) = PENDING_TAP_HOLDS.lock() {
            if let Some(info) = pending.remove(&vk_code) {
                if info.is_held {
                    hold_actions = Some(info.hold_actions);
                } else {
                    tap_actions = Some(info.tap_actions);
                }
            }
        }

        if let Some(actions) = tap_actions {
            execute_actions(&actions, simulator, &ctx_arc, true, state, 0);
            execute_actions(&actions, simulator, &ctx_arc, false, state, 0);
            return EventAction::Block;
        } else if let Some(actions) = hold_actions {
            execute_actions(&actions, simulator, &ctx_arc, false, state, 0);
            return EventAction::Block;
        }
    }

    // Complete the exact rule selected on the physical key-down. Release is not
    // allowed to depend on the *current* modifiers/window/layer state.
    if !is_key_down {
        if let Ok(mut pending) = PENDING_KEY_UP_ACTIONS.lock() {
            if let Some(lifecycle) = pending.remove(&vk_code) {
                drop(pending);
                execute_actions(
                    &lifecycle.actions,
                    simulator,
                    &ctx_arc,
                    true,
                    state,
                    lifecycle.trigger_modifiers,
                );
                return execute_actions(
                    &lifecycle.actions,
                    simulator,
                    &ctx_arc,
                    false,
                    state,
                    lifecycle.trigger_modifiers,
                );
            }
        }
        if let Ok(mut active) = ACTIVE_KEY_DOWN_ACTIONS.lock() {
            if let Some(lifecycle) = active.remove(&vk_code) {
                drop(active);
                return execute_actions(
                    &lifecycle.actions,
                    simulator,
                    &ctx_arc,
                    false,
                    state,
                    lifecycle.trigger_modifiers,
                );
            }
        }
    } else {
        // Windows autorepeat produces extra key-downs without a matching up.
        // Preserve repeat for ordinary single-key rules, but keep modifier
        // combinations one-shot until the primary key is physically released.
        if let Ok(active) = ACTIVE_KEY_DOWN_ACTIONS.lock() {
            if let Some(lifecycle) = active.get(&vk_code).cloned() {
                drop(active);
                if lifecycle.trigger_modifiers == 0 {
                    return execute_actions(
                        &lifecycle.actions,
                        simulator,
                        &ctx_arc,
                        true,
                        state,
                        0,
                    );
                }
                return EventAction::Block;
            }
        }
        if let Ok(pending) = PENDING_KEY_UP_ACTIONS.lock() {
            if pending.contains_key(&vk_code) {
                return EventAction::Block;
            }
        }
    }

    if is_key_down {
        if let Some(rules) = engine_schema.keyboard_map.get(&vk_code) {
            let Some(ctx) = try_read_ctx(&ctx_arc) else {
                return EventAction::PassThrough;
            };
            for rule in rules {
                if !modifiers_match(rule.required_modifiers, event_modifiers)
                    || !check_conditions(&rule.conditions, &ctx)
                {
                    continue;
                }

                let lifecycle = InputLifecycle {
                    actions: rule.actions.clone(),
                    trigger_modifiers: rule.required_modifiers,
                };
                let trigger_on_down = rule.trigger_on_down;
                drop(ctx);

                if trigger_on_down {
                    if let Ok(mut active) = ACTIVE_KEY_DOWN_ACTIONS.lock() {
                        active.insert(vk_code, lifecycle.clone());
                    }
                    return execute_actions(
                        &lifecycle.actions,
                        simulator,
                        &ctx_arc,
                        true,
                        state,
                        lifecycle.trigger_modifiers,
                    );
                }

                // KeyUp rules suppress the source key from the first down, then
                // activate exactly once when that same physical press is released.
                // Mask Alt/Win immediately because their primary key is blocked.
                if lifecycle.trigger_modifiers != 0 {
                    send_commands(simulator, shell_mask_commands(current_physical_modifiers()));
                }
                if let Ok(mut pending) = PENDING_KEY_UP_ACTIONS.lock() {
                    pending.insert(vk_code, lifecycle);
                }
                return EventAction::Block;
            }
        }
    }

    EventAction::PassThrough
}

pub fn process_mouse_event(
    button: u8,
    x: i32,
    y: i32,
    delta: i32,
    horizontal_wheel: bool,
    is_move: bool,
    _flags: u32,
    is_down: bool,
    state: Option<&DaemonStateRef>,
) -> EventAction {
    let state_ref = match state {
        Some(s) => s,
        None => return EventAction::PassThrough,
    };

    let s = match state_ref.read() {
        Ok(s) => s,
        Err(_) => return EventAction::PassThrough,
    };

    if !s.mouse_hook_enabled {
        return EventAction::PassThrough;
    }

    if is_down {
        if let Ok(mut input) = s.text_input.lock() {
            input.clear_all();
        }
    }

    let simulator = match &s.simulator {
        Some(sim) => sim,
        None => return EventAction::PassThrough,
    };
    let engine_schema = &s.engine_schema;
    let ctx_arc = match crate::trackers::context_tracker::get_context() {
        Some(c) => c,
        None => return EventAction::PassThrough,
    };
    let window_id = try_read_ctx(&ctx_arc)
        .map(|ctx| ctx.active_window_id)
        .unwrap_or(0);
    if let Ok(mut input) = s.advanced_input.lock() {
        input.prepare_window(window_id);
    }

    // Wheel/hwheel are standalone source events, so matched wheel rules can be
    // consumed immediately without creating a down/up lifecycle.
    if delta != 0 {
        if let Some(key) = wheel_key(delta, horizontal_wheel) {
            if let Some(rules) = engine_schema.mouse_wheel_map.get(&key) {
                let Some(ctx) = try_read_ctx(&ctx_arc) else {
                    return EventAction::PassThrough;
                };
                for rule in rules {
                    if !check_conditions(&rule.conditions, &ctx) {
                        continue;
                    }
                    let actions = rule.actions.clone();
                    drop(ctx);
                    execute_actions(&actions, simulator, &ctx_arc, true, state, 0);
                    execute_actions(&actions, simulator, &ctx_arc, false, state, 0);
                    return EventAction::Block;
                }
            }
        }
        return EventAction::PassThrough;
    }

    // Feed any active gesture before ordinary mouse-move triggers. The gesture
    // state is bounded by configured rules and max 8 directions per rule.
    if is_move {
        if let Ok(mut input) = s.advanced_input.lock() {
            input.gesture_move(x, y);
        }
        if !engine_schema.mouse_move_rules.is_empty() {
            let Some(ctx) = try_read_ctx(&ctx_arc) else {
                return EventAction::PassThrough;
            };
            let now = Instant::now();
            for rule in &engine_schema.mouse_move_rules {
                if !check_conditions(&rule.conditions, &ctx) {
                    continue;
                }
                let should_fire = MOUSE_MOVE_GATES
                    .lock()
                    .map(|mut gates| {
                        gates.entry(rule.rule_id_hash).or_default().should_fire(
                            x,
                            y,
                            now,
                            rule.min_distance,
                            Duration::from_millis(u64::from(rule.cooldown_ms)),
                        )
                    })
                    .unwrap_or(false);
                if should_fire {
                    let actions = rule.actions.clone();
                    drop(ctx);
                    execute_actions(&actions, simulator, &ctx_arc, true, state, 0);
                    execute_actions(&actions, simulator, &ctx_arc, false, state, 0);
                    break;
                }
            }
        }
        return EventAction::PassThrough;
    }

    // Start an observational mouse-gesture session on anchor-button down.
    if is_down && button != 255 {
        let specs = engine_schema
            .mouse_gesture_rules
            .iter()
            .filter(|rule| rule.code == button)
            .map(|rule| GestureSpec {
                rule_id_hash: rule.rule_id_hash,
                directions: rule.directions.clone(),
                min_distance: rule.min_distance,
            })
            .collect::<Vec<_>>();
        if let Ok(mut input) = s.advanced_input.lock() {
            input.start_gesture(button, x, y, specs);
        }
    }

    // Finish gestures on anchor release and fire the highest-priority eligible
    // completed rule. The physical button event itself remains pass-through.
    if !is_down && button != 255 {
        let completed = s
            .advanced_input
            .lock()
            .map(|mut input| input.finish_gesture(button))
            .unwrap_or_default();
        if !completed.is_empty() {
            if let Some(ctx) = try_read_ctx(&ctx_arc) {
                for rule in &engine_schema.mouse_gesture_rules {
                    if completed.contains(&rule.rule_id_hash)
                        && check_conditions(&rule.rule.conditions, &ctx)
                    {
                        let actions = rule.rule.actions.clone();
                        drop(ctx);
                        execute_actions(&actions, simulator, &ctx_arc, true, state, 0);
                        execute_actions(&actions, simulator, &ctx_arc, false, state, 0);
                        break;
                    }
                }
            }
        }
    }

    // Double-click detection is also additive: the first click is never delayed
    // while waiting for a possible second click.
    if is_down && button != 255 {
        if let Some(rules) = engine_schema.mouse_double_click_map.get(&button) {
            let (interval, max_dx, max_dy) = system_double_click_limits();
            let is_double = DOUBLE_CLICK_DETECTOR
                .lock()
                .map(|mut detector| {
                    detector.register_down(button, x, y, Instant::now(), interval, max_dx, max_dy)
                })
                .unwrap_or(false);
            if is_double {
                if let Some(ctx) = try_read_ctx(&ctx_arc) {
                    for rule in rules {
                        if !check_conditions(&rule.conditions, &ctx) {
                            continue;
                        }
                        let actions = rule.actions.clone();
                        drop(ctx);
                        execute_actions(&actions, simulator, &ctx_arc, true, state, 0);
                        execute_actions(&actions, simulator, &ctx_arc, false, state, 0);
                        break;
                    }
                }
            }
        }
    }

    // Ordinary button down/up lifecycle stays exactly as before.
    if !is_down {
        if let Ok(mut pending) = PENDING_MOUSE_UP_ACTIONS.lock() {
            if let Some(actions) = pending.remove(&button) {
                drop(pending);
                execute_actions(&actions, simulator, &ctx_arc, true, state, 0);
                return execute_actions(&actions, simulator, &ctx_arc, false, state, 0);
            }
        }
        if let Ok(mut active) = ACTIVE_MOUSE_DOWN_ACTIONS.lock() {
            if let Some(actions) = active.remove(&button) {
                drop(active);
                return execute_actions(&actions, simulator, &ctx_arc, false, state, 0);
            }
        }
    } else {
        if ACTIVE_MOUSE_DOWN_ACTIONS
            .lock()
            .map(|active| active.contains_key(&button))
            .unwrap_or(false)
            || PENDING_MOUSE_UP_ACTIONS
                .lock()
                .map(|pending| pending.contains_key(&button))
                .unwrap_or(false)
        {
            return EventAction::Block;
        }

        if let Some(rules) = engine_schema.mouse_map.get(&button) {
            let Some(ctx) = try_read_ctx(&ctx_arc) else {
                return EventAction::PassThrough;
            };
            for rule in rules {
                if !check_conditions(&rule.conditions, &ctx) {
                    continue;
                }
                let actions = rule.actions.clone();
                let trigger_on_down = rule.trigger_on_down;
                drop(ctx);
                if trigger_on_down {
                    if let Ok(mut active) = ACTIVE_MOUSE_DOWN_ACTIONS.lock() {
                        active.insert(button, actions.clone());
                    }
                    return execute_actions(&actions, simulator, &ctx_arc, true, state, 0);
                }
                if let Ok(mut pending) = PENDING_MOUSE_UP_ACTIONS.lock() {
                    pending.insert(button, actions);
                }
                return EventAction::Block;
            }
        }
    }

    EventAction::PassThrough
}

pub fn vk_to_key_name(vk: u8) -> String {
    match vk {
        0x14 => "CapsLock".to_string(),
        0x1B => "Escape".to_string(),
        0x20 => "Space".to_string(),
        0x0D => "Enter".to_string(),
        0x08 => "Backspace".to_string(),
        0x09 => "Tab".to_string(),
        0x2E => "Delete".to_string(),
        0x2D => "Insert".to_string(),
        0x21 => "PageUp".to_string(),
        0x22 => "PageDown".to_string(),
        0x23 => "End".to_string(),
        0x24 => "Home".to_string(),
        0x25 => "Left".to_string(),
        0x26 => "Up".to_string(),
        0x27 => "Right".to_string(),
        0x28 => "Down".to_string(),
        0xAD => "VolumeMute".to_string(),
        0xAE => "VolumeDown".to_string(),
        0xAF => "VolumeUp".to_string(),
        0x12 | 0xA4 | 0xA5 => "Alt".to_string(),
        0x11 | 0xA2 | 0xA3 => "Ctrl".to_string(),
        0x10 | 0xA0 | 0xA1 => "Shift".to_string(),
        0x5B | 0x5C => "Win".to_string(),
        0x70 => "F1".to_string(),
        0x71 => "F2".to_string(),
        0x72 => "F3".to_string(),
        0x73 => "F4".to_string(),
        0x74 => "F5".to_string(),
        0x75 => "F6".to_string(),
        0x76 => "F7".to_string(),
        0x77 => "F8".to_string(),
        0x78 => "F9".to_string(),
        0x79 => "F10".to_string(),
        0x7A => "F11".to_string(),
        0x7B => "F12".to_string(),
        val @ 0x41..=0x5A => ((val - 0x41 + b'A') as char).to_string(),
        val @ 0x30..=0x39 => ((val - 0x30 + b'0') as char).to_string(),
        other => format!("VK_{}", other),
    }
}

/// VK → символ по US-раскладке с учётом текущего shift-состояния события
/// (на Windows использовался ToUnicodeEx с активной раскладкой).
fn vk_to_char(vk: u8, event_modifiers: u16) -> Option<char> {
    let shift = event_modifiers & crate::schemas::frontend::key_modifiers::SHIFT != 0;
    crate::daemon::keymap::vk_to_char_us(vk, shift)
}

#[cfg(test)]
mod chord_tests {
    use super::*;

    #[test]
    fn generic_modifier_accepts_either_side() {
        assert!(modifiers_match(key_modifiers::CTRL, key_modifiers::LCTRL));
        assert!(modifiers_match(key_modifiers::CTRL, key_modifiers::RCTRL));
        assert!(modifiers_match(
            key_modifiers::CTRL | key_modifiers::SHIFT,
            key_modifiers::LCTRL | key_modifiers::RSHIFT,
        ));
    }

    #[test]
    fn exact_side_modifier_is_strict() {
        assert!(modifiers_match(key_modifiers::LCTRL, key_modifiers::LCTRL));
        assert!(!modifiers_match(key_modifiers::LCTRL, key_modifiers::RCTRL));
        assert!(!modifiers_match(
            key_modifiers::LCTRL,
            key_modifiers::LCTRL | key_modifiers::RCTRL,
        ));
    }

    #[test]
    fn unrequested_extra_modifier_does_not_match() {
        assert!(!modifiers_match(
            key_modifiers::CTRL,
            key_modifiers::LCTRL | key_modifiers::SHIFT,
        ));
        assert!(modifiers_match(0, 0));
        assert!(!modifiers_match(0, key_modifiers::ALT));
    }

    #[test]
    fn lifecycle_keeps_original_modifier_requirement() {
        let lifecycle = InputLifecycle {
            actions: vec![EngineAction::RemapKey {
                code: 0x42,
                modifiers: 0,
            }],
            trigger_modifiers: key_modifiers::CTRL | key_modifiers::SHIFT,
        };
        assert_eq!(
            lifecycle.trigger_modifiers,
            key_modifiers::CTRL | key_modifiers::SHIFT,
        );
        assert_eq!(lifecycle.actions.len(), 1);
    }
}

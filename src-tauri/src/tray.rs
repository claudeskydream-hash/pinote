use std::{
    fs::File,
    io::Read,
    path::Path,
    sync::Mutex,
};

use log::error;
use serde::Deserialize;
use tauri::{
    Manager,
    menu::{Menu, MenuItem, PredefinedMenuItem, Submenu},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
};

use crate::{
    locale, note_window_visibility, open_new_note_window, open_note_path_from_tray,
    window::{restore_hidden_window, restore_latest_hidden_window, show_settings_window},
};

/// 历史子菜单最多列多少条，更多的去设置页「历史记录」里找
const HISTORY_MENU_LIMIT: usize = 30;
/// 标题最多显示的字符数
const HISTORY_TITLE_MAX_CHARS: usize = 28;
/// 取标题只读文件开头这么多字节，避免大文件拖慢托盘
const HISTORY_TITLE_READ_BYTES: u64 = 4096;
const HISTORY_FILE_NAME: &str = "history.json";
const HISTORY_ITEM_PREFIX: &str = "history:";
const HISTORY_MORE_ID: &str = "history_more";
const HISTORY_EMPTY_ID: &str = "history_empty";

struct TrayMenuItems {
    new_note: MenuItem<tauri::Wry>,
    history: Submenu<tauri::Wry>,
    show_hide: MenuItem<tauri::Wry>,
    settings: MenuItem<tauri::Wry>,
    quit: MenuItem<tauri::Wry>,
    /// 历史子菜单第 N 项对应的便签路径
    history_paths: Mutex<Vec<String>>,
}

#[derive(Deserialize)]
struct HistoryStore {
    #[serde(default)]
    entries: std::collections::HashMap<String, HistoryEntry>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HistoryEntry {
    #[serde(default)]
    note_path: String,
    #[serde(default)]
    last_opened_at: String,
}

pub fn setup_tray(app: &tauri::AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    let text = locale::current_text(app);
    let new_note = MenuItem::with_id(app, "new_note", text.new_note, true, None::<&str>)?;
    let history = Submenu::with_id(app, "history", text.history_notes, true)?;
    let show_hide = MenuItem::with_id(app, "show_hide", text.restore_hidden, true, None::<&str>)?;
    let settings = MenuItem::with_id(app, "settings", text.open_settings, true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", text.quit, true, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(app, &[&new_note, &history, &separator, &show_hide, &settings, &quit])?;

    app.manage(TrayMenuItems {
        new_note,
        history,
        show_hide,
        settings,
        quit,
        history_paths: Mutex::new(Vec::new()),
    });
    refresh_history_menu(app);

    let _tray = TrayIconBuilder::new()
        .icon(app.default_window_icon().unwrap().clone())
        .tooltip("Pinote")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| {
            let id = event.id.as_ref();
            if let Some(index) = id.strip_prefix(HISTORY_ITEM_PREFIX) {
                open_history_item(app, index);
                return;
            }
            match id {
                "new_note" => {
                    open_new_note_window(app);
                }
                "show_hide" => {
                    restore_hidden_window(app);
                }
                "settings" | HISTORY_MORE_ID => {
                    let section = (id == HISTORY_MORE_ID).then(|| "history".to_string());
                    let handle = app.clone();
                    std::thread::spawn(move || {
                        let _ = show_settings_window(&handle, section);
                    });
                }
                "quit" => {
                    app.exit(0);
                }
                _ => {}
            }
        })
        .on_tray_icon_event(|tray, event| match event {
            // 鼠标移到托盘图标上、右键按下时刷新历史，菜单弹出时就是最新的
            TrayIconEvent::Enter { .. }
            | TrayIconEvent::Click {
                button: MouseButton::Right,
                button_state: MouseButtonState::Down,
                ..
            } => {
                refresh_history_menu(tray.app_handle());
            }
            TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } => {
                open_last_note(tray.app_handle());
            }
            _ => {}
        })
        .build(app)?;

    Ok(())
}

pub fn sync_locale(app: &tauri::AppHandle) -> Result<(), tauri::Error> {
    let Some(items) = app.try_state::<TrayMenuItems>() else {
        return Ok(());
    };
    let text = locale::current_text(app);
    items.new_note.set_text(text.new_note)?;
    items.history.set_text(text.history_notes)?;
    items.show_hide.set_text(text.restore_hidden)?;
    items.settings.set_text(text.open_settings)?;
    items.quit.set_text(text.quit)?;
    refresh_history_menu(app);
    Ok(())
}

/// 左键托盘：先恢复最近隐藏的便签；没有就重开最近关闭的那张；
/// 历史里的便签都开着就聚焦最近的一张；一张历史都没有才新建
fn open_last_note(app: &tauri::AppHandle) {
    if restore_latest_hidden_window(app) {
        return;
    }
    let entries = load_history_entries(app);
    let target = entries
        .iter()
        .find(|path| note_window_visibility(app, path).is_none())
        .or_else(|| entries.first())
        .cloned();
    match target {
        Some(path) => open_note_path_from_tray(app, path),
        None => open_new_note_window(app),
    }
}

fn open_history_item(app: &tauri::AppHandle, index: &str) {
    let Some(items) = app.try_state::<TrayMenuItems>() else {
        return;
    };
    let path = index.parse::<usize>().ok().and_then(|index| {
        items
            .history_paths
            .lock()
            .ok()
            .and_then(|paths| paths.get(index).cloned())
    });
    if let Some(path) = path {
        open_note_path_from_tray(app, path);
    }
}

/// 按 history.json 重建「历史便签」子菜单：最近打开的在上，文件已不存在的跳过
fn refresh_history_menu(app: &tauri::AppHandle) {
    if let Err(err) = rebuild_history_menu(app) {
        error!("tray_history_refresh_failed error={err}");
    }
}

fn rebuild_history_menu(app: &tauri::AppHandle) -> Result<(), tauri::Error> {
    let Some(items) = app.try_state::<TrayMenuItems>() else {
        return Ok(());
    };
    let text = locale::current_text(app);
    let entries = load_history_entries(app);

    for _ in 0..items.history.items()?.len() {
        items.history.remove_at(0)?;
    }

    let mut paths = Vec::with_capacity(entries.len());
    for (index, note_path) in entries.into_iter().enumerate() {
        let marker = match note_window_visibility(app, &note_path) {
            Some(true) => "● ",
            Some(false) => "○ ",
            None => "    ",
        };
        let label = format!("{marker}{}", read_note_title(&note_path));
        let item = MenuItem::with_id(
            app,
            format!("{HISTORY_ITEM_PREFIX}{index}"),
            label,
            true,
            None::<&str>,
        )?;
        items.history.append(&item)?;
        paths.push(note_path);
    }

    if paths.is_empty() {
        let empty = MenuItem::with_id(app, HISTORY_EMPTY_ID, text.history_empty, false, None::<&str>)?;
        items.history.append(&empty)?;
    }
    items.history.append(&PredefinedMenuItem::separator(app)?)?;
    let more = MenuItem::with_id(app, HISTORY_MORE_ID, text.history_more, true, None::<&str>)?;
    items.history.append(&more)?;

    if let Ok(mut stored) = items.history_paths.lock() {
        *stored = paths;
    }
    Ok(())
}

fn load_history_entries(app: &tauri::AppHandle) -> Vec<String> {
    let Ok(data_dir) = app.path().app_data_dir() else {
        return Vec::new();
    };
    let Ok(raw) = std::fs::read_to_string(data_dir.join(HISTORY_FILE_NAME)) else {
        return Vec::new();
    };
    let Ok(store) = serde_json::from_str::<HistoryStore>(&raw) else {
        return Vec::new();
    };
    let mut entries: Vec<HistoryEntry> = store
        .entries
        .into_values()
        .filter(|entry| !entry.note_path.trim().is_empty() && Path::new(&entry.note_path).is_file())
        .collect();
    // ISO 时间串可直接按字典序比较
    entries.sort_by(|left, right| right.last_opened_at.cmp(&left.last_opened_at));
    let mut seen = std::collections::HashSet::new();
    entries
        .into_iter()
        .filter(|entry| seen.insert(entry.note_path.to_lowercase()))
        .take(HISTORY_MENU_LIMIT)
        .map(|entry| entry.note_path)
        .collect()
}

/// 取便签正文第一行非空文字作标题，去掉 Markdown 标记；空便签用文件名
fn read_note_title(note_path: &str) -> String {
    let mut buffer = Vec::new();
    if let Ok(file) = File::open(note_path) {
        let _ = file.take(HISTORY_TITLE_READ_BYTES).read_to_end(&mut buffer);
    }
    let content = String::from_utf8_lossy(&buffer);
    let title = content
        .lines()
        .map(strip_markdown_prefix)
        .find(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| {
            Path::new(note_path)
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or("Untitled")
                .to_string()
        });
    let mut shown: String = title.chars().take(HISTORY_TITLE_MAX_CHARS).collect();
    if title.chars().count() > HISTORY_TITLE_MAX_CHARS {
        shown.push('…');
    }
    // Windows 菜单里单个 & 会被当成助记符
    shown.replace('&', "&&")
}

fn strip_markdown_prefix(line: &str) -> &str {
    let mut text = line.trim().trim_start_matches('\u{feff}').trim();
    text = text.trim_start_matches(['#', '>']).trim_start();
    for marker in ["- [ ] ", "- [x] ", "- [X] ", "* [ ] ", "* [x] ", "- ", "* ", "+ "] {
        if let Some(rest) = text.strip_prefix(marker) {
            text = rest.trim_start();
            break;
        }
    }
    text.trim()
}

use lazy_static::lazy_static;
use nexus::{
    gui::{register_render, render, RenderType},
    imgui::{InputText, TreeNodeFlags, Ui, Window},
    keybind::{register_keybind_with_string, unregister_keybind},
    log::{self, LogLevel},
    paths::get_addon_dir,
    quick_access::{add_quick_access, remove_quick_access},
    texture::load_texture_from_file,
    AddonFlags, UpdateProvider,
};
use rfd::FileDialog;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    ffi::{c_char, CStr},
    fs,
    io::Cursor,
    panic,
    path::{Path, PathBuf},
    process::Command,
    sync::Mutex,
};
use sysinfo::System;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use windows_icons::get_icon_base64_by_path;

// --- Configuration & State Management ---
#[derive(Serialize, Deserialize, Clone, PartialEq)]
enum LaunchTrigger { OnAddonLoad, OnKeybind }
#[derive(Serialize, Deserialize, Clone)]
struct ProgramToLaunch { name: String, path: String, trigger: LaunchTrigger, close_on_unload: bool }
#[derive(Serialize, Deserialize, Clone)]
struct Config { programs_to_launch: Vec<ProgramToLaunch>, programs_to_kill: Vec<String>, wait_seconds: u64 }
impl Default for Config { fn default() -> Self { Self { programs_to_launch: Vec::new(), programs_to_kill: Vec::new(), wait_seconds: 2 } } }

lazy_static! {
    static ref CONFIG: Mutex<Config> = Mutex::new(Config::default());
    static ref SYSTEM_INFO: Mutex<System> = Mutex::new(System::new_all());
    static ref ICON_CACHE: Mutex<HashMap<String, PathBuf>> = Mutex::new(HashMap::new());
    static ref LAUNCH_INPUT: Mutex<String> = Mutex::new(String::with_capacity(260));
    static ref KILL_INPUT: Mutex<String> = Mutex::new(String::with_capacity(64));
    static ref PENDING_LAUNCH_CONFIRMATION: Mutex<Option<String>> = Mutex::new(None);
}

// --- Helper Functions ---
fn get_filename_from_path(path_str: &str) -> Option<String> { Path::new(path_str).file_name().and_then(|s| s.to_str()).map(|s| s.to_string()) }
fn is_process_running(process_name: &str) -> bool {
    let mut sys = SYSTEM_INFO.lock().unwrap();
    sys.refresh_processes();
    sys.processes().values().any(|p| p.name().eq_ignore_ascii_case(process_name))
}
fn force_launch_process(path: &str) {
    log::log(LogLevel::Info, "SYSTEM", &format!("Attempting to launch: {}", path));
    let mut command = Command::new(path);
    if let Some(parent_dir) = Path::new(path).parent() { command.current_dir(parent_dir); }
    if let Err(e) = command.spawn() { log::log(LogLevel::Critical, "SYSTEM", &format!("Failed to launch process '{}': {}", path, e)); }
}
fn launch_process(path: &str) {
    if let Some(filename) = get_filename_from_path(path) {
        if is_process_running(&filename) { *PENDING_LAUNCH_CONFIRMATION.lock().unwrap() = Some(path.to_string()); }
        else { force_launch_process(path); }
    } else { force_launch_process(path); }
}
fn launch_process_by_name(name: &str) {
    let config = CONFIG.lock().unwrap();
    if let Some(program) = config.programs_to_launch.iter().find(|p| p.name == name) {
        launch_process(&program.path);
    } else { log::log(LogLevel::Critical, "SYSTEM", &format!("Program with name '{}' not found for keybind/QA.", name)); }
}

// --- Quick Access & Icon Management ---
fn create_placeholder_icon(path: &Path) {
    image::RgbaImage::new(32, 32).save_with_format(path, image::ImageFormat::Png).ok();
}

fn extract_and_save_icon(exe_path: &str, save_path: &Path) -> Result<(), String> {
    let base64_str = get_icon_base64_by_path(exe_path).map_err(|e| e.to_string())?;
    let image_data = BASE64.decode(base64_str).map_err(|e| e.to_string())?;
    let img = image::load(Cursor::new(&image_data), image::ImageFormat::Png).map_err(|e| e.to_string())?;
    img.save(save_path).map_err(|e| e.to_string())
}

fn setup_quick_access_for_program(program: &ProgramToLaunch) {
    let addon_dir = match get_addon_dir(env!("CARGO_PKG_NAME")) { Some(dir) => dir, None => return };
    let icons_dir = addon_dir.join("icons");
    fs::create_dir_all(&icons_dir).ok();
    
    let icon_path = icons_dir.join(format!("{}.png", program.name));
    if let Err(e) = extract_and_save_icon(&program.path, &icon_path) {
        log::log(LogLevel::Warning, "SYSTEM", &format!("Could not extract icon for {}: {}. Using placeholder.", program.name, e));
        create_placeholder_icon(&icon_path);
    }

    let qa_item_id = format!("QA_ITEM_{}", program.name);
    let qa_tex_id = format!("QA_TEX_{}", program.name);
    let qa_kb_id = format!("QA_KB_{}", program.name);
    
    load_texture_from_file(&qa_tex_id, &icon_path, None);
    ICON_CACHE.lock().unwrap().insert(program.name.clone(), icon_path);
    
    register_keybind_with_string(&qa_kb_id, keybind_callback, "").revert_on_unload();
    
    // MODIFIED: This now correctly links the keybind to the quick access item.
    // This will show the assigned key in the tooltip, or `((null))` if it's unbound.
    add_quick_access(
        &qa_item_id,
        &qa_tex_id,
        &qa_tex_id,
        &qa_kb_id, // <-- Correctly linked
        &get_filename_from_path(&program.path).unwrap_or_default(),
    ).revert_on_unload();
}

fn teardown_quick_access_for_program(program: &ProgramToLaunch) {
    let qa_item_id = format!("QA_ITEM_{}", program.name);
    let qa_kb_id = format!("QA_KB_{}", program.name);
    remove_quick_access(&qa_item_id);
    unregister_keybind(&qa_kb_id);
    if let Some(path) = ICON_CACHE.lock().unwrap().remove(&program.name) {
        fs::remove_file(path).ok();
    }
}

extern "C-unwind" fn keybind_callback(identifier: *const c_char, is_release: bool) {
    if is_release || identifier.is_null() { return; }
    let result = panic::catch_unwind(|| {
        let identifier_cstr = unsafe { CStr::from_ptr(identifier) };
        if let Ok(id_str) = identifier_cstr.to_str() {
            if let Some(name) = id_str.strip_prefix("LAUNCH_") {
                launch_process_by_name(name);
            } else if let Some(name) = id_str.strip_prefix("QA_KB_") {
                launch_process_by_name(name);
            }
        }
    });
    if result.is_err() { log::log(LogLevel::Critical, "SYSTEM", "Panic caught in keybind handler!"); }
}

fn get_config_path() -> PathBuf { get_addon_dir(env!("CARGO_PKG_NAME")).expect("Addon directory should exist").join("settings.ron") }
fn load_config_from_file() {
    let path = get_config_path();
    let loaded_config = fs::read_to_string(&path).ok().and_then(|c| ron::from_str(&c).ok()).unwrap_or_default();
    *CONFIG.lock().unwrap() = loaded_config;
}
fn save_config_to_file() {
    let path = get_config_path();
    if let Some(parent) = path.parent() { fs::create_dir_all(parent).ok(); }
    if let Ok(ser) = ron::to_string(&*CONFIG.lock().unwrap()) { fs::write(path, ser).ok(); }
}

fn load() {
    load_config_from_file();
    let config = CONFIG.lock().unwrap().clone();
    log::log(LogLevel::Info, "SYSTEM", "Loading Assisted Deployment and Departure...");
    for program in config.programs_to_launch.iter() {
        setup_quick_access_for_program(program);
        if program.trigger == LaunchTrigger::OnAddonLoad { launch_process(&program.path); }
        else if program.trigger == LaunchTrigger::OnKeybind {
            register_keybind_with_string(format!("LAUNCH_{}", program.name), keybind_callback, "").revert_on_unload();
        }
    }
    register_render(RenderType::OptionsRender, render!(render_options)).revert_on_unload();
    register_render(RenderType::Render, render!(render_popup)).revert_on_unload();
}

fn unload() {
    save_config_to_file();
    let config = CONFIG.lock().unwrap().clone();
    for program in config.programs_to_launch.iter() { teardown_quick_access_for_program(program); }
    let mut kill_list = config.programs_to_kill;
    for program in &config.programs_to_launch {
        if program.close_on_unload {
            if let Some(filename) = get_filename_from_path(&program.path) {
                if !kill_list.contains(&filename) { kill_list.push(filename); }
            }
        }
    }
    log::log(LogLevel::Info, "SYSTEM", &format!("Initial kill list: {:?}", kill_list));
    if !kill_list.is_empty() {
        if config.wait_seconds > 0 {
            log::log(LogLevel::Info, "SYSTEM", &format!("Waiting {}s...", config.wait_seconds));
            std::thread::sleep(std::time::Duration::from_secs(config.wait_seconds));
        }
        cleanup_processes(&kill_list);
    }
    log::log(LogLevel::Info, "SYSTEM", "Unloaded.");
}

fn cleanup_processes(targets: &[String]) {
    let safe_targets: Vec<_> = targets.iter().filter(|n| !n.eq_ignore_ascii_case("Gw2-64.exe")).collect();
    log::log(LogLevel::Info, "SYSTEM", &format!("SAFE kill list: {:?}", safe_targets));
    if safe_targets.is_empty() { return; }
    let mut sys = SYSTEM_INFO.lock().unwrap();
    sys.refresh_processes();
    for target in safe_targets {
        for p in sys.processes().values().filter(|p| p.name().eq_ignore_ascii_case(target)) {
            log::log(LogLevel::Info, "SYSTEM", &format!("Killing: {} (PID: {})", p.name(), p.pid()));
            p.kill();
        }
    }
}

// --- UI Rendering ---
fn render_popup(ui: &Ui) {
    let mut pending_launch = PENDING_LAUNCH_CONFIRMATION.lock().unwrap();
    let mut close_popup = false;
    let path_to_launch = pending_launch.clone();

    if let Some(path) = path_to_launch {
        let filename = get_filename_from_path(&path).unwrap_or_else(|| "program".to_string());
        let mut open = true;
        Window::new(&format!("'{}' Already Running", filename))
            .opened(&mut open).always_auto_resize(true).collapsible(false).focus_on_appearing(true)
            .build(ui, || {
                ui.text("This program is already running.");
                ui.text("Do you want to open another instance?");
                ui.separator();
                if ui.button("Yes") { force_launch_process(&path); close_popup = true; }
                ui.same_line();
                if ui.button("No") { close_popup = true; }
            });
        if !open { close_popup = true; }
    }
    if close_popup { *pending_launch = None; }
}

fn render_options(ui: &Ui) {
    let mut config = CONFIG.lock().unwrap();
    let mut changed = false;
    ui.text("Manage external programs to launch/kill.");
    ui.separator();
    if ui.collapsing_header("Programs to Launch", TreeNodeFlags::DEFAULT_OPEN) {
        let mut to_remove_idx = None;
        for (i, prog) in config.programs_to_launch.iter_mut().enumerate() {
            ui.text(&prog.path);
            ui.same_line();
            if ui.small_button(&format!("-##launch{}", i)) {
                teardown_quick_access_for_program(prog);
                if prog.trigger == LaunchTrigger::OnKeybind { unregister_keybind(&format!("LAUNCH_{}", prog.name)); }
                to_remove_idx = Some(i); changed = true;
            }
            let original_trigger = prog.trigger.clone();
            if ui.radio_button_bool(&format!("On Addon Start##{}", i), prog.trigger == LaunchTrigger::OnAddonLoad) { prog.trigger = LaunchTrigger::OnAddonLoad; }
            ui.same_line();
            if ui.radio_button_bool(&format!("On Keybind##{}", i), prog.trigger == LaunchTrigger::OnKeybind) { prog.trigger = LaunchTrigger::OnKeybind; }
            if original_trigger != prog.trigger {
                changed = true;
                if original_trigger == LaunchTrigger::OnKeybind { unregister_keybind(&format!("LAUNCH_{}", prog.name)); }
                if prog.trigger == LaunchTrigger::OnKeybind { register_keybind_with_string(format!("LAUNCH_{}", prog.name), keybind_callback, "").revert_on_unload(); }
            }
            if prog.trigger == LaunchTrigger::OnKeybind { ui.text_colored([0.6, 0.6, 0.6, 1.0], format!("Keybind ID: LAUNCH_{}", prog.name)); }
            if ui.checkbox(&format!("Close on unload##{}", i), &mut prog.close_on_unload) { changed = true; }
            ui.separator();
        }
        if let Some(i) = to_remove_idx { config.programs_to_launch.remove(i); }
        ui.text("Add new program:");
        let mut launch_input = LAUNCH_INPUT.lock().unwrap();
        ui.group(|| {
            ui.set_next_item_width(300.0);
            InputText::new(ui, "##add_launch", &mut *launch_input).build();
            ui.same_line();
            if ui.button("Browse...") {
                let file = FileDialog::new()
                    .add_filter("Executable", &["exe"])
                    .set_title("Select a program to launch")
                    .pick_file();
                if let Some(path) = file {
                    *launch_input = path.to_string_lossy().to_string();
                }
            }
            ui.same_line();
            if ui.button("+##add_launch_btn") && !launch_input.is_empty() {
                let path = launch_input.clone();
                if let Some(name) = get_filename_from_path(&path) {
                    if !config.programs_to_launch.iter().any(|p| p.name == name) {
                        let new_prog = ProgramToLaunch { name, path, trigger: LaunchTrigger::OnAddonLoad, close_on_unload: false };
                        setup_quick_access_for_program(&new_prog);
                        config.programs_to_launch.push(new_prog);
                        changed = true;
                    } else { log::log(LogLevel::Warning, "SYSTEM", &format!("Program '{}' already exists.", name)); }
                }
                launch_input.clear();
            }
        });
    }
    if ui.collapsing_header("Programs to Kill on Unload", TreeNodeFlags::empty()) {
        let mut to_remove_idx = None;
        for (i, name) in config.programs_to_kill.iter().enumerate() {
            ui.text(name);
            ui.same_line();
            if ui.small_button(&format!("-##kill{}", i)) { to_remove_idx = Some(i); changed = true; }
        }
        if let Some(i) = to_remove_idx { config.programs_to_kill.remove(i); }
        ui.text("Add process name to kill list:");
        let mut kill_input = KILL_INPUT.lock().unwrap();
        ui.group(|| {
            ui.set_next_item_width(300.0);
            InputText::new(ui, "##add_kill", &mut *kill_input).build();
            ui.same_line();
            if ui.button("+##add_kill_btn") && !kill_input.is_empty() {
                if !config.programs_to_kill.contains(&*kill_input) {
                    config.programs_to_kill.push(kill_input.clone());
                    changed = true;
                }
                kill_input.clear();
            }
        });
    }
    ui.separator();
    ui.text("Wait time before cleanup (seconds):");
    ui.same_line();
    ui.set_next_item_width(40.0);
    let mut wait_str = config.wait_seconds.to_string();
    if InputText::new(ui, "##wait_time", &mut wait_str).chars_decimal(true).build() {
        if let Ok(val) = wait_str.parse::<u64>() {
            if config.wait_seconds != val { config.wait_seconds = val; changed = true; }
        }
    }
    if changed { drop(config); save_config_to_file(); }
}

nexus::export! {
    name: "Assisted Deployment and Departure",
    signature: -128175,
    flags: AddonFlags::None,
    load,
    unload,
    provider: UpdateProvider::GitHub,
    update_link: "https://github.com/qjv/assisted_deployment_and_departure"
}
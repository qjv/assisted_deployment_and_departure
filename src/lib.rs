use std::io;
use std::path::Path;

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use lazy_static::lazy_static;
use nexus::{
    gui::{register_render, render, RenderType},
    imgui::{InputText, TreeNodeFlags, Ui, Window},
    keybind::{register_keybind_with_string},
    log::{self, LogLevel},
    paths::get_addon_dir,
    quick_access::{add_quick_access, remove_quick_access},
    texture::get_texture_or_create_from_file,
    AddonFlags, UpdateProvider,
};
use rfd::FileDialog;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    ffi::{c_char, CStr},
    fs,
    io::Cursor,
    panic,
    path::PathBuf,
    process::Command,
    sync::Mutex,
};
use sysinfo::System;
use windows_icons::get_icon_base64_by_path;

// --- Configuration & State Management ---
fn default_true() -> bool {
    true
}

#[derive(Serialize, Deserialize, Clone, PartialEq)]
enum LaunchTrigger {
    OnAddonLoad,
    OnKeybind,
}

// Legacy structure for backwards compatibility
#[derive(Deserialize)]
struct LegacyProgramToLaunch {
    name: String,
    #[serde(default)]
    display_name: String,
    path: String,
    trigger: LaunchTrigger,
    close_on_unload: bool,
    #[serde(default = "default_true")]
    show_in_quick_access: bool,
}

#[derive(Serialize, Deserialize, Clone)]
struct ProgramToLaunch {
    #[serde(default)]
    name: String,
    #[serde(default)]
    display_name: String,
    path: String,
    trigger: LaunchTrigger,
    close_on_unload: bool,
    #[serde(default = "default_true")]
    show_in_quick_access: bool,
}

// Legacy config for reading old formats
#[derive(Deserialize)]
struct LegacyConfig {
    programs_to_launch: Vec<LegacyProgramToLaunch>,
    programs_to_kill: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Default)]
struct Config {
    programs_to_launch: Vec<ProgramToLaunch>,
    programs_to_kill: Vec<String>,
}

// Structure to hold pending updates
#[derive(Clone)]
struct PendingUpdate {
    name: String,
    action: UpdateAction,
}

#[derive(Clone)]
enum UpdateAction {
    Remove,
    UpdateDisplayName(String),
    ToggleQuickAccess(bool),
}

lazy_static! {
    static ref CONFIG: Mutex<Config> = Mutex::new(Config::default());
    static ref SYSTEM_INFO: Mutex<System> = Mutex::new(System::new_all());
    static ref ICON_CACHE: Mutex<HashMap<String, PathBuf>> = Mutex::new(HashMap::new());
    static ref LAUNCH_INPUT: Mutex<String> = Mutex::new(String::with_capacity(260));
    static ref KILL_INPUT: Mutex<String> = Mutex::new(String::with_capacity(64));
    static ref PENDING_LAUNCH_CONFIRMATION: Mutex<Option<String>> = Mutex::new(None);
}

// --- Helper Functions ---

// Convert legacy program to new format
impl From<LegacyProgramToLaunch> for ProgramToLaunch {
    fn from(legacy: LegacyProgramToLaunch) -> Self {
        let mut new_prog = ProgramToLaunch {
            name: String::new(), // Will be set properly below
            display_name: legacy.display_name,
            path: legacy.path.clone(),
            trigger: legacy.trigger,
            close_on_unload: legacy.close_on_unload,
            show_in_quick_access: legacy.show_in_quick_access,
        };

        // Fix the name field - remove .exe and sanitize
        let clean_name = if legacy.name.ends_with(".exe") || legacy.name.ends_with(".com") || legacy.name.ends_with(".bat") {
            // Remove extension from name
            let without_ext = legacy.name.rsplit_once('.').map_or(legacy.name.as_str(), |(name, _)| name);
            sanitize_identifier(without_ext)
        } else {
            sanitize_identifier(&legacy.name)
        };
        
        new_prog.name = clean_name;

        // Set display name if empty
        if new_prog.display_name.is_empty() {
            if let Some(base_name) = get_program_name_from_command(&new_prog.path) {
                new_prog.display_name = base_name;
            } else {
                new_prog.display_name = new_prog.name.clone();
            }
        }

        new_prog
    }
}

// Convert legacy config to new format
impl From<LegacyConfig> for Config {
    fn from(legacy: LegacyConfig) -> Self {
        let mut new_config = Config {
            programs_to_launch: Vec::new(),
            programs_to_kill: legacy.programs_to_kill,
        };

        let mut used_names = HashSet::new();
        
        for legacy_prog in legacy.programs_to_launch {
            let mut new_prog = ProgramToLaunch::from(legacy_prog);
            
            // Ensure name uniqueness
            let base_name = new_prog.name.clone();
            let mut final_name = base_name.clone();
            let mut suffix = 2;
            
            while used_names.contains(&final_name) {
                final_name = format!("{}_{}", base_name, suffix);
                suffix += 1;
            }
            
            new_prog.name = final_name;
            used_names.insert(new_prog.name.clone());
            
            new_config.programs_to_launch.push(new_prog);
        }

        new_config
    }
}
fn sanitize_identifier(text: &str) -> String {
    text.replace(' ', "_")
}

fn get_executable_and_args_from_command(
    command_str: &str,
) -> Option<(String, Vec<String>)> {
    let command_lower = command_str.to_lowercase();
    
    let exe_end_index = command_lower.rfind(".exe").map(|i| i + 4)
        .or_else(|| command_lower.rfind(".com").map(|i| i + 4))
        .or_else(|| command_lower.rfind(".bat").map(|i| i + 4));

    let (exe_path_str, args_str) = match exe_end_index {
        Some(index) if Path::new(&command_str[..index]).is_file() => {
            (&command_str[..index], &command_str[index..])
        }
        _ => {
            let parts = shell_words::split(command_str).ok()?;
            if parts.is_empty() { return None; }
            let (exe, args_vec) = parts.split_first().unwrap();
            let args = args_vec.join(" ");
            return Some((exe.to_string(), shell_words::split(&args).ok()?));
        }
    };

    let exe_path = exe_path_str.trim().to_string();
    let args = shell_words::split(args_str.trim()).unwrap_or_default();

    Some((exe_path, args))
}

fn get_program_name_from_command(command_str: &str) -> Option<String> {
    get_executable_and_args_from_command(command_str)
        .and_then(|(exe_path, _)| Path::new(&exe_path).file_name()?.to_str().map(String::from))
}


fn build_command(path: &str) -> io::Result<Command> {
    match get_executable_and_args_from_command(path) {
        Some((exe, args)) => {
            let mut command = Command::new(exe);
            command.args(args);
            Ok(command)
        }
        None => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Empty or invalid command path",
        )),
    }
}

fn is_process_running(process_name: &str) -> bool {
    let mut sys = SYSTEM_INFO.lock().unwrap();
    sys.refresh_processes();
    sys.processes()
        .values()
        .any(|p| p.name().eq_ignore_ascii_case(process_name))
}

fn force_launch_process(path: &str) {
    log::log(
        LogLevel::Info,
        "SYSTEM",
        &format!("Attempting to launch: {}", path),
    );

    let mut command = match build_command(path) {
        Ok(cmd) => cmd,
        Err(e) => {
            log::log(
                LogLevel::Critical,
                "SYSTEM",
                &format!("Failed to parse command: {}", e),
            );
            return;
        }
    };

    if let Some((exe_path, _)) = get_executable_and_args_from_command(path) {
        if let Some(parent_dir) = Path::new(&exe_path).parent() {
            command.current_dir(parent_dir);
        }
    }

    if let Err(e) = command.spawn() {
        log::log(
            LogLevel::Critical,
            "SYSTEM",
            &format!("Failed to launch process: {}", e),
        );
    }
}
fn launch_process(path: &str) {
    if let Some(filename) = get_program_name_from_command(path) {
        if is_process_running(&filename) {
            *PENDING_LAUNCH_CONFIRMATION.lock().unwrap() = Some(path.to_string());
        } else {
            force_launch_process(path);
        }
    } else {
        force_launch_process(path);
    }
}
fn launch_process_by_name(name: &str) {
    let config = CONFIG.lock().unwrap();
    if let Some(program) = config.programs_to_launch.iter().find(|p| p.name == name) {
        launch_process(&program.path);
    } else {
        log::log(
            LogLevel::Critical,
            "SYSTEM",
            &format!("Program with name '{}' not found.", name),
        );
    }
}

// --- Quick Access & Icon Management ---
fn create_placeholder_icon(path: &Path) {
    image::RgbaImage::new(32, 32)
        .save_with_format(path, image::ImageFormat::Png)
        .ok();
}
fn extract_and_save_icon(exe_path: &str, save_path: &Path) -> Result<(), String> {
    let base64_str = get_icon_base64_by_path(exe_path).map_err(|e| e.to_string())?;
    let image_data = BASE64.decode(base64_str).map_err(|e| e.to_string())?;
    let img = image::load(Cursor::new(&image_data), image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    img.save(save_path).map_err(|e| e.to_string())
}
fn setup_quick_access_for_program(program: &ProgramToLaunch) {
    let addon_dir = match get_addon_dir(env!("CARGO_PKG_NAME")) {
        Some(dir) => dir,
        None => return,
    };
    let icons_dir = addon_dir.join("icons");
    fs::create_dir_all(&icons_dir).ok();

    let icon_path = icons_dir.join(format!("{}.png", program.name));
    if !icon_path.exists() {
        if let Some((exe_path, _)) = get_executable_and_args_from_command(&program.path) {
            if let Err(e) = extract_and_save_icon(&exe_path, &icon_path) {
                log::log(
                    LogLevel::Warning,
                    "SYSTEM",
                    &format!(
                        "Could not extract icon for {}: {}. Using placeholder.",
                        program.display_name, e
                    ),
                );
                create_placeholder_icon(&icon_path);
            }
        } else {
            create_placeholder_icon(&icon_path);
        }
    }

    let qa_item_id = format!("QA_ITEM_{}", program.name);
    let qa_tex_id = format!("QA_TEX_{}", program.name);

    get_texture_or_create_from_file(&qa_tex_id, &icon_path);
    ICON_CACHE
        .lock()
        .unwrap()
        .insert(program.name.clone(), icon_path);

    if program.show_in_quick_access {
        add_quick_access(
            &qa_item_id,
            &qa_tex_id,
            &qa_tex_id,
            &format!("LAUNCH_{}", program.name),
            &program.display_name,
        ).revert_on_unload();
    }
}
fn teardown_quick_access_for_program(program: &ProgramToLaunch) {
    let qa_item_id = format!("QA_ITEM_{}", program.name);
    remove_quick_access(&qa_item_id);

    if let Some(path) = ICON_CACHE.lock().unwrap().remove(&program.name) {
        fs::remove_file(path).ok();
    }
}

// --- Core Logic ---
extern "C-unwind" fn keybind_callback(identifier: *const c_char, is_release: bool) {
    if is_release || identifier.is_null() {
        return;
    }
    let result = panic::catch_unwind(|| {
        let identifier_cstr = unsafe { CStr::from_ptr(identifier) };
        if let Ok(id_str) = identifier_cstr.to_str() {
            if let Some(name) = id_str.strip_prefix("LAUNCH_") {
                launch_process_by_name(name);
            }
        }
    });
    if result.is_err() {
        log::log(
            LogLevel::Critical,
            "SYSTEM",
            "Panic caught in keybind handler!",
        );
    }
}
fn get_config_path() -> PathBuf {
    get_addon_dir(env!("CARGO_PKG_NAME"))
        .expect("Addon directory should exist")
        .join("settings.ron")
}
fn load_config_from_file() {
    let path = get_config_path();
    
    let loaded_config = match fs::read_to_string(&path) {
        Ok(content) => {
            log::log(LogLevel::Info, "SYSTEM", "Loading configuration file...");
            
            // First try to load as new format
            match ron::from_str::<Config>(&content) {
                Ok(config) => {
                    log::log(LogLevel::Info, "SYSTEM", "Configuration loaded successfully (new format)");
                    config
                }
                Err(_) => {
                    // Try legacy format
                    log::log(LogLevel::Info, "SYSTEM", "Trying legacy configuration format...");
                    match ron::from_str::<LegacyConfig>(&content) {
                        Ok(legacy_config) => {
                            log::log(LogLevel::Info, "SYSTEM", "Legacy configuration loaded, converting to new format");
                            let new_config = Config::from(legacy_config);
                            
                            // Save the converted config immediately
                            let config_to_save = new_config.clone();
                            *CONFIG.lock().unwrap() = new_config.clone();
                            drop(CONFIG.lock().unwrap()); // Make sure we release the lock
                            
                            // Save in new format
                            let save_path = get_config_path();
                            if let Some(parent) = save_path.parent() {
                                fs::create_dir_all(parent).ok();
                            }
                            if let Ok(serialized) = ron::ser::to_string_pretty(&config_to_save, ron::ser::PrettyConfig::default()) {
                                fs::write(save_path, serialized).ok();
                            }
                            
                            new_config
                        }
                        Err(e) => {
                            log::log(
                                LogLevel::Warning,
                                "SYSTEM",
                                &format!("Failed to parse config file as legacy format: {}. Using defaults.", e),
                            );
                            
                            // Backup the corrupted config
                            let backup_path = path.with_extension("ron.backup");
                            if fs::copy(&path, &backup_path).is_ok() {
                                log::log(LogLevel::Info, "SYSTEM", "Backed up corrupted config to settings.ron.backup");
                            }
                            
                            Config::default()
                        }
                    }
                }
            }
        }
        Err(_) => {
            log::log(LogLevel::Info, "SYSTEM", "No configuration file found, using defaults");
            Config::default()
        }
    };
    
    *CONFIG.lock().unwrap() = loaded_config;
}

fn save_config_to_file() {
    let path = get_config_path();
    if let Some(parent) = path.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            log::log(
                LogLevel::Critical,
                "SYSTEM",
                &format!("Failed to create config directory: {}", e),
            );
            return;
        }
    }
    
    let config = CONFIG.lock().unwrap();
    match ron::ser::to_string_pretty(&*config, ron::ser::PrettyConfig::default()) {
        Ok(serialized) => {
            if let Err(e) = fs::write(&path, serialized) {
                log::log(
                    LogLevel::Critical,
                    "SYSTEM",
                    &format!("Failed to write config file: {}", e),
                );
            } else {
                log::log(LogLevel::Info, "SYSTEM", "Configuration saved successfully");
            }
        }
        Err(e) => {
            log::log(
                LogLevel::Critical,
                "SYSTEM",
                &format!("Failed to serialize config: {}", e),
            );
        }
    }
}

fn validate_and_cleanup_config() {
    let mut config = CONFIG.lock().unwrap();
    let mut needs_save = false;
    let mut used_names = HashSet::new();

    log::log(LogLevel::Info, "SYSTEM", "Validating configuration...");

    // Clean up and validate programs
    config.programs_to_launch.retain_mut(|prog| {
        // Validate path exists (basic check)
        if let Some((exe_path, _)) = get_executable_and_args_from_command(&prog.path) {
            if !Path::new(&exe_path).exists() {
                log::log(
                    LogLevel::Warning,
                    "SYSTEM",
                    &format!("Removing program with non-existent path: {}", prog.path),
                );
                return false;
            }
        }

        // Ensure display_name is set
        if prog.display_name.is_empty() {
            if let Some(base_name) = get_program_name_from_command(&prog.path) {
                prog.display_name = base_name;
                needs_save = true;
            } else {
                prog.display_name = prog.name.clone();
                needs_save = true;
            }
        }

        // Ensure name is properly sanitized and unique
        if prog.name.is_empty() {
            if let Some(base_name) = get_program_name_from_command(&prog.path) {
                prog.name = sanitize_identifier(&base_name);
                needs_save = true;
            } else {
                prog.name = sanitize_identifier(&prog.display_name);
                needs_save = true;
            }
        }

        // Ensure uniqueness
        let base_name = prog.name.clone();
        let mut final_name = base_name.clone();
        let mut suffix = 2;
        
        while used_names.contains(&final_name) {
            final_name = format!("{}_{}", base_name, suffix);
            suffix += 1;
        }
        
        if final_name != prog.name {
            prog.name = final_name;
            needs_save = true;
        }
        
        used_names.insert(prog.name.clone());
        true
    });

    if needs_save {
        drop(config); // Release lock before saving
        log::log(LogLevel::Info, "SYSTEM", "Configuration updated, saving...");
        save_config_to_file();
    } else {
        log::log(LogLevel::Info, "SYSTEM", "Configuration is valid");
    }
}

fn load() {
    // Load config with backwards compatibility
    load_config_from_file();
    
    // Validate and cleanup
    validate_and_cleanup_config();

    log::log(
        LogLevel::Info,
        "SYSTEM",
        "Loading Assisted Deployment and Departure...",
    );

    // Clear any existing quick access items first
    let config = CONFIG.lock().unwrap().clone();
    for program in &config.programs_to_launch {
        remove_quick_access(&format!("QA_ITEM_{}", program.name));
    }
    
    // Setup programs
    for program in &config.programs_to_launch {
        log::log(
            LogLevel::Info,
            "SYSTEM",
            &format!("Setting up program: {} ({})", program.display_name, program.name),
        );
        
        register_keybind_with_string(
            format!("LAUNCH_{}", program.name), 
            keybind_callback, 
            ""
        ).revert_on_unload();
        
        setup_quick_access_for_program(&program);
        
        if program.trigger == LaunchTrigger::OnAddonLoad {
            launch_process(&program.path);
        }
    }
    
    register_render(RenderType::OptionsRender, render!(render_options)).revert_on_unload();
    register_render(RenderType::Render, render!(render_popup)).revert_on_unload();
}

fn unload() {
    save_config_to_file();

    let kill_list = {
        let config = CONFIG.lock().unwrap();
        let mut list = config.programs_to_kill.clone();
        for program in &config.programs_to_launch {
            if program.close_on_unload {
                if let Some(filename) = get_program_name_from_command(&program.path) {
                    if !list.contains(&filename) {
                        list.push(filename);
                    }
                }
            }
        }
        list
    };

    if !kill_list.is_empty() {
        cleanup_processes(&kill_list);
    }
    log::log(LogLevel::Info, "SYSTEM", "Unloaded.");
}
fn cleanup_processes(targets: &[String]) {
    let safe_targets: Vec<_> = targets
        .iter()
        .filter(|n| !n.eq_ignore_ascii_case("Gw2-64.exe"))
        .collect();
    if safe_targets.is_empty() {
        return;
    }

    log::log(
        LogLevel::Info,
        "SYSTEM",
        &format!("Closing processes: {:?}", safe_targets),
    );
    let mut sys = System::new_all();
    sys.refresh_processes();
    for target in safe_targets {
        for p in sys
            .processes()
            .values()
            .filter(|p| p.name().eq_ignore_ascii_case(target))
        {
            log::log(
                LogLevel::Info,
                "SYSTEM",
                &format!("Killing: {} (PID: {})", p.name(), p.pid()),
            );
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
        let filename = get_program_name_from_command(&path).unwrap_or_else(|| "program".to_string());
        let mut open = true;
        Window::new(&format!("'{}' Already Running", filename))
            .opened(&mut open)
            .always_auto_resize(true)
            .collapsible(false)
            .focus_on_appearing(true)
            .build(ui, || {
                ui.text("This program is already running.");
                ui.text("Do you want to open another instance?");
                ui.separator();
                if ui.button("Yes") {
                    force_launch_process(&path);
                    close_popup = true;
                }
                ui.same_line();
                if ui.button("No") {
                    close_popup = true;
                }
            });
        if !open {
            close_popup = true;
        }
    }
    if close_popup {
        *pending_launch = None;
    }
}

fn render_options(ui: &Ui) {
    ui.text("Manage external programs to launch/kill.");
    ui.separator();
    
    // Handle Programs to Launch section
    render_programs_to_launch_section(ui);
    
    // Handle Programs to Kill section
    render_programs_to_kill_section(ui);
}

fn render_programs_to_launch_section(ui: &Ui) {
    if !ui.collapsing_header("Programs to Launch", TreeNodeFlags::DEFAULT_OPEN) {
        return;
    }
    
    let mut config_changed = false;
    let mut pending_updates: Vec<PendingUpdate> = Vec::new();
    let mut new_program_to_add: Option<ProgramToLaunch> = None;
    
    // First pass: collect UI changes without holding lock for too long
    {
        let mut config = CONFIG.lock().unwrap();
        
        for prog in config.programs_to_launch.iter_mut() {
            ui.text(&prog.path);
            ui.same_line();
            if ui.small_button(&format!("-##launch{}", prog.name)) {
                pending_updates.push(PendingUpdate {
                    name: prog.name.clone(),
                    action: UpdateAction::Remove,
                });
                config_changed = true;
                continue; // Skip other UI elements for items being removed
            }

            let mut display_name = prog.display_name.clone();
            ui.set_next_item_width(200.0);
            if InputText::new(ui, &format!("Display Name##{}", prog.name), &mut display_name).build() {
                if display_name != prog.display_name && !display_name.trim().is_empty() {
                    pending_updates.push(PendingUpdate {
                        name: prog.name.clone(),
                        action: UpdateAction::UpdateDisplayName(display_name),
                    });
                    config_changed = true;
                }
            }

            let mut show_qa = prog.show_in_quick_access;
            if ui.checkbox(&format!("Show in Quick Access##{}", prog.name), &mut show_qa) {
                if show_qa != prog.show_in_quick_access {
                    pending_updates.push(PendingUpdate {
                        name: prog.name.clone(),
                        action: UpdateAction::ToggleQuickAccess(show_qa),
                    });
                    config_changed = true;
                }
            }
            ui.same_line();
            if ui.checkbox(&format!("Close on unload##{}", prog.name), &mut prog.close_on_unload) {
                config_changed = true;
            }

            if ui.radio_button_bool(
                &format!("On Addon Start##{}", prog.name),
                prog.trigger == LaunchTrigger::OnAddonLoad,
            ) { 
                prog.trigger = LaunchTrigger::OnAddonLoad; 
                config_changed = true; 
            }
            ui.same_line();
            if ui.radio_button_bool(
                &format!("On Keybind##{}", prog.name),
                prog.trigger == LaunchTrigger::OnKeybind,
            ) { 
                prog.trigger = LaunchTrigger::OnKeybind; 
                config_changed = true; 
            }
            
            if prog.trigger == LaunchTrigger::OnKeybind {
                ui.text_colored(
                    [0.6, 0.6, 0.6, 1.0],
                    format!("Keybind ID: LAUNCH_{}", prog.name),
                );
            }
            ui.separator();
        }
        
        // Handle new program addition UI
        ui.text("Add new program:");
        let mut launch_input = LAUNCH_INPUT.lock().unwrap();
        ui.group(|| {
            ui.set_next_item_width(300.0);
            InputText::new(ui, "##add_launch", &mut *launch_input).build();
            ui.same_line();
            if ui.button("Browse...") {
                if let Some(path) = FileDialog::new()
                    .add_filter("Executable", &["exe"])
                    .pick_file()
                {
                    *launch_input = path.to_string_lossy().to_string();
                }
            }
            ui.same_line();
            if ui.button("+##add_launch_btn") && !launch_input.is_empty() {
                let path = launch_input.clone();
                if let Some(base_name) = get_program_name_from_command(&path) {
                    let sanitized_base_name = sanitize_identifier(&base_name);
                    let mut final_name = sanitized_base_name.clone();
                    let mut suffix = 2;
                    while config.programs_to_launch.iter().any(|p| p.name == final_name) {
                        final_name = format!("{}_{}", sanitized_base_name, suffix);
                        suffix += 1;
                    }

                    new_program_to_add = Some(ProgramToLaunch {
                        name: final_name,
                        display_name: base_name,
                        path,
                        trigger: LaunchTrigger::OnAddonLoad,
                        close_on_unload: false,
                        show_in_quick_access: true,
                    });
                    config_changed = true;
                }
                launch_input.clear();
            }
        });
    } // Config lock is dropped here
    
    // Second pass: Process all updates safely
    for update in pending_updates {
        match update.action {
            UpdateAction::Remove => {
                let mut config = CONFIG.lock().unwrap();
                if let Some(pos) = config.programs_to_launch.iter().position(|p| p.name == update.name) {
                    let prog = config.programs_to_launch.remove(pos);
                    drop(config); // Release lock before UI operations
                    
                    // Clean up UI elements
                    remove_quick_access(&format!("QA_ITEM_{}", prog.name));
                    teardown_quick_access_for_program(&prog);
                }
            }
            UpdateAction::UpdateDisplayName(new_display_name) => {
                let prog_to_update = {
                    let mut config = CONFIG.lock().unwrap();
                    if let Some(prog) = config.programs_to_launch.iter_mut().find(|p| p.name == update.name) {
                        prog.display_name = new_display_name;
                        Some(prog.clone())
                    } else {
                        None
                    }
                };
                
                if let Some(prog) = prog_to_update {
                    // Update UI without holding config lock
                    remove_quick_access(&format!("QA_ITEM_{}", prog.name));
                    setup_quick_access_for_program(&prog);
                }
            }
            UpdateAction::ToggleQuickAccess(show_qa) => {
                let prog_to_update = {
                    let mut config = CONFIG.lock().unwrap();
                    if let Some(prog) = config.programs_to_launch.iter_mut().find(|p| p.name == update.name) {
                        prog.show_in_quick_access = show_qa;
                        Some(prog.clone())
                    } else {
                        None
                    }
                };
                
                if let Some(prog) = prog_to_update {
                    // Update UI without holding config lock
                    if show_qa {
                        setup_quick_access_for_program(&prog);
                    } else {
                        remove_quick_access(&format!("QA_ITEM_{}", prog.name));
                    }
                }
            }
        }
    }
    
    // Handle new program addition
    if let Some(new_prog) = new_program_to_add {
        {
            let mut config = CONFIG.lock().unwrap();
            config.programs_to_launch.push(new_prog.clone());
        } // Release lock before UI operation
        
        setup_quick_access_for_program(&new_prog);
    }
    
    if config_changed {
        save_config_to_file();
    }
}

fn render_programs_to_kill_section(ui: &Ui) {
    if !ui.collapsing_header("Programs to Kill on Unload", TreeNodeFlags::empty()) {
        return;
    }
    
    let mut changed = false;
    let mut programs_to_kill = {
        let config = CONFIG.lock().unwrap();
        config.programs_to_kill.clone()
    }; // Release lock early
    
    let mut to_remove_idx = None;
    for (i, name) in programs_to_kill.iter().enumerate() {
        ui.text(name);
        ui.same_line();
        if ui.small_button(&format!("-##kill{}", i)) {
            to_remove_idx = Some(i);
            changed = true;
        }
    }
    
    if let Some(i) = to_remove_idx {
        programs_to_kill.remove(i);
        let mut config = CONFIG.lock().unwrap();
        config.programs_to_kill = programs_to_kill.clone();
    }
    
    ui.text("Add process name to kill list:");
    let mut kill_input = KILL_INPUT.lock().unwrap();
    ui.group(|| {
        ui.set_next_item_width(300.0);
        InputText::new(ui, "##add_kill", &mut *kill_input).build();
        ui.same_line();
        if ui.button("+##add_kill_btn") && !kill_input.is_empty() {
            if !programs_to_kill.contains(&*kill_input) {
                programs_to_kill.push(kill_input.clone());
                let mut config = CONFIG.lock().unwrap();
                config.programs_to_kill = programs_to_kill;
                changed = true;
            }
            kill_input.clear();
        }
    });
    
    if changed {
        save_config_to_file();
    }
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
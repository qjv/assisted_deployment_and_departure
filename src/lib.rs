use lazy_static::lazy_static;
use nexus::{
    AddonFlags, UpdateProvider,
    gui::{RenderType, register_render, render},
    imgui::{InputText, TreeNodeFlags, Ui, Window},
    keybind::{register_keybind_with_string, unregister_keybind},
    log::{self, LogLevel},
    paths::get_addon_dir,
};
use serde::{Deserialize, Serialize};
use std::{
    ffi::{CStr, c_char},
    fs, panic,
    path::{Path, PathBuf},
    process::Command,
    sync::Mutex,
};
use sysinfo::{ProcessExt, Signal, System, SystemExt};

// --- Configuration & State Management ---

#[derive(Serialize, Deserialize, Clone, PartialEq)]
enum LaunchTrigger {
    OnAddonLoad,
    OnKeybind,
}

#[derive(Serialize, Deserialize, Clone)]
struct ProgramToLaunch {
    name: String,
    path: String,
    trigger: LaunchTrigger,
    close_on_unload: bool,
}

#[derive(Serialize, Deserialize, Clone)]
struct Config {
    programs_to_launch: Vec<ProgramToLaunch>,
    programs_to_kill: Vec<String>,
    wait_seconds: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            programs_to_launch: Vec::new(),
            // The kill list is now empty by default.
            programs_to_kill: Vec::new(),
            wait_seconds: 2,
        }
    }
}

lazy_static! {
    static ref CONFIG: Mutex<Config> = Mutex::new(Config::default());
    // Transient UI state for input boxes
    static ref LAUNCH_INPUT: Mutex<String> = Mutex::new(String::with_capacity(260));
    static ref KILL_INPUT: Mutex<String> = Mutex::new(String::with_capacity(64));
    // State for handling launch confirmation popup
    static ref PENDING_LAUNCH_CONFIRMATION: Mutex<Option<String>> = Mutex::new(None);
}

// --- Helper Functions ---

fn get_filename_from_path(path_str: &str) -> Option<String> {
    Path::new(path_str)
        .file_name()
        .and_then(|os_str| os_str.to_str())
        .map(|s| s.to_string())
}

/// Checks if a process with the given name is currently running.
fn is_process_running(process_name: &str) -> bool {
    let mut sys = System::new_all();
    sys.refresh_processes();
    let lower_process_name = process_name.to_lowercase();
    sys.processes()
        .values()
        .any(|p| p.name().to_lowercase() == lower_process_name)
}

/// Launches a process directly without any checks, setting its working directory.
fn force_launch_process(path: &str) {
    log::log(
        LogLevel::Info,
        "SYSTEM",
        &format!("Attempting to launch: {}", path),
    );

    let mut command = Command::new(path);

    // Set the working directory to the parent folder of the executable
    if let Some(parent_dir) = Path::new(path).parent() {
        command.current_dir(parent_dir);
        log::log(
            LogLevel::Info,
            "SYSTEM",
            &format!("Setting working directory to: {:?}", parent_dir),
        );
    }

    if let Err(e) = command.spawn() {
        log::log(
            LogLevel::Critical,
            "SYSTEM",
            &format!("Failed to launch process '{}': {}", path, e),
        );
    }
}

/// Checks if a process is running and launches it, asking for confirmation if needed.
fn launch_process(path: &str) {
    if let Some(filename) = get_filename_from_path(path) {
        if is_process_running(&filename) {
            // Set state to trigger UI popup
            let mut pending = PENDING_LAUNCH_CONFIRMATION.lock().unwrap();
            *pending = Some(path.to_string());
        } else {
            force_launch_process(path);
        }
    } else {
        // If we can't get a filename, just try to launch it.
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
            &format!("Program with name '{}' not found for keybind.", name),
        );
    }
}

// --- Keybind Handler ---

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

// --- Configuration Persistence ---

fn get_config_path() -> PathBuf {
    get_addon_dir(env!("CARGO_PKG_NAME"))
        .expect("Addon directory should exist")
        .join("settings.ron")
}

fn load_config_from_file() {
    let path = get_config_path();
    let loaded_config = fs::read_to_string(&path)
        .ok()
        .and_then(|content| ron::from_str(&content).ok())
        .unwrap_or_default();
    *CONFIG.lock().unwrap() = loaded_config;
}

fn save_config_to_file() {
    let path = get_config_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let config = CONFIG.lock().unwrap();
    if let Ok(serialized) = ron::to_string(&*config) {
        let _ = fs::write(path, serialized);
    }
}

// --- Core Logic & Lifecycle ---

fn load() {
    load_config_from_file();

    let config = CONFIG.lock().unwrap();
    log::log(
        LogLevel::Info,
        "SYSTEM",
        "Loading Assisted Deployment and Departure...",
    );

    // Initial setup on load
    for program in config.programs_to_launch.iter() {
        match program.trigger {
            LaunchTrigger::OnAddonLoad => {
                launch_process(&program.path);
            }
            LaunchTrigger::OnKeybind => {
                let identifier = format!("LAUNCH_{}", program.name);
                register_keybind_with_string(identifier, keybind_callback, "");
            }
        }
    }

    register_render(RenderType::OptionsRender, render!(render_options)).revert_on_unload();
}

fn unload() {
    save_config_to_file();
    let config = CONFIG.lock().unwrap().clone();

    // Keybinds are no longer unregistered on unload to allow them to persist.
    // They are now only unregistered in the UI when the user explicitly removes
    // a program or changes its trigger.

    // Collect all processes that need to be terminated.
    let mut kill_list = config.programs_to_kill;
    for program in config.programs_to_launch.iter() {
        if program.close_on_unload {
            if let Some(filename) = get_filename_from_path(&program.path) {
                if !kill_list.contains(&filename) {
                    kill_list.push(filename);
                }
            }
        }
    }

    if !kill_list.is_empty() {
        if config.wait_seconds > 0 {
            log::log(
                LogLevel::Info,
                "SYSTEM",
                &format!(
                    "Waiting {} seconds before process cleanup...",
                    config.wait_seconds
                ),
            );
            std::thread::sleep(std::time::Duration::from_secs(config.wait_seconds));
        }
        cleanup_processes(&kill_list);
    }
    log::log(
        LogLevel::Info,
        "SYSTEM",
        "Unloaded Assisted Deployment and Departure.",
    );
}

fn cleanup_processes(targets: &[String]) {
    log::log(
        LogLevel::Info,
        "SYSTEM",
        &format!("Starting process cleanup for targets: {:?}", targets),
    );
    let mut sys = System::new_all();
    sys.refresh_processes();

    let current_pid_res = sysinfo::get_current_pid();

    for target_name in targets {
        // Find all running processes that match the target name.
        // This check ensures we only try to kill processes that are actually alive.
        let processes_to_kill: Vec<_> = sys
            .processes()
            .values()
            .filter(|p| p.name().eq_ignore_ascii_case(target_name))
            .collect();

        for process in processes_to_kill {
            // Ensure we don't kill the host process (the game) until the very end.
            if let Ok(current_pid) = current_pid_res {
                if process.pid() == current_pid {
                    continue; // Skip killing self for now.
                }
            }
            log::log(
                LogLevel::Info,
                "SYSTEM",
                &format!(
                    "Killing process: {} (PID: {})",
                    process.name(),
                    process.pid()
                ),
            );
            process.kill_with(Signal::Kill);
        }
    }

    // Finally, if Gw2-64.exe was in the kill list, terminate it now.
    if let Ok(current_pid) = current_pid_res {
        if targets.iter().any(|t| t.eq_ignore_ascii_case("Gw2-64.exe")) {
            if let Some(proc) = sys.process(current_pid) {
                log::log(
                    LogLevel::Info,
                    "SYSTEM",
                    &format!(
                        "Killing own process last: {} (PID: {})",
                        proc.name(),
                        current_pid
                    ),
                );
                proc.kill_with(Signal::Kill);
            }
        }
    }
}

// --- UI Rendering ---

fn render_options(ui: &Ui) {
    // --- Popup Window Handling ---
    let mut pending_launch = PENDING_LAUNCH_CONFIRMATION.lock().unwrap();
    let mut close_popup = false;

    if let Some(path_to_launch) = pending_launch.as_ref() {
        let filename =
            get_filename_from_path(path_to_launch).unwrap_or_else(|| "program".to_string());
        let mut window_is_open = true;

        Window::new(&format!("'{}' Already Running", filename))
            .opened(&mut window_is_open)
            .always_auto_resize(true)
            .collapsible(false)
            .build(ui, || {
                ui.text("This program is already running.");
                ui.text("Do you want to open another instance?");
                ui.separator();

                if ui.button("Yes") {
                    force_launch_process(path_to_launch);
                    close_popup = true;
                }
                ui.same_line();
                if ui.button("No") {
                    close_popup = true;
                }
            });

        if !window_is_open {
            close_popup = true;
        }
    }

    if close_popup {
        *pending_launch = None;
    }
    drop(pending_launch);

    // --- Main UI ---
    let mut config = CONFIG.lock().unwrap();
    let mut config_changed = false;

    ui.text("Manage external programs to launch with the game or kill on exit.");
    ui.separator();

    // --- Section: Programs to Launch ---
    if ui.collapsing_header("Programs to Launch", TreeNodeFlags::DEFAULT_OPEN) {
        let mut to_remove = None;
        for (i, prog) in config.programs_to_launch.iter_mut().enumerate() {
            ui.text(&prog.path);
            ui.same_line();
            if ui.small_button(&format!("-##launch{}", i)) {
                if prog.trigger == LaunchTrigger::OnKeybind {
                    unregister_keybind(&format!("LAUNCH_{}", prog.name));
                }
                to_remove = Some(i);
                config_changed = true;
            }

            // --- Corrected Radio Button Logic ---
            let original_trigger = prog.trigger.clone();

            // Create unique labels for the radio buttons using the item index `i`.
            if ui.radio_button_bool(
                &format!("Load on Addon Start##{}", i),
                prog.trigger == LaunchTrigger::OnAddonLoad,
            ) {
                prog.trigger = LaunchTrigger::OnAddonLoad;
            }
            ui.same_line();
            if ui.radio_button_bool(
                &format!("Load on Keybind##{}", i),
                prog.trigger == LaunchTrigger::OnKeybind,
            ) {
                prog.trigger = LaunchTrigger::OnKeybind;
            }

            // If the state changed, handle the side-effects
            if original_trigger != prog.trigger {
                config_changed = true;
                match original_trigger {
                    LaunchTrigger::OnKeybind => {
                        // It was a keybind, now it's not. Unregister it.
                        unregister_keybind(&format!("LAUNCH_{}", prog.name));
                    }
                    _ => {} // No action needed if it was OnAddonLoad
                }
                match prog.trigger {
                    LaunchTrigger::OnKeybind => {
                        // It's a keybind now. Register it.
                        let identifier = format!("LAUNCH_{}", prog.name);
                        register_keybind_with_string(identifier, keybind_callback, "");
                    }
                    _ => {} // No action needed if it's now OnAddonLoad
                }
            }
            // --- End Corrected Logic ---

            if prog.trigger == LaunchTrigger::OnKeybind {
                ui.text_colored(
                    [0.6, 0.6, 0.6, 1.0],
                    format!("Keybind Identifier: LAUNCH_{}", prog.name),
                );
            }

            if ui.checkbox(
                &format!("Close when unloading##{}", i),
                &mut prog.close_on_unload,
            ) {
                config_changed = true;
            }
            ui.separator();
        }

        if let Some(i) = to_remove {
            config.programs_to_launch.remove(i);
        }

        ui.text("Add new program (full path):");
        let mut launch_input = LAUNCH_INPUT.lock().unwrap();
        ui.group(|| {
            ui.set_next_item_width(300.0);
            InputText::new(ui, "##add_launch", &mut *launch_input).build();
            ui.same_line();
            if ui.button("+##add_launch_btn") {
                if !launch_input.is_empty() {
                    let path = launch_input.clone();
                    if let Some(name) = get_filename_from_path(&path) {
                        if !config.programs_to_launch.iter().any(|p| p.name == name) {
                            config.programs_to_launch.push(ProgramToLaunch {
                                name,
                                path,
                                trigger: LaunchTrigger::OnAddonLoad,
                                close_on_unload: false,
                            });
                            config_changed = true;
                        } else {
                            log::log(
                                LogLevel::Warning,
                                "SYSTEM",
                                &format!("A program named '{}' already exists.", name),
                            );
                        }
                    }
                    launch_input.clear();
                }
            }
        });
    }

    // --- Section: Programs to Kill ---
    if ui.collapsing_header("Programs to Kill on Unload", TreeNodeFlags::empty()) {
        let mut to_remove = None;
        for (i, name) in config.programs_to_kill.iter().enumerate() {
            ui.text(name);
            ui.same_line();
            if ui.small_button(&format!("-##kill{}", i)) {
                to_remove = Some(i);
                config_changed = true;
            }
        }

        if let Some(i) = to_remove {
            config.programs_to_kill.remove(i);
        }

        ui.text("Add process name to kill list:");
        let mut kill_input = KILL_INPUT.lock().unwrap();
        ui.group(|| {
            ui.set_next_item_width(300.0);
            InputText::new(ui, "##add_kill", &mut *kill_input).build();
            ui.same_line();
            if ui.button("+##add_kill_btn") {
                if !kill_input.is_empty() {
                    if !config.programs_to_kill.contains(&*kill_input) {
                        config.programs_to_kill.push(kill_input.clone());
                        config_changed = true;
                    }
                    kill_input.clear();
                }
            }
        });
    }

    ui.separator();

    // --- Section: Global Settings ---
    ui.text("Wait time before cleanup (seconds):");
    ui.same_line();
    ui.set_next_item_width(40.0);
    let mut wait_str = config.wait_seconds.to_string();
    if InputText::new(ui, "##wait_time", &mut wait_str)
        .chars_decimal(true)
        .build()
    {
        if let Ok(val) = wait_str.parse::<u64>() {
            if config.wait_seconds != val {
                config.wait_seconds = val;
                config_changed = true;
            }
        }
    }

    if config_changed {
        drop(config);
        save_config_to_file();
    }
}

nexus::export! {
    name: "Assisted Deployment and Departure",
    signature: -128175, // Incremented signature for the new version
    flags: AddonFlags::None,
    load,
    unload,
    provider: UpdateProvider::GitHub,
    update_link: "https://github.com/qjv/assisted_deployment_and_departure"
}

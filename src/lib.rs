use nexus::{
    gui::{register_render, render, RenderType},
    imgui::{Ui, InputText},
    paths::get_addon_dir,
    AddonFlags, UpdateProvider,
};
use lazy_static::lazy_static;
use std::sync::Mutex;
use sysinfo::{System, SystemExt, Signal, ProcessExt};
use serde::{Serialize, Deserialize};
use std::{fs, path::PathBuf};

lazy_static! {
    static ref TO_KILL: Mutex<Vec<String>> = Mutex::new(Vec::new());
    static ref INPUT: Mutex<String> = Mutex::new(String::with_capacity(64));
    static ref KILL_ON_UNLOAD: Mutex<bool> = Mutex::new(false);
}

#[derive(Serialize, Deserialize, Default)]
struct Config {
    to_kill: Vec<String>,
    kill_on_unload: bool,
}

fn get_config_path() -> PathBuf {
    get_addon_dir(env!("CARGO_PKG_NAME"))
        .expect("Addon dir should exist")
        .join("settings.ron")
}

fn load_config() -> Config {
    let path = get_config_path();
    if let Ok(content) = fs::read_to_string(&path) {
        ron::from_str(&content).unwrap_or_default()
    } else {
        Config::default()
    }
}

fn save_config(config: &Config) {
    let path = get_config_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(serialized) = ron::to_string(config) {
        let _ = fs::write(path, serialized);
    }
}

fn load() {
    let config = load_config();
    let mut to_kill = TO_KILL.lock().unwrap();
    *to_kill = config.to_kill;
    *KILL_ON_UNLOAD.lock().unwrap() = config.kill_on_unload;

    // Only visible in the options tab
    register_render(RenderType::OptionsRender, render!(render_options)).revert_on_unload();
}

fn save_current_config() {
    let to_kill = TO_KILL.lock().unwrap();
    let kill_on_unload = *KILL_ON_UNLOAD.lock().unwrap();
    save_config(&Config {
        to_kill: to_kill.clone(),
        kill_on_unload,
    });
}

fn unload() {
    save_current_config();
    if *KILL_ON_UNLOAD.lock().unwrap() {
        cleanup_processes(); // Synchronous, no thread
    }
    TO_KILL.lock().unwrap().clear();
}

fn cleanup_processes() {
    log::info!("Starting cleanup_processes");
    let mut sys = System::new_all();
    sys.refresh_processes();

    let targets = TO_KILL.lock().unwrap().clone();
    log::info!("Targets: {:?}", targets);

    let current_pid = sysinfo::get_current_pid().unwrap();
    let mut kill_self = false;

    // First, kill all matching processes except self
    for proc in sys.processes().values() {
        let name = proc.name().to_lowercase();
        let pid = proc.pid();

        if targets.iter().any(|t| name == t.to_lowercase()) {
            if pid == current_pid {
                kill_self = true;
                continue;
            }
            log::info!("Killing process: {} (PID: {})", proc.name(), pid);
            let _ = proc.kill_with(Signal::Kill);
        }
    }

    // Now, if self is in the kill list, kill self last
    if kill_self {
        if let Some(proc) = sys.process(current_pid) {
            log::info!("Killing own process last: {} (PID: {})", proc.name(), current_pid);
            let _ = proc.kill_with(Signal::Kill);
        }
    }

    log::info!("Finished cleanup_processes");
}

fn render_options(ui: &Ui) {
    ui.text("Processes to kill on unload:");

    // Step 1: Collect a snapshot for display
    let to_kill_snapshot = {
        let to_kill = TO_KILL.lock().unwrap();
        to_kill.clone()
    };

    let mut action = None; // (action_type, index)

    for (i, name) in to_kill_snapshot.iter().enumerate() {
        ui.text(format!("• {}", name));

        if i > 0 {
            ui.same_line();
            if ui.small_button(&format!("Up##{}", i)) {
                action = Some(("up", i));
            }
        }

        if i + 1 < to_kill_snapshot.len() {
            ui.same_line();
            if ui.small_button(&format!("Down##{}", i)) {
                action = Some(("down", i));
            }
        }

        ui.same_line();
        if ui.small_button(&format!("Remove##{}", i)) {
            action = Some(("remove", i));
        }
    }

    // Step 2: Apply the action after the loop
    if let Some((act, i)) = action {
        let mut to_kill = TO_KILL.lock().unwrap();
        match act {
            "up" if i > 0 => {
                to_kill.swap(i, i - 1);
            }
            "down" if i + 1 < to_kill.len() => {
                to_kill.swap(i, i + 1);
            }
            "remove" if i < to_kill.len() => {
                to_kill.remove(i);
            }
            _ => {}
        }
        save_config(&Config {
            to_kill: to_kill.clone(),
            kill_on_unload: *KILL_ON_UNLOAD.lock().unwrap(),
        });
    }

    ui.separator();

    let mut input = INPUT.lock().unwrap();
    if InputText::new(ui, "Add process name", &mut *input).enter_returns_true(true).build() {
        if !input.is_empty() {
            let mut to_kill = TO_KILL.lock().unwrap();
            to_kill.push(input.to_string());
            save_config(&Config {
                to_kill: to_kill.clone(),
                kill_on_unload: *KILL_ON_UNLOAD.lock().unwrap(),
            });
            input.clear();
        }
    }

    if ui.button("Add to list") {
        if !input.is_empty() {
            let mut to_kill = TO_KILL.lock().unwrap();
            to_kill.push(input.to_string());
            save_config(&Config {
                to_kill: to_kill.clone(),
                kill_on_unload: *KILL_ON_UNLOAD.lock().unwrap(),
            });
            input.clear();
        }
    }

    ui.separator();
    if ui.button("Kill All Processes Now") {
        std::thread::spawn(|| {
            cleanup_processes();
        });
    }

    // Add the checkbox for "Kill on unload"
    let mut kill_on_unload = *KILL_ON_UNLOAD.lock().unwrap();
    if ui.checkbox("Kill on unload (may cause stutter/crash!)", &mut kill_on_unload) {
        *KILL_ON_UNLOAD.lock().unwrap() = kill_on_unload;
        save_current_config();
    }
}

nexus::export! {
    name: "Process Retirement House and Assisted Departure",
    signature: -128174,
    flags: AddonFlags::None,
    load,
    unload,
    provider: UpdateProvider::GitHub,
    update_link: "https://github.com/qjv/assisted_departure"
}

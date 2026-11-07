use crate::discord::{SpoofMethod, SpoofableGame};
use crate::history::{load_history, record_played_batch};
use crate::i18n::{is_russian, pick};
use crate::rpc::DiscordRpcSession;
use crate::state::{
    ActiveGameInfo, SpooferConfig, SpooferLogEvent, SpooferStatus, SpooferStatusEvent,
    SpooferTickEvent, SpoofMode,
};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter};
use tokio::sync::{Mutex, Notify, RwLock};

#[cfg(windows)]
static DUMMY_EXE_BYTES: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/dummy.exe"));
#[cfg(not(windows))]
static DUMMY_EXE_BYTES: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/dummy"));

const MAX_FARM_GAMES: usize = 20;

/// Quest mode defaults and bounds (seconds). Discord quests usually ask for
/// 5–60 minutes of PC playtime, so a quarter of an hour is a safe default.
const DEFAULT_QUEST_SECS: u64 = 15 * 60;
const MIN_QUEST_SECS: u64 = 60;
const MAX_QUEST_SECS: u64 = 24 * 60 * 60;
/// How many times a single quest dummy may be respawned if it exits early.
const MAX_QUEST_RESTARTS: u32 = 5;

/// Log lines carry no timestamp: the UI stamps them with the PC's local time on arrival.
pub(crate) fn log_event(app: &AppHandle, level: &str, message: &str) {
    let _ = app.emit(
        "spoofer_log",
        SpooferLogEvent {
            level: level.to_string(),
            message: message.to_string(),
        },
    );
}

fn cleanup_temp_dir(temp_dir: &Path) {
    if temp_dir.exists() {
        let _ = fs::remove_dir_all(temp_dir);
    }
}

pub fn force_kill_pid(pid: u32) {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
        let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if !h.is_null() {
            TerminateProcess(h, 1);
            CloseHandle(h);
        }
    }
    #[cfg(unix)]
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
}

// Win32 Job Object sandbox
#[cfg(windows)]
struct JobObjectGuard {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
impl JobObjectGuard {
    fn new() -> Option<Self> {
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::System::JobObjects::{
            CreateJobObjectW, SetInformationJobObject, JobObjectExtendedLimitInformation,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };
        unsafe {
            let handle = CreateJobObjectW(std::ptr::null_mut(), std::ptr::null());
            if handle.is_null() || handle == INVALID_HANDLE_VALUE {
                return None;
            }

            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

            let res = SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const std::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );

            if res == 0 {
                windows_sys::Win32::Foundation::CloseHandle(handle);
                return None;
            }

            Some(Self { handle })
        }
    }

    fn assign_pid(&self, pid: u32) {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
        use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE};
        unsafe {
            let proc_h = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
            if !proc_h.is_null() {
                AssignProcessToJobObject(self.handle, proc_h);
                CloseHandle(proc_h);
            }
        }
    }
}

#[cfg(windows)]
impl Drop for JobObjectGuard {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.handle);
        }
    }
}

#[cfg(windows)]
unsafe impl Send for JobObjectGuard {}
#[cfg(windows)]
unsafe impl Sync for JobObjectGuard {}

/// Launches the dummy binary under the game's own relative path, so the OS
/// process table (and Discord's process scan) sees the game's executable name.
fn spawn_dummy_process(exe_path: &Path, game: &SpoofableGame) -> std::io::Result<Child> {
    let mut cmd = Command::new(exe_path);
    cmd.arg(&game.name);

    if let Some(args) = &game.arguments {
        for arg in args.split_whitespace() {
            cmd.arg(arg);
        }
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x00004000);
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                libc::prctl(1, libc::SIGKILL, 0, 0, 0);
                Ok(())
            });
        }
    }

    cmd.spawn()
}

/// Picks the fixed farm set. Games sharing an exe name can't run side by side,
/// so duplicates are dropped here instead of spilling into a second batch.
fn select_farm_games(
    app: &AppHandle,
    games: Vec<SpoofableGame>,
    farm_game_ids: &[String],
    is_quest: bool,
) -> Vec<SpoofableGame> {
    let mut exe_basenames: HashSet<String> = HashSet::new();
    let mut selected: Vec<SpoofableGame> = Vec::new();
    let label_ru = if is_quest { "Квест" } else { "Фарм" };
    let label_en = if is_quest { "Quest" } else { "Farm" };

    for id in farm_game_ids {
        if selected.len() >= MAX_FARM_GAMES {
            break;
        }
        let Some(game) = games.iter().find(|g| &g.id == id) else {
            continue;
        };

        if game.method == SpoofMethod::Process {
            if let Some(rel) = &game.rel_path {
                let exe_name = Path::new(rel)
                    .file_name()
                    .map(|s| s.to_string_lossy().to_lowercase())
                    .unwrap_or_default();

                if !exe_name.is_empty() && !exe_basenames.insert(exe_name.clone()) {
                    let message = if is_russian() {
                        format!(
                            "{}: '{}' пропущена, файл {} уже занят другой игрой.",
                            label_ru, game.name, exe_name
                        )
                    } else {
                        format!(
                            "{}: skipped '{}', {} is already used by another game.",
                            label_en, game.name, exe_name
                        )
                    };
                    log_event(app, "warn", &message);
                    continue;
                }
            }
        }

        selected.push(game.clone());
    }

    selected
}

pub async fn run_spoofer(
    app: AppHandle,
    all_games: Vec<SpoofableGame>,
    config: SpooferConfig,
    status_lock: Arc<RwLock<SpooferStatus>>,
    cancel_flag: Arc<AtomicBool>,
    pause_flag: Arc<AtomicBool>,
    resume_notify: Arc<Notify>,
    active_pids: Arc<Mutex<Vec<u32>>>,
) {
    // Quests are only credited for native game processes, so RPC/console titles
    // are left out here whatever spoof mode the dashboard is set to.
    let mode_filtered_games: Vec<SpoofableGame> = if config.quest_mode {
        all_games
            .into_iter()
            .filter(|g| g.method == SpoofMethod::Process)
            .collect()
    } else {
        match config.spoof_mode {
            SpoofMode::Hybrid => all_games,
            SpoofMode::ProcessOnly => all_games
                .into_iter()
                .filter(|g| g.method == SpoofMethod::Process)
                .collect(),
            SpoofMode::RpcOnly => all_games
                .into_iter()
                .filter(|g| g.method == SpoofMethod::Rpc)
                .collect(),
        }
    };

    let mode_filtered_games: Vec<SpoofableGame> = if config.exclude_adult {
        let before_count = mode_filtered_games.len();
        let filtered: Vec<SpoofableGame> =
            mode_filtered_games.into_iter().filter(|g| !g.adult).collect();
        let skipped = before_count - filtered.len();
        if skipped > 0 {
            let message = if is_russian() {
                format!("Пропуск {} игр 18+.", skipped)
            } else {
                format!("Skipped {} 18+ games.", skipped)
            };
            log_event(&app, "info", &message);
        }
        filtered
    } else {
        mode_filtered_games
    };

    if config.quest_mode && config.quest_game_ids.is_empty() {
        *status_lock.write().await = SpooferStatus::Error;
        let message = pick(
            "Квест: не выбрана ни одна игра.",
            "Quest: no game was selected.",
        );
        let _ = app.emit(
            "spoofer_status",
            SpooferStatusEvent {
                status: SpooferStatus::Error,
                message: Some(message.to_string()),
            },
        );
        log_event(&app, "error", message);
        return;
    }

    // A quest run is a fixed set of native games held for a precise time, which
    // is exactly what Discord checks for "play on PC" quests.
    let is_quest = config.quest_mode;
    let is_farm = !is_quest && !config.farm_game_ids.is_empty();
    let is_hold_run = is_farm || is_quest;
    let hold_game_ids: &[String] = if is_quest {
        &config.quest_game_ids
    } else {
        &config.farm_game_ids
    };

    let history = load_history();
    let mut pool: Vec<SpoofableGame> = if is_hold_run {
        select_farm_games(&app, mode_filtered_games, hold_game_ids, is_quest)
    } else if config.skip_previously_spoofed {
        let before_count = mode_filtered_games.len();
        let filtered: Vec<SpoofableGame> = mode_filtered_games
            .into_iter()
            .filter(|g| !history.contains(&g.id))
            .collect();
        let skipped = before_count - filtered.len();
        if skipped > 0 {
            let message = if is_russian() {
                format!("Пропуск {} ранее эмулированных игр из истории.", skipped)
            } else {
                format!("Skipped {} previously spoofed games from history.", skipped)
            };
            log_event(&app, "info", &message);
        }
        filtered
    } else {
        mode_filtered_games
    };

    let total_available = pool.len();
    if total_available == 0 {
        *status_lock.write().await = SpooferStatus::Finished;
        let unavailable_message = if is_quest {
            pick(
                "Квест: выбранные игры недоступны (нужны только нативные процессы).",
                "Quest: the selected games are unavailable (native processes only).",
            )
        } else if is_farm {
            pick(
                "Выбранные для фарма игры недоступны.",
                "The games selected for farming are unavailable.",
            )
        } else {
            pick(
                "Все доступные игры уже эмулированы согласно истории!",
                "All available games are already spoofed according to history!",
            )
        };
        let _ = app.emit(
            "spoofer_status",
            SpooferStatusEvent {
                status: SpooferStatus::Finished,
                message: Some(unavailable_message.to_string()),
            },
        );
        if is_hold_run {
            log_event(&app, "warn", unavailable_message);
        } else {
            log_event(
                &app,
                "success",
                pick(
                    "Все игры уже были обработаны.",
                    "All games have already been processed.",
                ),
            );
        }
        return;
    }

    let target_count = if is_hold_run {
        total_available
    } else {
        config
            .limit
            .map(|lim| lim.min(total_available))
            .unwrap_or(total_available)
    };

    // A farm or quest run is a single fixed batch, so it never rotates.
    let batch_size = if is_hold_run {
        total_available
    } else {
        config.batch_size.clamp(1, 20)
    };

    // Quest time is counted to the minute, because that is what Discord counts.
    let quest_duration = Duration::from_secs(
        config
            .quest_duration_secs
            .unwrap_or(DEFAULT_QUEST_SECS)
            .clamp(MIN_QUEST_SECS, MAX_QUEST_SECS),
    );

    // Initialize Win32 Job Object sandbox for orphan process protection
    #[cfg(windows)]
    let job_guard = JobObjectGuard::new();

    // Prepare temp directory for process spoofing
    let temp_root = std::env::temp_dir().join("mimic_spoofer");
    cleanup_temp_dir(&temp_root);

    if let Err(e) = fs::create_dir_all(&temp_root) {
        *status_lock.write().await = SpooferStatus::Error;
        let err_msg = if is_russian() {
            format!("Ошибка создания временной папки: {}", e)
        } else {
            format!("Failed to create the temporary folder: {}", e)
        };
        let _ = app.emit(
            "spoofer_status",
            SpooferStatusEvent {
                status: SpooferStatus::Error,
                message: Some(err_msg.clone()),
            },
        );
        log_event(&app, "error", &err_msg);
        return;
    }

    #[cfg(windows)]
    let base_dummy_path = temp_root.join("base_dummy.exe");
    #[cfg(not(windows))]
    let base_dummy_path = temp_root.join("base_dummy");

    if let Err(e) = fs::write(&base_dummy_path, DUMMY_EXE_BYTES) {
        *status_lock.write().await = SpooferStatus::Error;
        let err_msg = if is_russian() {
            format!("Не удалось создать базовый бинарный файл: {}", e)
        } else {
            format!("Failed to create the base binary file: {}", e)
        };
        let _ = app.emit(
            "spoofer_status",
            SpooferStatusEvent {
                status: SpooferStatus::Error,
                message: Some(err_msg.clone()),
            },
        );
        log_event(&app, "error", &err_msg);
        cleanup_temp_dir(&temp_root);
        return;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&base_dummy_path, fs::Permissions::from_mode(0o755));
    }

    *status_lock.write().await = SpooferStatus::Running;
    let _ = app.emit(
        "spoofer_status",
        SpooferStatusEvent {
            status: SpooferStatus::Running,
            message: None,
        },
    );

    let mode_str = match config.spoof_mode {
        SpoofMode::Hybrid => pick("Гибридный (Процессы + RPC)", "Hybrid (processes + RPC)"),
        SpoofMode::ProcessOnly => pick("Только процессы (.exe)", "Native processes only"),
        SpoofMode::RpcOnly => pick("Остальные (Discord IPC/RPC)", "Other (Discord IPC/RPC)"),
    };

    let start_message = if is_quest {
        let minutes = quest_duration.as_secs() / 60;
        if is_russian() {
            format!(
                "Квест запущен: {} игр на {} мин. Держите mimic и Discord открытыми — таймер идёт в реальном времени.",
                target_count, minutes
            )
        } else {
            format!(
                "Quest started: {} games for {} min. Keep mimic and Discord open — the timer runs in real time.",
                target_count, minutes
            )
        }
    } else if is_farm {
        if is_russian() {
            format!("Старт фарма: {} игр без смены.", target_count)
        } else {
            format!("Farm started: {} games with no rotation.", target_count)
        }
    } else if is_russian() {
        format!(
            "Старт mimic: режим {}, цель {} игр, батч {}, удержание {} сек",
            mode_str, target_count, batch_size, config.hold_duration_secs
        )
    } else {
        format!(
            "mimic started: mode {}, target {} games, batch {}, hold {} sec",
            mode_str, target_count, batch_size, config.hold_duration_secs
        )
    };
    log_event(&app, "info", &start_message);

    let start_time = Instant::now();
    let mut processed_count = 0usize;
    let mut succeeded_count = 0usize;
    let mut failed_count = 0usize;
    let mut batch_counter = 0usize;

    while processed_count < target_count && !pool.is_empty() {
        if cancel_flag.load(Ordering::SeqCst) {
            log_event(
                &app,
                "warn",
                pick(
                    "Процесс остановлен пользователем.",
                    "Stopped by the user.",
                ),
            );
            break;
        }

        // Smart Cooldown Manager
        if config.cooldown_enabled && batch_counter > 0 && batch_counter % config.cooldown_every_n_batches == 0 {
            let cooldown_message = if is_russian() {
                format!(
                    "Кулдаун: пауза {} сек для защиты от рейтлимитов Discord...",
                    config.cooldown_seconds
                )
            } else {
                format!(
                    "Cooldown: pausing {} sec to avoid Discord rate limits...",
                    config.cooldown_seconds
                )
            };
            log_event(&app, "info", &cooldown_message);
            let cd_start = Instant::now();
            let cd_duration = Duration::from_secs(config.cooldown_seconds);
            while cd_start.elapsed() < cd_duration {
                if cancel_flag.load(Ordering::SeqCst) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            if cancel_flag.load(Ordering::SeqCst) {
                break;
            }
            log_event(
                &app,
                "info",
                pick(
                    "Кулдаун завершен, продолжение работы.",
                    "Cooldown finished, resuming.",
                ),
            );
        }

        // Check pause
        if pause_flag.load(Ordering::SeqCst) {
            *status_lock.write().await = SpooferStatus::Paused;
            let _ = app.emit(
                "spoofer_status",
                SpooferStatusEvent {
                    status: SpooferStatus::Paused,
                    message: None,
                },
            );
            log_event(
                &app,
                "warn",
                pick("Пауза. Ожидание возобновления...", "Paused. Waiting to resume..."),
            );
            resume_notify.notified().await;
            if cancel_flag.load(Ordering::SeqCst) {
                break;
            }
            *status_lock.write().await = SpooferStatus::Running;
            let _ = app.emit(
                "spoofer_status",
                SpooferStatusEvent {
                    status: SpooferStatus::Running,
                    message: None,
                },
            );
            log_event(
                &app,
                "info",
                pick("Продолжение работы.", "Resuming."),
            );
        }

        let mut current_batch: Vec<SpoofableGame> = Vec::new();
        let mut active_exe_basenames: HashSet<String> = HashSet::new();
        let mut indices_to_remove: Vec<usize> = Vec::new();

        for (idx, game) in pool.iter().enumerate() {
            if current_batch.len() >= batch_size || (processed_count + current_batch.len()) >= target_count {
                break;
            }

            if game.method == SpoofMethod::Process {
                if let Some(rel) = &game.rel_path {
                    let exe_name = Path::new(rel)
                        .file_name()
                        .map(|s| s.to_string_lossy().to_lowercase())
                        .unwrap_or_default();

                    if !exe_name.is_empty() && active_exe_basenames.contains(&exe_name) {
                        continue;
                    }

                    active_exe_basenames.insert(exe_name);
                }
            }

            current_batch.push(game.clone());
            indices_to_remove.push(idx);
        }

        for idx in indices_to_remove.into_iter().rev() {
            pool.swap_remove(idx);
        }

        if current_batch.is_empty() {
            break;
        }

        batch_counter += 1;

        {
            let mut pids_guard = active_pids.lock().await;
            pids_guard.clear();
        }

        let mut spawned_processes: Vec<(Child, SpoofableGame, PathBuf)> = Vec::new();
        let mut active_rpc_sessions: Vec<DiscordRpcSession> = Vec::new();
        let mut active_batch_info: Vec<ActiveGameInfo> = Vec::new();

        let batch_start_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        for game in &current_batch {
            if game.method == SpoofMethod::Process && game.rel_path.is_some() {
                let rel = game.rel_path.as_ref().unwrap();
                let target_exe_path = temp_root.join(rel);

                if let Some(parent) = target_exe_path.parent() {
                    let _ = fs::create_dir_all(parent);
                }

                if !target_exe_path.exists() {
                    if fs::hard_link(&base_dummy_path, &target_exe_path).is_err() {
                        let _ = fs::copy(&base_dummy_path, &target_exe_path);
                    }
                }

                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = fs::set_permissions(&target_exe_path, fs::Permissions::from_mode(0o755));
                }

                match spawn_dummy_process(&target_exe_path, game) {
                    Ok(child) => {
                        let pid = child.id();
                        succeeded_count += 1;

                        #[cfg(windows)]
                        if let Some(guard) = &job_guard {
                            guard.assign_pid(pid);
                        }

                        {
                            let mut pids_guard = active_pids.lock().await;
                            pids_guard.push(pid);
                        }

                        active_batch_info.push(ActiveGameInfo {
                            id: game.id.clone(),
                            name: game.name.clone(),
                            executable: Some(rel.clone()),
                            pid: Some(pid),
                            icon_hash: game.icon_hash.clone(),
                            method: "process".to_string(),
                        });
                        spawned_processes.push((child, game.clone(), target_exe_path));
                    }
                    Err(e) => {
                        failed_count += 1;
                        let message = if is_russian() {
                            format!("Не удалось запустить процесс '{}': {}", game.name, e)
                        } else {
                            format!("Failed to launch process '{}': {}", game.name, e)
                        };
                        log_event(&app, "warn", &message);
                    }
                }
            } else {
                // RPC / Console / SDK game: spoof via local Discord IPC socket
                match DiscordRpcSession::connect(&game.id).await {
                    Ok(mut session) => {
                        let current_pid = std::process::id();
                        if let Err(e) = session.set_activity(&game.name, batch_start_ms, current_pid).await {
                            failed_count += 1;
                            let message = if is_russian() {
                                format!("Ошибка установки RPC для '{}': {}", game.name, e)
                            } else {
                                format!("Failed to set RPC activity for '{}': {}", game.name, e)
                            };
                            log_event(&app, "warn", &message);
                        } else {
                            succeeded_count += 1;
                            active_batch_info.push(ActiveGameInfo {
                                id: game.id.clone(),
                                name: game.name.clone(),
                                executable: None,
                                pid: None,
                                icon_hash: game.icon_hash.clone(),
                                method: "rpc".to_string(),
                            });
                            active_rpc_sessions.push(session);
                        }
                    }
                    Err(e) => {
                        failed_count += 1;
                        let message = if is_russian() {
                            format!("RPC пайп недоступен для '{}': {}", game.name, e)
                        } else {
                            format!("RPC pipe unavailable for '{}': {}", game.name, e)
                        };
                        log_event(&app, "warn", &message);
                    }
                }
            }

            tokio::time::sleep(Duration::from_millis(40)).await;
        }

        // Calculate holding duration with optional organic jitter
        let hold_secs = if config.jitter_enabled {
            let jitter = (batch_counter % 3) as u64 + 1;
            config.hold_duration_secs + jitter
        } else {
            config.hold_duration_secs
        };
        let hold_duration = if is_quest {
            Some(quest_duration)
        } else if is_farm {
            config.farm_duration_secs.map(Duration::from_secs)
        } else {
            Some(Duration::from_secs(hold_secs.clamp(3, 3600)))
        };

        // Emit spoofer tick event
        let elapsed_secs = start_time.elapsed().as_secs();
        let games_done = processed_count;
        let games_per_sec = if elapsed_secs > 0 {
            games_done as f64 / elapsed_secs as f64
        } else {
            0.0
        };
        let remaining_games = target_count.saturating_sub(games_done);
        let estimated_remaining_secs = if games_per_sec > 0.0 {
            (remaining_games as f64 / games_per_sec) as u64
        } else {
            (remaining_games as u64 / batch_size as u64) * config.hold_duration_secs
        };

        let _ = app.emit(
            "spoofer_tick",
            SpooferTickEvent {
                current_index: processed_count,
                total_games: target_count,
                active_batch: active_batch_info.clone(),
                elapsed_secs,
                estimated_remaining_secs,
                games_per_minute: games_per_sec * 60.0,
                succeeded_count,
                failed_count,
            },
        );

        let batch_titles: Vec<String> = current_batch
            .iter()
            .map(|g| {
                if g.method == SpoofMethod::Process {
                    format!("{} [exe]", g.name)
                } else {
                    format!("{} [rpc]", g.name)
                }
            })
            .collect();

        let batch_message = if is_russian() {
            format!(
                "Активный батч ({}/{}): {}",
                processed_count + current_batch.len(),
                target_count,
                batch_titles.join(", ")
            )
        } else {
            format!(
                "Active batch ({}/{}): {}",
                processed_count + current_batch.len(),
                target_count,
                batch_titles.join(", ")
            )
        };
        log_event(&app, "info", &batch_message);

        // Hold processes and RPC sessions for the duration
        let mut hold_start = Instant::now();
        let mut last_live_tick = Instant::now();
        let mut quest_restarts: HashMap<String, u32> = HashMap::new();
        while hold_duration.map_or(true, |limit| hold_start.elapsed() < limit) {
            if cancel_flag.load(Ordering::SeqCst) {
                break;
            }

            // Farm and quest runs can last hours: keep the timer live and keep the
            // dummies alive. A quest dummy that exits early is respawned, because a
            // dead process silently stops that quest's progress.
            if is_hold_run && last_live_tick.elapsed() >= Duration::from_secs(1) {
                last_live_tick = Instant::now();

                let mut dead_indices: Vec<usize> = Vec::new();
                for (idx, (child, _, _)) in spawned_processes.iter_mut().enumerate() {
                    if matches!(child.try_wait(), Ok(Some(_))) {
                        dead_indices.push(idx);
                    }
                }

                for idx in dead_indices.into_iter().rev() {
                    let (mut child, game, exe_path) = spawned_processes.remove(idx);
                    let dead_pid = child.id();
                    let _ = child.wait();

                    {
                        let mut pids_guard = active_pids.lock().await;
                        pids_guard.retain(|pid| *pid != dead_pid);
                    }

                    let restarts = quest_restarts.entry(game.id.clone()).or_insert(0);
                    if is_quest && *restarts < MAX_QUEST_RESTARTS {
                        *restarts += 1;
                        let attempt = *restarts;

                        match spawn_dummy_process(&exe_path, &game) {
                            Ok(new_child) => {
                                let new_pid = new_child.id();

                                #[cfg(windows)]
                                if let Some(guard) = &job_guard {
                                    guard.assign_pid(new_pid);
                                }

                                {
                                    let mut pids_guard = active_pids.lock().await;
                                    pids_guard.push(new_pid);
                                }

                                if let Some(info) =
                                    active_batch_info.iter_mut().find(|i| i.id == game.id)
                                {
                                    info.pid = Some(new_pid);
                                }
                                spawned_processes.push((new_child, game.clone(), exe_path));

                                let message = if is_russian() {
                                    format!(
                                        "Квест: процесс '{}' завершился, перезапуск {}/{}.",
                                        game.name, attempt, MAX_QUEST_RESTARTS
                                    )
                                } else {
                                    format!(
                                        "Quest: the process for '{}' exited, restarting it ({}/{}).",
                                        game.name, attempt, MAX_QUEST_RESTARTS
                                    )
                                };
                                log_event(&app, "warn", &message);
                            }
                            Err(e) => {
                                active_batch_info.retain(|info| info.id != game.id);
                                let message = if is_russian() {
                                    format!(
                                        "Квест: не удалось перезапустить '{}': {}",
                                        game.name, e
                                    )
                                } else {
                                    format!("Quest: failed to restart '{}': {}", game.name, e)
                                };
                                log_event(&app, "error", &message);
                            }
                        }
                    } else {
                        let message = if is_quest {
                            if is_russian() {
                                format!(
                                    "Квест: '{}' больше не перезапускается, прогресс по игре остановлен.",
                                    game.name
                                )
                            } else {
                                format!(
                                    "Quest: '{}' hit the restart limit, progress for this game is stopped.",
                                    game.name
                                )
                            }
                        } else if is_russian() {
                            format!(
                                "Фарм: процесс '{}' завершился, часы по этой игре не идут.",
                                game.name
                            )
                        } else {
                            format!(
                                "Farm: the process for '{}' exited, this game is no longer counted.",
                                game.name
                            )
                        };
                        log_event(&app, if is_quest { "error" } else { "warn" }, &message);
                        active_batch_info.retain(|info| info.id != game.id);
                    }
                }

                let held = hold_start.elapsed();
                let _ = app.emit(
                    "spoofer_tick",
                    SpooferTickEvent {
                        current_index: processed_count,
                        total_games: target_count,
                        active_batch: active_batch_info.clone(),
                        elapsed_secs: held.as_secs(),
                        estimated_remaining_secs: hold_duration
                            .map_or(0, |limit| limit.saturating_sub(held).as_secs()),
                        games_per_minute: 0.0,
                        succeeded_count,
                        failed_count,
                    },
                );
            }

            if pause_flag.load(Ordering::SeqCst) {
                *status_lock.write().await = SpooferStatus::Paused;
                let _ = app.emit(
                    "spoofer_status",
                    SpooferStatusEvent {
                        status: SpooferStatus::Paused,
                        message: None,
                    },
                );
                log_event(
                    &app,
                    "warn",
                    pick("Пауза в текущем батче.", "Paused inside the current batch."),
                );
                let pause_start = Instant::now();
                resume_notify.notified().await;
                if cancel_flag.load(Ordering::SeqCst) {
                    break;
                }
                hold_start += pause_start.elapsed();
                *status_lock.write().await = SpooferStatus::Running;
                let _ = app.emit(
                    "spoofer_status",
                    SpooferStatusEvent {
                        status: SpooferStatus::Running,
                        message: None,
                    },
                );
                log_event(
                &app,
                "info",
                pick("Продолжение работы.", "Resuming."),
            );
            }

            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        // Cleanup spawned child processes and temporary files
        for (mut child, _game, exe_path) in spawned_processes {
            let pid = child.id();
            force_kill_pid(pid);
            let _ = child.kill();
            let _ = child.wait();

            let _ = fs::remove_file(&exe_path);
            if let Some(parent) = exe_path.parent() {
                if parent != temp_root {
                    let _ = fs::remove_dir(parent);
                }
            }
        }

        // Close RPC sessions
        let cur_pid = std::process::id();
        for mut session in active_rpc_sessions {
            let _ = session.clear_activity(cur_pid).await;
            session.close().await;
        }

        {
            let mut pids_guard = active_pids.lock().await;
            pids_guard.clear();
        }

        // Record completed batch in persistent history (farm games stay available for normal runs)
        if !is_hold_run && !cancel_flag.load(Ordering::SeqCst) {
            // Games that failed to start are left out so they are retried next time
            let batch_ids: Vec<String> = active_batch_info.iter().map(|g| g.id.clone()).collect();
            record_played_batch(&batch_ids);
        }

        processed_count += current_batch.len();

        // Brief breather between batches to let the OS process table and Discord process watcher settle cleanly
        if !cancel_flag.load(Ordering::SeqCst) && processed_count < target_count && !pool.is_empty() {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        let elapsed_secs = start_time.elapsed().as_secs();
        let games_done = processed_count;
        let games_per_sec = if elapsed_secs > 0 {
            games_done as f64 / elapsed_secs as f64
        } else {
            0.0
        };
        let remaining_games = target_count.saturating_sub(games_done);
        let fallback_secs = if batch_size > 0 {
            let batches_left = (remaining_games + batch_size - 1) / batch_size;
            batches_left as u64 * config.hold_duration_secs
        } else {
            0
        };
        let estimated_remaining_secs = if games_per_sec > 0.0 {
            (remaining_games as f64 / games_per_sec) as u64
        } else {
            fallback_secs
        };

        let _ = app.emit(
            "spoofer_tick",
            SpooferTickEvent {
                current_index: processed_count,
                total_games: target_count,
                active_batch: Vec::new(),
                elapsed_secs,
                estimated_remaining_secs,
                games_per_minute: games_per_sec * 60.0,
                succeeded_count,
                failed_count,
            },
        );

        if cancel_flag.load(Ordering::SeqCst) {
            break;
        }
    }

    cleanup_temp_dir(&temp_root);

    let final_status = if cancel_flag.load(Ordering::SeqCst) {
        SpooferStatus::Idle
    } else {
        SpooferStatus::Finished
    };

    *status_lock.write().await = final_status;
    let _ = app.emit(
        "spoofer_status",
        SpooferStatusEvent {
            status: final_status,
            message: None,
        },
    );

    let total_elapsed = start_time.elapsed().as_secs();
    if is_quest {
        let completed = final_status == SpooferStatus::Finished;
        let minutes = total_elapsed / 60;
        let seconds = total_elapsed % 60;
        let message = if completed {
            if is_russian() {
                format!(
                    "Квест отработан: {} игр, {} мин {} сек. Откройте Discord → «Квесты» и проверьте прогресс: он обновляется с задержкой.",
                    processed_count, minutes, seconds
                )
            } else {
                format!(
                    "Quest run finished: {} games, {} min {} sec. Open Discord → Quests and check the progress: it updates with a delay.",
                    processed_count, minutes, seconds
                )
            }
        } else if is_russian() {
            format!("Квест остановлен: {} игр, {} сек.", processed_count, total_elapsed)
        } else {
            format!(
                "Quest run stopped: {} games, {} sec.",
                processed_count, total_elapsed
            )
        };
        log_event(&app, if completed { "success" } else { "info" }, &message);
    } else if is_farm {
        let message = if is_russian() {
            format!("Фарм завершён: {} игр, {} сек.", processed_count, total_elapsed)
        } else {
            format!("Farm finished: {} games, {} sec.", processed_count, total_elapsed)
        };
        log_event(&app, "info", &message);
    } else if final_status == SpooferStatus::Finished {
        let message = if is_russian() {
            format!(
                "Эмуляция {} игр успешно завершена за {} сек! Прогресс сохранен в истории.",
                processed_count, total_elapsed
            )
        } else {
            format!(
                "Spoofed {} games in {} sec! Progress saved to history.",
                processed_count, total_elapsed
            )
        };
        log_event(&app, "success", &message);
    } else {
        let message = if is_russian() {
            format!(
                "Работа остановлена. Обработано {} игр за {} сек.",
                processed_count, total_elapsed
            )
        } else {
            format!(
                "Stopped. Processed {} games in {} sec.",
                processed_count, total_elapsed
            )
        };
        log_event(&app, "info", &message);
    }
}

#[cfg(test)]
mod quest_mode_tests {
    use super::*;

    fn test_game(name: &str) -> SpoofableGame {
        SpoofableGame {
            id: "quest-test".to_string(),
            name: name.to_string(),
            rel_path: Some("QuestTest/QuestTest.exe".to_string()),
            arguments: None,
            icon_hash: None,
            method: SpoofMethod::Process,
            adult: false,
        }
    }

    /// The whole quest timer rests on this: the dummy has to start, stay alive for
    /// the whole window and be killable through the pid the caller tracks.
    #[test]
    fn dummy_process_survives_and_can_be_killed() {
        let dir = std::env::temp_dir().join("mimic_quest_test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("QuestTest")).unwrap();
        let exe_path = dir.join("QuestTest").join("QuestTest.exe");

        let base = Path::new(env!("OUT_DIR")).join("dummy");
        if fs::hard_link(&base, &exe_path).is_err() {
            fs::copy(&base, &exe_path).unwrap();
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&exe_path, fs::Permissions::from_mode(0o755));
        }

        let game = test_game("QuestTest");
        let mut child = spawn_dummy_process(&exe_path, &game).expect("dummy should start");

        std::thread::sleep(Duration::from_millis(600));
        assert!(
            matches!(child.try_wait(), Ok(None)),
            "the dummy exited before the quest timer could count it"
        );

        #[cfg(unix)]
        {
            let comm =
                fs::read_to_string(format!("/proc/{}/comm", child.id())).unwrap_or_default();
            // The dummy renames itself to the game name, which is what Discord
            // reads when it looks for a running quest game.
            assert_eq!(
                comm.trim(),
                "QuestTest",
                "the process name has to match the game name"
            );
        }

        let _ = child.kill();
        let _ = child.wait();
        let _ = fs::remove_dir_all(&dir);
    }
}

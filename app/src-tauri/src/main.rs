//! Reperio desktop app.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod disks;
mod helper;
mod media;
mod vault;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use reperio_engine::forensic::{self, ForensicOptions, Hashes};
use reperio_engine::health::{self, DriveHealth, SmartReport};
use reperio_engine::imaging::{self, ImageOptions, Mapfile, Pass};
use reperio_engine::preview::Signature;
use reperio_engine::raid::detect::{detect, Detection, Hints};
use reperio_engine::raid::Layout as RaidLayout;
use reperio_engine::session::{
    self, Event, FreeSpaceCheck, Item, Layout, Phase, ScanOptions, ScanResult, VolumeInfo,
};
use reperio_engine::source::read_tolerant;
use reperio_engine::surface::{self, SurfaceMap, SurfaceOptions};
use reperio_engine::{preview, similar, DeviceSource, Source};
use serde::Serialize;
use tauri::http::{Response, StatusCode};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_dialog::DialogExt;

/// What is currently open for recovery.
struct Opened {
    source: Arc<dyn Source>,
    /// What the user picked: a disk name or an image file name.
    label: String,
    /// The device or image path, used to refuse saving onto it.
    path: PathBuf,
    disk_id: Option<String>,
    /// For a RAID set: the member devices, none of which may be written to.
    members: Vec<PathBuf>,
}

struct AppState {
    opened: Mutex<Option<Opened>>,
    items: Mutex<Vec<Item>>,
    volumes: Mutex<Vec<VolumeInfo>>,
    cancel: Arc<AtomicBool>,
    /// Set to give up waiting for the password and the disk reader.
    cancel_open: AtomicBool,
    thumbs: Mutex<HashMap<u32, Arc<Vec<u8>>>>,
    /// Photo signatures for "Group similar photos", from thumbnails as they
    /// are made and from the grouping itself; None for photos that could
    /// not be decoded, so they are not tried again.
    sigs: Mutex<HashMap<u32, Option<Signature>>>,
    /// Set to stop copying the file the viewer showed last, when it moves
    /// on before the copy has finished.
    preview_cancel: Mutex<Arc<AtomicBool>>,
    /// Drives opened for a RAID set, kept from detecting its layout to
    /// opening it so the password is asked for once.
    raid_members: Mutex<Option<RaidDrives>>,
    cache: PathBuf,
    /// The iPhone backup that is open, with its photos and messages listed.
    ios: Mutex<Option<Arc<IosOpen>>>,
    /// Stops an iPhone backup in progress.
    ios_cancel: Arc<AtomicBool>,
    /// Plays video and audio previews where WebKit can't from `asset:`.
    media: Option<media::Media>,
    pool: Pool,
    /// What sampling the free space found (TRIM), for saved results.
    free_space: Mutex<Option<FreeSpaceCheck>>,
    /// The surface test has its own stop switch, so stopping it never
    /// stops a scan or a backup, and the other way round.
    surface_cancel: Arc<AtomicBool>,
    surface_running: Arc<AtomicBool>,
    /// Stops a copy from, or an image of, an Android phone.
    android_cancel: Arc<AtomicBool>,
}

type Job = Box<dyn FnOnce() + Send>;

/// The ids of the drives of a RAID set and the drives, opened.
type RaidDrives = (Vec<String>, Vec<Arc<dyn Source>>);
/// The chosen disks and their opened readers.
type ChosenDrives = (Vec<disks::Disk>, Vec<Arc<dyn Source>>);

/// A few worker threads for thumbnails. Jobs run newest first: the images
/// the user is looking at right now were requested last, and requests for
/// images scrolled past can wait.
struct Pool {
    jobs: Arc<(Mutex<Vec<Job>>, std::sync::Condvar)>,
}

impl Pool {
    fn new(threads: usize) -> Self {
        let jobs: Arc<(Mutex<Vec<Job>>, std::sync::Condvar)> = Arc::default();
        for _ in 0..threads {
            let jobs = jobs.clone();
            std::thread::spawn(move || loop {
                let job = {
                    let (lock, ready) = &*jobs;
                    let mut queue = lock.lock().unwrap();
                    loop {
                        if let Some(job) = queue.pop() {
                            break job;
                        }
                        queue = ready.wait(queue).unwrap();
                    }
                };
                job();
            });
        }
        Pool { jobs }
    }

    fn run(&self, job: impl FnOnce() + Send + 'static) {
        let (lock, ready) = &*self.jobs;
        lock.lock().unwrap().push(Box::new(job));
        ready.notify_one();
    }
}

#[derive(Serialize, Clone)]
struct SourceInfo {
    label: String,
    path: String,
    size: u64,
    is_disk: bool,
}

fn info(o: &Opened) -> SourceInfo {
    SourceInfo {
        label: o.label.clone(),
        path: o.path.display().to_string(),
        size: o.source.len(),
        is_disk: o.disk_id.is_some(),
    }
}

fn source(state: &AppState) -> Result<Arc<dyn Source>, String> {
    state
        .opened
        .lock()
        .unwrap()
        .as_ref()
        .map(|o| o.source.clone())
        .ok_or_else(|| "Nothing is open.".into())
}

fn reset(state: &AppState, opened: Opened) -> SourceInfo {
    let i = info(&opened);
    *state.opened.lock().unwrap() = Some(opened);
    state.items.lock().unwrap().clear();
    state.volumes.lock().unwrap().clear();
    state.thumbs.lock().unwrap().clear();
    state.sigs.lock().unwrap().clear();
    *state.free_space.lock().unwrap() = None;
    // A surface test of the drive opened before stops.
    state.surface_cancel.store(true, Ordering::Relaxed);
    i
}

#[tauri::command]
fn platform() -> &'static str {
    std::env::consts::OS
}

#[tauri::command]
async fn list_disks() -> Vec<disks::Disk> {
    disks::list()
}

/// Whether the drive reports itself as failing (SMART). Most card readers
/// don't pass this through, which gives `unknown`.
#[tauri::command]
async fn disk_health(id: String) -> Result<DriveHealth, String> {
    let disk = disks::list()
        .into_iter()
        .find(|d| d.id == id)
        .ok_or("That disk is no longer connected.")?;
    Ok(health::check(Path::new(&disk.device)))
}

/// Unmounts a disk if need be and opens it for reading.
fn open_member(disk: &disks::Disk, state: &AppState) -> Result<Arc<dyn Source>, String> {
    if disk.mounted {
        disks::unmount(&disk.id).map_err(|e| {
            format!("Could not unmount the disk: {e}. Close any windows or apps using it and try again.")
        })?;
    }
    helper::open_device(Path::new(&disk.device), &state.cancel_open).map_err(|e| e.to_string())
}

/// Everything the drive says about its health, for the Drive health
/// sheet: of disk `id`, or of the open disk when there is none.
#[tauri::command]
async fn drive_report(
    id: Option<String>,
    state: State<'_, AppState>,
) -> Result<SmartReport, String> {
    let device = match id {
        Some(id) => disks::list()
            .into_iter()
            .find(|d| d.id == id)
            .map(|d| PathBuf::from(d.device))
            .ok_or("That disk is no longer connected.")?,
        None => {
            let o = state.opened.lock().unwrap();
            let o = o.as_ref().ok_or("Nothing is open.")?;
            if o.disk_id.is_none() {
                return Err("A disk image has no health report of its own.".into());
            }
            o.path.clone()
        }
    };
    Ok(health::report(&device))
}

/// Cells in the map sent to the app: more than the sheet's grid has, so
/// it can draw its own grid from it at any width.
const SURFACE_CELLS: usize = 2048;

#[derive(Serialize, Clone)]
struct SurfaceUpdate {
    done: u64,
    total: u64,
    /// One character per cell; see `surface::Class::symbol`.
    map: String,
    summary: surface::Summary,
    finished: bool,
    cancelled: bool,
}

fn surface_update(map: &SurfaceMap, finished: bool, cancelled: bool) -> SurfaceUpdate {
    SurfaceUpdate {
        done: map.done(),
        total: map.total(),
        map: map.coarse(SURFACE_CELLS),
        summary: map.summary(),
        finished,
        cancelled,
    }
}

/// Reads the whole open disk, timing every block, and reports how it
/// went as `surface-update` events, then `surface-done`.
#[tauri::command]
fn start_surface(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    let src = source(&state)?;
    if state.surface_running.swap(true, Ordering::SeqCst) {
        return Err("A surface test is already running.".into());
    }
    state.surface_cancel.store(false, Ordering::Relaxed);
    let cancel = state.surface_cancel.clone();
    let running = state.surface_running.clone();
    std::thread::spawn(move || {
        let mut map = SurfaceMap::new(src.len(), &SurfaceOptions::default());
        let mut last = Instant::now() - Duration::from_secs(1);
        let finished = surface::run(&*src, &mut map, &cancel, |m| {
            if last.elapsed() > Duration::from_millis(200) {
                last = Instant::now();
                let _ = app.emit("surface-update", surface_update(m, false, false));
            }
        });
        running.store(false, Ordering::SeqCst);
        let _ = app.emit("surface-done", surface_update(&map, finished, !finished));
    });
    Ok(())
}

#[tauri::command]
fn cancel_surface(state: State<'_, AppState>) {
    state.surface_cancel.store(true, Ordering::Relaxed);
}

#[tauri::command]
async fn open_disk(id: String, state: State<'_, AppState>) -> Result<SourceInfo, String> {
    let disk = disks::list()
        .into_iter()
        .find(|d| d.id == id)
        .ok_or("That disk is no longer connected.")?;
    state.cancel_open.store(false, Ordering::Relaxed);
    let src = open_member(&disk, &state)?;
    let label = if disk.volumes.is_empty() {
        disk.name.clone()
    } else {
        format!("{} ({})", disk.volumes.join(", "), disk.name)
    };
    Ok(reset(
        &state,
        Opened {
            source: src,
            label,
            path: disk.device.into(),
            disk_id: Some(disk.id),
            members: Vec::new(),
        },
    ))
}

/// The drives of a RAID set, opened (and unmounted) once and kept.
fn raid_drives(ids: &[String], state: &AppState) -> Result<ChosenDrives, String> {
    if ids.is_empty() || ids.len() > reperio_engine::raid::MAX_MEMBERS {
        return Err("Choose at least one drive.".into());
    }
    let all = disks::list();
    let mut chosen = Vec::new();
    for id in ids {
        if chosen.iter().any(|d: &disks::Disk| &d.id == id) {
            return Err("A drive was chosen twice.".into());
        }
        chosen.push(
            all.iter()
                .find(|d| &d.id == id)
                .cloned()
                .ok_or("One of the drives is no longer connected.")?,
        );
    }
    if let Some((have, drives)) = state.raid_members.lock().unwrap().as_ref() {
        if have == ids {
            return Ok((chosen, drives.clone()));
        }
    }
    state.cancel_open.store(false, Ordering::Relaxed);
    let drives = chosen
        .iter()
        .map(|d| open_member(d, state))
        .collect::<Result<Vec<_>, _>>()?;
    *state.raid_members.lock().unwrap() = Some((ids.to_vec(), drives.clone()));
    Ok((chosen, drives))
}

/// How the chosen drives fit together: ranked layouts with confidence.
/// `hints` narrows the search when the user knows something.
#[tauri::command]
async fn detect_raid(
    ids: Vec<String>,
    hints: Option<Hints>,
    state: State<'_, AppState>,
) -> Result<Detection, String> {
    let (_, drives) = raid_drives(&ids, &state)?;
    let hints = hints.unwrap_or_default();
    tauri::async_runtime::spawn_blocking(move || detect(&drives, &hints, &AtomicBool::new(false)))
        .await
        .map_err(|e| e.to_string())
}

/// Combines the chosen drives into one virtual drive and opens it. With no
/// `layout`, the best one found is used.
#[tauri::command]
async fn open_raid(
    ids: Vec<String>,
    layout: Option<RaidLayout>,
    state: State<'_, AppState>,
) -> Result<SourceInfo, String> {
    let (chosen, drives) = raid_drives(&ids, &state)?;
    let layout = match layout {
        Some(l) => l,
        None => {
            let d = drives.clone();
            let found = tauri::async_runtime::spawn_blocking(move || {
                detect(&d, &Hints::default(), &AtomicBool::new(false))
            })
            .await
            .map_err(|e| e.to_string())?;
            found
                .candidates
                .into_iter()
                .next()
                .ok_or("Reperio could not work out how these drives fit together.")?
                .layout
        }
    };
    let src = layout.build(&drives)?;
    let names: Vec<&str> = chosen.iter().map(|d| d.name.as_str()).collect();
    let label = format!("{} ({})", layout.level.name(), names.join(" + "));
    let devices: Vec<PathBuf> = chosen.iter().map(|d| PathBuf::from(&d.device)).collect();
    let path = PathBuf::from(format!(
        "raid:{}",
        devices
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join("+")
    ));
    Ok(reset(
        &state,
        Opened {
            source: Arc::new(src),
            label,
            path,
            disk_id: None,
            members: devices,
        },
    ))
}

fn open_image_at(path: PathBuf, state: &AppState) -> Result<SourceInfo, String> {
    let src =
        DeviceSource::open(&path).map_err(|e| format!("Could not open {}: {e}", path.display()))?;
    let label = path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into(),
    );
    Ok(reset(
        state,
        Opened {
            source: Arc::new(src),
            label,
            path,
            disk_id: None,
            members: Vec::new(),
        },
    ))
}

#[tauri::command]
async fn open_image(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<Option<SourceInfo>, String> {
    let Some(file) = app
        .dialog()
        .file()
        .set_title("Open a disk image")
        .blocking_pick_file()
    else {
        return Ok(None);
    };
    let path = file.into_path().map_err(|e| e.to_string())?;
    open_image_at(path, &state).map(Some)
}

#[tauri::command]
async fn open_image_path(path: String, state: State<'_, AppState>) -> Result<SourceInfo, String> {
    open_image_at(PathBuf::from(path), &state)
}

/// Devices reachable through source plug-ins (an iPhone, a camera, a share,
/// anything a plug-in was written for). Asked separately from the disks, so
/// a slow plug-in never holds up the list of cards and drives.
#[tauri::command]
async fn plugin_devices() -> Vec<reperio_engine::devices::PluginDevice> {
    tauri::async_runtime::spawn_blocking(reperio_engine::devices::list)
        .await
        .unwrap_or_default()
}

/// Opens a device served by a source plug-in, to scan and recover from like
/// any disk. The plug-in only answers reads, so nothing can be written to it.
#[tauri::command]
async fn open_plugin_device(
    plugin: String,
    id: String,
    state: State<'_, AppState>,
) -> Result<SourceInfo, String> {
    state.cancel_open.store(false, Ordering::Relaxed);
    let (p, i) = (plugin.clone(), id.clone());
    let src = tauri::async_runtime::spawn_blocking(move || {
        reperio_engine::devices::PluginSource::open(&p, &i)
    })
    .await
    .map_err(|e| e.to_string())??;
    let label = src.label().to_string();
    Ok(reset(
        &state,
        Opened {
            source: Arc::new(src),
            label,
            path: PathBuf::from(format!("plugin:{plugin}:{id}")),
            disk_id: None,
            members: Vec::new(),
        },
    ))
}

#[derive(Serialize, Clone)]
struct ScanUpdate {
    phase: Phase,
    done: u64,
    total: u64,
    /// Where on the disk the deep scan is reading.
    at: Option<u64>,
    items: Vec<Item>,
    updated: Vec<Item>,
    volumes: Vec<VolumeInfo>,
    /// Whether the free space has already been erased (TRIM), once known.
    free_space: Option<FreeSpaceCheck>,
}

/// Where a scan of a source had got to, saved every 30 seconds so an
/// interrupted scan can continue later.
#[derive(Serialize, serde::Deserialize, Clone)]
struct Checkpoint {
    source: String,
    len: u64,
    /// Seconds since 1970.
    saved: i64,
    percent: f64,
    resume: session::Resume,
}

/// Summary of a saved checkpoint, for the app to offer continuing.
#[derive(Serialize, Clone)]
struct CheckpointInfo {
    percent: f64,
    found: usize,
    saved: i64,
}

/// How often an unfinished scan saves where it got to. Test hook:
/// REPERIO_TEST_CHECKPOINT_SECS shortens it.
fn checkpoint_every() -> Duration {
    let secs = std::env::var("REPERIO_TEST_CHECKPOINT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);
    Duration::from_secs(secs)
}

fn checkpoint_path(source: &str, len: u64) -> Option<PathBuf> {
    let key = reperio_engine::vault::fingerprint(format!("{source}\0{len}").as_bytes());
    Some(
        vault::dir()?
            .with_file_name("scans")
            .join(format!("{key:016x}.json")),
    )
}

fn load_checkpoint(source: &str, len: u64) -> Option<Checkpoint> {
    let data = std::fs::read(checkpoint_path(source, len)?).ok()?;
    let c: Checkpoint = serde_json::from_slice(&data).ok()?;
    (c.source == source && c.len == len).then_some(c)
}

fn save_checkpoint(c: &Checkpoint) {
    let Some(path) = checkpoint_path(&c.source, c.len) else {
        return;
    };
    let _ = std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")));
    let tmp = path.with_extension("partial");
    if serde_json::to_vec(c)
        .ok()
        .and_then(|d| std::fs::write(&tmp, d).ok())
        .is_some()
    {
        let _ = std::fs::rename(tmp, path);
    }
}

fn opened_name(state: &AppState) -> String {
    state
        .opened
        .lock()
        .unwrap()
        .as_ref()
        .map(|o| o.path.display().to_string())
        .unwrap_or_default()
}

/// A scan of the opened source that stopped partway, if any.
#[tauri::command]
fn scan_checkpoint(state: State<'_, AppState>) -> Option<CheckpointInfo> {
    let src = source(&state).ok()?;
    let c = load_checkpoint(&opened_name(&state), src.len())?;
    Some(CheckpointInfo {
        percent: c.percent,
        found: c.resume.carved.len(),
        saved: c.saved,
    })
}

#[derive(Serialize, Clone)]
struct ScanDone {
    cancelled: bool,
    bad_sectors: u64,
    /// Reads that worked but were very slow: a card starting to fail.
    slow_reads: u64,
    count: usize,
}

#[tauri::command]
fn start_scan(
    deep: bool,
    resume: Option<bool>,
    find_partitions: Option<bool>,
    plugins: Option<Vec<String>>,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let src = source(&state)?;
    reperio_engine::plugin::clear_stop();
    let name = opened_name(&state);
    let len = src.len();
    let resume = if resume == Some(true) {
        load_checkpoint(&name, len).map(|c| c.resume)
    } else {
        None
    };
    state.cancel.store(false, Ordering::Relaxed);
    state.items.lock().unwrap().clear();
    state.volumes.lock().unwrap().clear();
    // Ids are given out again from 0.
    state.thumbs.lock().unwrap().clear();
    state.sigs.lock().unwrap().clear();
    let cancel = state.cancel.clone();
    std::thread::spawn(move || {
        let state = app.state::<AppState>();
        let opts = ScanOptions {
            deep,
            existing: true,
            formats: Vec::new(),
            resume,
            find_partitions: find_partitions == Some(true),
            plugins,
            case_id: None,
            evidence_id: None,
            nsrl_db: None,
        };
        let mut update = ScanUpdate {
            phase: Phase::FileSystem,
            done: 0,
            total: 0,
            at: None,
            items: vec![],
            updated: vec![],
            volumes: vec![],
            free_space: None,
        };
        let mut last = Instant::now();
        let mut saved = Instant::now();
        let mut deep = (0u64, 0u64);
        let checkpoint = |deep: (u64, u64)| {
            let carved = state
                .items
                .lock()
                .unwrap()
                .iter()
                .filter(|i| i.origin == session::Origin::Carved && i.fix.is_none())
                .cloned()
                .collect();
            save_checkpoint(&Checkpoint {
                source: name.clone(),
                len,
                saved: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs() as i64),
                // As the scan screen shows it: how far the deep scan got.
                percent: 100.0 * deep.0 as f64 / deep.1.max(1) as f64,
                resume: session::Resume {
                    deep_from: deep.0,
                    deep_total: deep.1,
                    carved,
                },
            });
        };
        let result = session::scan(&*src, &name, &opts, &cancel, |ev| {
            match ev {
                Event::Phase(p) => update.phase = p,
                Event::Volume(v) => {
                    state.volumes.lock().unwrap().push(v.clone());
                    update.volumes.push(v.clone());
                }
                Event::FreeSpace(check) => {
                    *state.free_space.lock().unwrap() = Some(*check);
                    update.free_space = Some(*check);
                }
                Event::Item(item) => {
                    state.items.lock().unwrap().push(item.clone());
                    update.items.push(item.clone());
                }
                Event::Updated(item) => {
                    if let Some(slot) = state.items.lock().unwrap().get_mut(item.id as usize) {
                        *slot = item.clone();
                    }
                    state.thumbs.lock().unwrap().remove(&item.id);
                    state.sigs.lock().unwrap().remove(&item.id);
                    update.updated.push(item.clone());
                }
                Event::Progress { done, total, at } => {
                    update.done = done;
                    update.total = total;
                    update.at = at;
                    if matches!(update.phase, Phase::DeepScan) {
                        // Positions already searched, less one stretch in
                        // case the last was not finished.
                        deep = (done.saturating_sub(8 << 20), total);
                        if saved.elapsed() > checkpoint_every() {
                            saved = Instant::now();
                            checkpoint(deep);
                        }
                    }
                }
            }
            if last.elapsed() > Duration::from_millis(150) {
                last = Instant::now();
                let _ = app.emit("scan-update", &update);
                update.items.clear();
                update.updated.clear();
                update.volumes.clear();
            }
        });
        // Files found without names get them back from the Recovery Vault.
        // Stopped partway: keep where it got to, to continue later. A
        // finished scan needs no checkpoint.
        if result.cancelled && deep.1 > 0 {
            checkpoint(deep);
        } else if !result.cancelled {
            if let Some(p) = checkpoint_path(&name, len) {
                let _ = std::fs::remove_file(p);
            }
        }
        if !result.cancelled {
            let mut items = state.items.lock().unwrap();
            for id in vault::name_found(&*src, &mut items) {
                if let Some(item) = items.get(id as usize) {
                    update.updated.push(item.clone());
                }
            }
        }
        let _ = app.emit("scan-update", &update);
        let _ = app.emit(
            "scan-done",
            ScanDone {
                cancelled: result.cancelled,
                bad_sectors: result.bad_sectors,
                slow_reads: result.slow_reads,
                count: result.items.len(),
            },
        );
    });
    Ok(())
}

#[tauri::command]
fn cancel(state: State<'_, AppState>) {
    state.cancel.store(true, Ordering::Relaxed);
    // A plug-in being waited on is let go too.
    reperio_engine::plugin::request_stop();
}

/// Opens System Settings at Privacy & Security → Full Disk Access.
#[tauri::command]
fn open_full_disk_access() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles")
            .status()
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
fn cancel_open(state: State<'_, AppState>) {
    state.cancel_open.store(true, Ordering::Relaxed);
}

#[tauri::command]
fn items(state: State<'_, AppState>) -> Vec<Item> {
    state.items.lock().unwrap().clone()
}

/// Drives that can be protected by the Recovery Vault, and those that are.
#[tauri::command]
async fn vault_drives() -> Vec<vault::VaultDrive> {
    tauri::async_runtime::spawn_blocking(vault::drives)
        .await
        .unwrap_or_default()
}

/// Starts or stops protecting a drive.
#[tauri::command]
async fn vault_protect(id: String, on: bool) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || vault::protect(&id, on))
        .await
        .map_err(|e| e.to_string())?
}

#[derive(Serialize)]
struct GroupMember {
    id: u32,
    width: Option<u32>,
    height: Option<u32>,
}

#[derive(Serialize)]
struct SimilarGroup {
    best: u32,
    /// Best first.
    members: Vec<GroupMember>,
}

#[derive(Serialize, Clone)]
struct GroupProgress {
    done: usize,
    total: usize,
}

/// Groups of similar photos and of identical files, for "Group similar
/// photos". Photos whose thumbnails were shown already have signatures;
/// the rest are decoded small, on several threads, reporting progress.
#[tauri::command]
async fn similar_groups(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<Vec<SimilarGroup>, String> {
    let src = source(&state)?;
    let items = state.items.lock().unwrap().clone();
    let todo: Vec<Item> = {
        let sigs = state.sigs.lock().unwrap();
        items
            .iter()
            .filter(|i| is_photo(i) && preview::supported(i) && !sigs.contains_key(&i.id))
            .cloned()
            .collect()
    };
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let next = std::sync::atomic::AtomicUsize::new(0);
        let done = std::sync::atomic::AtomicUsize::new(0);
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get().clamp(2, 8));
        std::thread::scope(|scope| {
            for _ in 0..threads {
                scope.spawn(|| {
                    while let Some(item) = todo.get(next.fetch_add(1, Ordering::Relaxed)) {
                        // A photo that panics a decoder must not stop the
                        // count the progress loop below waits on.
                        let sig = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            preview::signature(&*src, item)
                        }))
                        .ok()
                        .flatten();
                        state.sigs.lock().unwrap().insert(item.id, sig);
                        done.fetch_add(1, Ordering::Relaxed);
                    }
                });
            }
            loop {
                let n = done.load(Ordering::Relaxed);
                let _ = app.emit(
                    "group-progress",
                    GroupProgress {
                        done: n,
                        total: todo.len(),
                    },
                );
                if n >= todo.len() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        });
        let sigs: HashMap<u32, Signature> = state
            .sigs
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(&id, s)| Some((id, (*s)?)))
            .collect();
        let exact = session::duplicate_groups(&*src, &items);
        similar::group(&items, &sigs, &exact)
            .into_iter()
            .map(|g| SimilarGroup {
                best: g.best,
                members: g
                    .members
                    .iter()
                    .map(|&id| GroupMember {
                        id,
                        width: sigs.get(&id).map(|s| s.width),
                        height: sigs.get(&id).map(|s| s.height),
                    })
                    .collect(),
            })
            .collect()
    })
    .await
    .map_err(|e| e.to_string())
}

/// Raw bytes of a file, for the byte view.
#[tauri::command]
async fn item_bytes(
    id: u32,
    offset: u64,
    len: u64,
    state: State<'_, AppState>,
) -> Result<Vec<u8>, String> {
    let src = source(&state)?;
    let item = item(&state, id)?;
    let view = reperio_engine::extent::MappedSource::new(&*src, item.extents.clone());
    let len = len.min(64 << 10).min(view.len().saturating_sub(offset));
    let mut buf = vec![0u8; len as usize];
    let (n, _) = read_tolerant(&view, offset, &mut buf);
    buf.truncate(n);
    Ok(buf)
}

fn item(state: &AppState, id: u32) -> Result<Item, String> {
    state
        .items
        .lock()
        .unwrap()
        .get(id as usize)
        .cloned()
        .ok_or_else(|| "Unknown file.".into())
}

#[derive(Serialize, Default)]
struct PreviewFile {
    /// "image", "video", "audio", "pdf", "text", "thumb" (nothing here
    /// decodes the file: show its thumbnail, if one can be made) or "none".
    kind: &'static str,
    path: Option<String>,
    /// Where to play it from, when `path` as an asset won't play (see
    /// [`media`]).
    url: Option<String>,
    text: Option<String>,
    /// Said with the preview, or in its place for "none".
    note: Option<String>,
}

/// Biggest files copied off the source to preview them. Videos play from
/// a copy in the app's cache; past these sizes the copy takes long enough
/// that recovering the file is the better way to watch it.
const MAX_VIDEO_PREVIEW: u64 = 4 << 30;
const MAX_AUDIO_PREVIEW: u64 = 1 << 30;
const MAX_DOC_PREVIEW: u64 = 512 << 20;
/// Photos up to this size are shown from the file itself, so 100% zoom
/// shows every pixel the camera took.
const MAX_IMAGE_COPY: u64 = 64 << 20;
/// Most room the copies in the cache take; the least recently used go.
const CACHE_BUDGET: u64 = 4 << 30;
/// Longest side of the previews the engine makes (RAW, TIFF, damaged
/// photos): as large as camera previews and most photos come.
const PREVIEW_MAX: u32 = 6000;
/// Most text shown of a text file or PDF.
const MAX_TEXT: u64 = 200_000;

fn is_photo(item: &Item) -> bool {
    matches!(
        item.kind,
        reperio_engine::Kind::Photo | reperio_engine::Kind::Raw
    )
}

#[derive(Serialize, Clone)]
struct PreviewProgress {
    id: u32,
    done: u64,
    total: u64,
}

#[tauri::command]
async fn preview(
    id: u32,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<PreviewFile, String> {
    let src = source(&state)?;
    let item = item(&state, id)?;
    let cache = state.cache.clone();
    let media = state.media.clone();
    // A new preview stops the copy still being made for the last one.
    let cancel = Arc::new(AtomicBool::new(false));
    let last = std::mem::replace(&mut *state.preview_cancel.lock().unwrap(), cancel.clone());
    last.store(true, Ordering::Relaxed);
    tauri::async_runtime::spawn_blocking(move || {
        make_preview(&*src, &item, &cache, &cancel, &app, media.as_ref())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Stops copying the file the viewer was showing, when it closes.
#[tauri::command]
fn cancel_preview(state: State<'_, AppState>) {
    state
        .preview_cancel
        .lock()
        .unwrap()
        .store(true, Ordering::Relaxed);
}

fn make_preview(
    src: &dyn Source,
    item: &Item,
    cache: &Path,
    cancel: &AtomicBool,
    app: &AppHandle,
    media: Option<&media::Media>,
) -> Result<PreviewFile, String> {
    let ext = item.ext.as_str();
    let mac = cfg!(target_os = "macos");
    let usable = matches!(
        item.health,
        session::Health::Good | session::Health::Unknown | session::Health::Repaired
    );
    let none = |note: &str| PreviewFile {
        kind: "none",
        note: Some(note.into()),
        ..Default::default()
    };
    let file = |kind, limit, what| -> Result<PreviewFile, String> {
        let path = copy_for_preview(src, item, cache, cancel, app, limit, what)?;
        let streamed = !mac && matches!(kind, "video" | "audio");
        let url = media
            .filter(|_| streamed)
            .zip(path.file_name())
            .map(|(m, n)| m.url(&n.to_string_lossy()));
        Ok(PreviewFile {
            kind,
            url,
            path: Some(path.display().to_string()),
            ..Default::default()
        })
    };

    // Photos WebKit shows itself, from the file: full size, animated GIFs
    // move. Damaged ones go through the engine, which shows what survived.
    let native = matches!(ext, "jpg" | "jpeg" | "jpe" | "png" | "gif" | "bmp" | "webp");
    if (native && usable && item.size <= MAX_IMAGE_COPY)
        || ext == "avif"
        || (mac && matches!(ext, "heic" | "heif"))
    {
        return file("image", MAX_IMAGE_COPY, "photo");
    }
    if preview::supported(item) {
        let path = cache.join(format!("{}-preview.jpg", item.id));
        if !path.exists() {
            let Some(jpeg) = preview::preview(src, item, PREVIEW_MAX) else {
                return Ok(none(if preview::raw(ext) {
                    "No preview picture was found inside this RAW file. It can still be recovered."
                } else {
                    "This photo could not be shown. It may be damaged."
                }));
            };
            std::fs::write(&path, jpeg).map_err(|e| e.to_string())?;
        }
        return Ok(PreviewFile {
            kind: "image",
            path: Some(path.display().to_string()),
            ..Default::default()
        });
    }
    if matches!(ext, "heic" | "heif") {
        return Ok(PreviewFile {
            kind: "thumb",
            note: Some("HEIC photos are shown full size on a Mac.".into()),
            ..Default::default()
        });
    }
    if matches!(
        ext,
        "mov" | "mp4" | "m4v" | "3gp" | "3g2" | "webm" | "mkv" | "ogv"
    ) {
        return file("video", MAX_VIDEO_PREVIEW, "video");
    }
    if matches!(
        ext,
        "mp3" | "m4a" | "wav" | "aac" | "aif" | "aiff" | "flac" | "ogg" | "oga" | "opus"
    ) {
        return file("audio", MAX_AUDIO_PREVIEW, "recording");
    }
    if ext == "pdf" {
        // WebKit on macOS shows PDF pages; elsewhere only the text can be.
        if mac {
            return file("pdf", MAX_DOC_PREVIEW, "PDF");
        }
        let data = read_head(src, item, 32 << 20)?;
        return Ok(match reperio_engine::pdftext::text(&data) {
            Some(text) => PreviewFile {
                kind: "text",
                text: Some(text),
                note: Some("Only the text of this PDF can be shown here. Recover it to see its pages.".into()),
                ..Default::default()
            },
            None => none("This PDF's pages can only be shown on a Mac, and its text could not be read here. It can still be recovered."),
        });
    }
    let texty = matches!(
        ext,
        "txt"
            | "csv"
            | "tsv"
            | "md"
            | "log"
            | "json"
            | "xml"
            | "html"
            | "htm"
            | "srt"
            | "vtt"
            | "ini"
            | "cfg"
            | "conf"
            | "yaml"
            | "yml"
            | "rtf"
            | "tex"
            | "css"
            | "js"
            | "py"
            | "sh"
            | "c"
            | "h"
            | "rs"
            | "eml"
            | "vcf"
            | "ics"
    );
    // Files of other kinds are shown as text when that is what they hold.
    if texty || item.size <= 16 << 20 {
        let head = read_head(src, item, MAX_TEXT)?;
        if let Some(text) = as_text(&head, texty) {
            return Ok(PreviewFile {
                kind: "text",
                text: Some(text),
                note: (item.size > MAX_TEXT).then(|| {
                    format!(
                        "The first {} of {} are shown.",
                        human(MAX_TEXT),
                        human(item.size)
                    )
                }),
                ..Default::default()
            });
        }
    }
    Ok(none(
        "There's no preview for this kind of file, but it can be recovered.",
    ))
}

/// Up to `max` bytes from the start of the file.
fn read_head(src: &dyn Source, item: &Item, max: u64) -> Result<Vec<u8>, String> {
    let view = reperio_engine::extent::MappedSource::new(src, item.extents.clone());
    let mut buf = vec![0u8; view.len().min(max) as usize];
    let (n, _) = read_tolerant(&view, 0, &mut buf);
    buf.truncate(n);
    Ok(buf)
}

/// The bytes as text, if they are text: UTF-8 (cut anywhere) or UTF-16
/// with its byte order mark, with no NULs. `known`: the name says text, so
/// stray bad bytes are allowed.
fn as_text(bytes: &[u8], known: bool) -> Option<String> {
    let utf16 = |le: bool| {
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| {
                if le {
                    u16::from_le_bytes([c[0], c[1]])
                } else {
                    u16::from_be_bytes([c[0], c[1]])
                }
            })
            .collect();
        String::from_utf16_lossy(&units)
    };
    let text = match bytes {
        [0xFF, 0xFE, ..] => utf16(true),
        [0xFE, 0xFF, ..] => utf16(false),
        _ => {
            let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
            match std::str::from_utf8(bytes) {
                Ok(s) => s.to_string(),
                // Cut off in the middle of a character at the end.
                Err(e) if e.error_len().is_none() => {
                    String::from_utf8_lossy(&bytes[..e.valid_up_to()]).into_owned()
                }
                Err(_) if known => String::from_utf8_lossy(bytes).into_owned(),
                Err(_) => return None,
            }
        }
    };
    // Trailing zeros are unused space after the data, not text.
    let text = text.trim_end_matches('\0');
    let controls = text
        .chars()
        .filter(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t' | '\x0c'))
        .count();
    (!text.is_empty() && (known || controls * 100 <= text.len())).then(|| text.replace('\0', ""))
}

/// Sizes as the app shows them: "1.4 GB".
fn human(bytes: u64) -> String {
    let units = ["bytes", "KB", "MB", "GB", "TB"];
    let (mut v, mut u) = (bytes as f64, 0);
    while v >= 1000.0 && u < units.len() - 1 {
        v /= 1000.0;
        u += 1;
    }
    match u {
        0 => format!("{bytes} bytes"),
        _ if v < 10.0 => format!("{v:.1} {}", units[u]),
        _ => format!("{v:.0} {}", units[u]),
    }
}

/// Copies the file into the cache to be shown from there, reporting its
/// progress to the viewer. Says why when the file is too large to.
fn copy_for_preview(
    src: &dyn Source,
    item: &Item,
    cache: &Path,
    cancel: &AtomicBool,
    app: &AppHandle,
    limit: u64,
    what: &str,
) -> Result<PathBuf, String> {
    if item.size > limit {
        return Err(format!(
            "This {what} is too large to preview here ({}). Recover it to open it.",
            human(item.size)
        ));
    }
    if !cache.join(format!("{}.{}", item.id, item.ext)).exists() {
        trim_cache(cache, item.size);
        if free_space(cache).is_some_and(|free| free < item.size.saturating_add(256 << 20)) {
            return Err(format!(
                "There isn't enough free space on this computer to preview this {what} ({}). It can still be recovered.",
                human(item.size)
            ));
        }
    }
    let mut last = Instant::now();
    let total = item.size;
    extract_to_cache(src, item, cache, Some(cancel), |done| {
        if last.elapsed() > Duration::from_millis(120) {
            last = Instant::now();
            let _ = app.emit(
                "preview-progress",
                PreviewProgress {
                    id: item.id,
                    done,
                    total,
                },
            );
        }
    })
}

/// Removes the least recently used files from the cache until `need`
/// more bytes fit in [`CACHE_BUDGET`], always keeping the two most recent:
/// one may be playing.
fn trim_cache(cache: &Path, need: u64) {
    let Ok(dir) = std::fs::read_dir(cache) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = dir
        .filter_map(|e| {
            let e = e.ok()?;
            let meta = e.metadata().ok()?;
            let path = e.path();
            let partial = path.extension().is_some_and(|x| x == "partial");
            let modified = meta.modified().ok()?;
            (meta.is_file() && !partial).then_some((modified, meta.len(), path))
        })
        .collect();
    files.sort();
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    let removable = files.len().saturating_sub(2);
    for (_, len, path) in files.into_iter().take(removable) {
        if total.saturating_add(need) <= CACHE_BUDGET {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total -= len;
        }
    }
}

/// A file writer that fails once `cancel` is set, to stop a copy.
struct Stoppable<'a> {
    file: std::fs::File,
    cancel: Option<&'a AtomicBool>,
}

impl std::io::Write for Stoppable<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            return Err(std::io::Error::other("Stopped."));
        }
        self.file.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

/// Writes the item into the cache, as it would be recovered (repaired
/// videos are rebuilt), and gives its path. Never writes to the source.
fn extract_to_cache(
    src: &dyn Source,
    item: &Item,
    cache: &Path,
    cancel: Option<&AtomicBool>,
    progress: impl FnMut(u64),
) -> Result<PathBuf, String> {
    use std::io::Write;
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let path = cache.join(format!("{}.{}", item.id, item.ext));
    if path.exists() {
        // Most recently used: the last the cache gives up.
        let _ = std::fs::File::options()
            .append(true)
            .open(&path)
            .and_then(|f| f.set_modified(std::time::SystemTime::now()));
        return Ok(path);
    }
    // The viewer and a Quick Look thumbnail may copy the same file at once.
    let tmp = cache.join(format!(
        "{}-{}.partial",
        item.id,
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let file = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
    let mut out = std::io::BufWriter::new(Stoppable { file, cancel });
    let result = session::write_item(src, item, &mut out, progress).and_then(|_| out.flush());
    drop(out);
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.to_string());
    }
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    Ok(path)
}

/// Thumbnail for the `thumb://` protocol. Uses the engine's decoders, and
/// on macOS falls back to Quick Look for videos, HEIC and PDFs.
fn make_thumbnail(state: &AppState, id: u32) -> Option<Arc<Vec<u8>>> {
    if let Some(t) = state.thumbs.lock().unwrap().get(&id) {
        return Some(t.clone());
    }
    let src = source(state).ok()?;
    let item = item(state, id).ok()?;
    let bytes = if preview::supported(&item) {
        let made = preview::thumbnail_with_signature(&*src, &item, 320);
        if is_photo(&item) {
            let sig = made.as_ref().map(|(_, sig)| *sig);
            state.sigs.lock().unwrap().insert(id, sig);
        }
        made.map(|(jpeg, _)| jpeg)
    } else {
        quicklook_thumbnail(&*src, &item, &state.cache)
    }?;
    let bytes = Arc::new(bytes);
    let mut thumbs = state.thumbs.lock().unwrap();
    if thumbs.len() > 4000 {
        thumbs.clear();
    }
    thumbs.insert(id, bytes.clone());
    Some(bytes)
}

#[cfg(target_os = "macos")]
fn quicklook_thumbnail(src: &dyn Source, item: &Item, cache: &Path) -> Option<Vec<u8>> {
    use reperio_engine::Kind;
    let wanted = matches!(item.kind, Kind::Video | Kind::Photo | Kind::Raw) || item.ext == "pdf";
    if !wanted || item.size > 400 << 20 {
        return None;
    }
    let file = extract_to_cache(src, item, cache, None, |_| {}).ok()?;
    let out_dir = cache.join("ql");
    std::fs::create_dir_all(&out_dir).ok()?;
    let ok = std::process::Command::new("qlmanage")
        .args(["-t", "-s", "320", "-o"])
        .arg(&out_dir)
        .arg(&file)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    let png = out_dir.join(format!("{}.png", file.file_name()?.to_string_lossy()));
    let bytes = if ok { std::fs::read(&png).ok() } else { None };
    let _ = std::fs::remove_file(&png);
    bytes
}

#[cfg(not(target_os = "macos"))]
fn quicklook_thumbnail(_: &dyn Source, _: &Item, _: &Path) -> Option<Vec<u8>> {
    None
}

#[derive(Serialize)]
struct Destination {
    path: String,
    free: Option<u64>,
}

#[tauri::command]
async fn choose_destination(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<Option<Destination>, String> {
    // Test hook: native folder pickers cannot be driven by automation.
    let dir = match std::env::var_os("REPERIO_TEST_DEST") {
        Some(dir) => PathBuf::from(dir),
        None => {
            let Some(dir) = app
                .dialog()
                .file()
                .set_title("Choose where to save recovered files")
                .blocking_pick_folder()
            else {
                return Ok(None);
            };
            dir.into_path().map_err(|e| e.to_string())?
        }
    };
    check_destination(&state, &dir)?;
    Ok(Some(Destination {
        free: free_space(&dir),
        path: dir.display().to_string(),
    }))
}

fn check_destination(state: &AppState, dir: &Path) -> Result<(), String> {
    let opened = state.opened.lock().unwrap();
    let Some(o) = opened.as_ref() else {
        return Ok(());
    };
    for member in &o.members {
        reperio_engine::safety::check_output(member, dir)?;
    }
    if o.members.is_empty() {
        reperio_engine::safety::check_output(&o.path, dir)?;
    }
    if o.disk_id.is_none() {
        // An image file: refuse only writing into the image's own path.
        if dir.starts_with(&o.path) {
            return Err("Choose a folder outside the disk image.".into());
        }
    }
    Ok(())
}

#[cfg(unix)]
fn free_space(dir: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(dir.as_os_str().as_bytes()).ok()?;
    // SAFETY: statvfs fills the zeroed struct.
    unsafe {
        let mut st: libc::statvfs = std::mem::zeroed();
        (libc::statvfs(c.as_ptr(), &mut st) == 0).then(|| st.f_bavail as u64 * st.f_frsize as u64)
    }
}

#[cfg(not(unix))]
fn free_space(_: &Path) -> Option<u64> {
    None
}

#[derive(Serialize, Clone)]
struct RecoverProgress {
    done: u64,
    total: u64,
    id: u32,
    name: String,
}

#[derive(Serialize, Clone)]
struct RecoverDone {
    count: usize,
    dest: String,
    bad_sectors: u64,
    error: Option<String>,
}

#[tauri::command]
fn recover(
    ids: Vec<u32>,
    dest: String,
    layout: Layout,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let src = source(&state)?;
    let dest = PathBuf::from(dest);
    check_destination(&state, &dest)?;
    let chosen: Vec<Item> = {
        let items = state.items.lock().unwrap();
        ids.iter()
            .filter_map(|&id| items.get(id as usize).cloned())
            .collect()
    };
    state.cancel.store(false, Ordering::Relaxed);
    let cancel = state.cancel.clone();
    std::thread::spawn(move || {
        let refs: Vec<&Item> = chosen.iter().collect();
        let mut last = Instant::now();
        // Test hook: slow copying down enough to watch it on a fast disk.
        let slow = std::env::var("REPERIO_TEST_SLOW_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .map(Duration::from_millis);
        let result = session::recover(&*src, &refs, &dest, layout, &cancel, |done, total, item| {
            if let Some(d) = slow {
                std::thread::sleep(d);
            }
            if last.elapsed() > Duration::from_millis(50) {
                last = Instant::now();
                let _ = app.emit(
                    "recover-progress",
                    RecoverProgress {
                        done,
                        total,
                        id: item.id,
                        name: item.name.clone(),
                    },
                );
            }
        });
        let done = match result {
            Ok(rec) => {
                let _ = session::write_report(&dest.join("Reperio report.csv"), &dest, &refs, &rec);
                RecoverDone {
                    count: rec.len(),
                    dest: dest.display().to_string(),
                    bad_sectors: rec.iter().map(|r| r.bad_sectors).sum(),
                    error: None,
                }
            }
            Err(e) => RecoverDone {
                count: 0,
                dest: dest.display().to_string(),
                bad_sectors: 0,
                error: Some(e.to_string()),
            },
        };
        let _ = app.emit("recover-done", done);
    });
    Ok(())
}

#[derive(Serialize)]
struct PluginInfo {
    name: String,
    file: String,
    volumes: Vec<String>,
    /// `volume` (reads a file system) or `analyzer` (forensic).
    kind: String,
    targets: Vec<String>,
    description: String,
    version: String,
    author: String,
    protocol: u32,
    permissions: Vec<String>,
    requires: Option<String>,
    /// Settings the plug-in asks for, and what they are now.
    options: Vec<reperio_engine::plugin::PluginOption>,
    values: serde_json::Map<String, serde_json::Value>,
    /// How its last run went.
    last_run: Option<reperio_engine::plugin::Health>,
    /// `new` (not looked at yet), `known` or `changed` (not the file you had):
    /// a record, never a block.
    trust: String,
    sha256: String,
}

#[derive(Serialize)]
struct PluginsInfo {
    folders: Vec<String>,
    plugins: Vec<PluginInfo>,
    /// Files in the folders that could not be used, and why.
    problems: Vec<PluginProblem>,
}

#[derive(Serialize)]
struct PluginProblem {
    file: String,
    reason: String,
}

/// The plug-ins found, and the folders they are looked for in.
#[tauri::command]
async fn plugins_info() -> PluginsInfo {
    tauri::async_runtime::spawn_blocking(|| {
        let found = reperio_engine::plugin::discovery();
        PluginsInfo {
            folders: reperio_engine::plugin::folders()
                .iter()
                .map(|f| f.display().to_string())
                .collect(),
            plugins: found
                .plugins
                .into_iter()
                .map(|p| {
                    let values = reperio_engine::plugin::options_for(&p);
                    let last_run = reperio_engine::plugin::last_run(&p.name);
                    let (trust, sha256) = reperio_engine::plugin_install::trust_state(&p);
                    PluginInfo {
                        last_run,
                        trust: trust.to_string(),
                        sha256,
                        file: p.source.as_ref().unwrap_or(&p.path).display().to_string(),
                        name: p.name,
                        volumes: p.fs,
                        kind: p.kind,
                        targets: p.targets,
                        description: p.description,
                        version: p.version,
                        author: p.author,
                        protocol: p.protocol,
                        permissions: p.permissions,
                        requires: p.requires,
                        options: p.options,
                        values,
                    }
                })
                .collect(),
            problems: found
                .problems
                .into_iter()
                .map(|p| PluginProblem {
                    file: p.file,
                    reason: p.reason,
                })
                .collect(),
        }
    })
    .await
    .unwrap_or(PluginsInfo {
        folders: Vec::new(),
        plugins: Vec::new(),
        problems: Vec::new(),
    })
}

// ---------------------------------------------------------------------------
// iPhones and iPads
// ---------------------------------------------------------------------------

/// An iPhone backup that is open: its photos and messages, listed once.
struct IosOpen {
    backup: Mutex<reperio_engine::ios::Backup>,
    photos: Vec<reperio_engine::ios::Photo>,
    messages: Vec<reperio_engine::ios::Message>,
    /// Why photos or messages could not be read, if they could not.
    photos_error: Option<String>,
    messages_error: Option<String>,
}

#[derive(Serialize)]
struct IosStatus {
    /// `libimobiledevice` programs that are not installed.
    missing: Vec<String>,
    devices: Vec<reperio_engine::ios::Device>,
    backups: Vec<reperio_engine::ios::BackupInfo>,
    error: Option<String>,
}

#[derive(Serialize)]
struct IosOverview {
    photos: usize,
    deleted_photos: usize,
    recoverable_photos: usize,
    messages: usize,
    deleted_messages: usize,
    photos_error: Option<String>,
    messages_error: Option<String>,
    /// The backup holds WhatsApp's and the Notes app's databases.
    whatsapp: bool,
    notes: bool,
}

#[derive(Serialize, Clone)]
struct IosProgress {
    percent: u8,
    text: String,
}

/// Where the app keeps the backups it makes.
fn ios_backup_folder(app: &AppHandle) -> PathBuf {
    app.path()
        .app_data_dir()
        .unwrap_or_else(|_| std::env::temp_dir())
        .join("iPhone backups")
}

/// Connected iPhones and iPads, the backups on this computer, and which of
/// the tools Reperio needs are missing.
#[tauri::command]
async fn ios_status(app: AppHandle) -> IosStatus {
    let ours = ios_backup_folder(&app);
    tauri::async_runtime::spawn_blocking(move || {
        let tools = reperio_engine::ios::Tools::default();
        let missing: Vec<String> = tools.missing().into_iter().map(str::to_string).collect();
        let (devices, error) = if missing.is_empty() {
            match reperio_engine::ios::devices(&tools) {
                Ok(d) => (d, None),
                Err(e) => (Vec::new(), Some(e)),
            }
        } else {
            (Vec::new(), None)
        };
        let mut folders = vec![ours];
        folders.extend(reperio_engine::ios::finder_backups_folder());
        IosStatus {
            missing,
            devices,
            backups: reperio_engine::ios::backups(&folders),
            error,
        }
    })
    .await
    .unwrap_or(IosStatus {
        missing: Vec::new(),
        devices: Vec::new(),
        backups: Vec::new(),
        error: None,
    })
}

/// Asks the device to trust this computer; the person taps Trust on it.
#[tauri::command]
async fn ios_pair(udid: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        reperio_engine::ios::pair(&reperio_engine::ios::Tools::default(), &udid)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Backs the device up. Returns the backup's folder.
#[tauri::command]
async fn ios_backup(
    udid: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<String, String> {
    let cancel = state.ios_cancel.clone();
    cancel.store(false, Ordering::Relaxed);
    let dest = ios_backup_folder(&app);
    tauri::async_runtime::spawn_blocking(move || {
        reperio_engine::ios::backup(
            &reperio_engine::ios::Tools::default(),
            &udid,
            &dest,
            &cancel,
            &mut |p| {
                let _ = app.emit(
                    "ios-progress",
                    IosProgress {
                        percent: p.percent,
                        text: p.text,
                    },
                );
            },
        )
        .map(|dir| dir.display().to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
fn ios_cancel(state: State<'_, AppState>) {
    state.ios_cancel.store(true, Ordering::Relaxed);
    reperio_engine::plugin::request_stop();
}

/// Opens a backup and lists its photos and messages.
#[tauri::command]
async fn ios_open_backup(path: String, state: State<'_, AppState>) -> Result<IosOverview, String> {
    let open = tauri::async_runtime::spawn_blocking(move || {
        let backup = reperio_engine::ios::Backup::open(Path::new(&path))?;
        let (photos, photos_error) = match backup.photos() {
            Ok(p) => (p, None),
            Err(e) => (Vec::new(), Some(e)),
        };
        let (messages, messages_error) = match backup.messages("", 50_000) {
            Ok(m) => (m, None),
            Err(e) => (Vec::new(), Some(e)),
        };
        Ok::<_, String>(IosOpen {
            backup: Mutex::new(backup),
            photos,
            messages,
            photos_error,
            messages_error,
        })
    })
    .await
    .map_err(|e| e.to_string())??;
    let overview = IosOverview {
        photos: open.photos.len(),
        deleted_photos: open.photos.iter().filter(|p| p.deleted).count(),
        recoverable_photos: open
            .photos
            .iter()
            .filter(|p| p.deleted && p.file_id.is_some())
            .count(),
        messages: open.messages.len(),
        deleted_messages: open.messages.iter().filter(|m| m.deleted).count(),
        photos_error: open.photos_error.clone(),
        messages_error: open.messages_error.clone(),
        whatsapp: !open
            .backup
            .lock()
            .unwrap()
            .files(None, "ChatStorage.sqlite", 1)
            .is_empty(),
        notes: !open
            .backup
            .lock()
            .unwrap()
            .files(None, "NoteStore.sqlite", 1)
            .is_empty(),
    };
    *state.ios.lock().unwrap() = Some(Arc::new(open));
    Ok(overview)
}

fn ios_open(state: &AppState) -> Result<Arc<IosOpen>, String> {
    state
        .ios
        .lock()
        .unwrap()
        .clone()
        .ok_or_else(|| "No backup is open.".to_string())
}

#[tauri::command]
fn ios_photos(state: State<'_, AppState>) -> Result<Vec<reperio_engine::ios::Photo>, String> {
    Ok(ios_open(&state)?.photos.clone())
}

#[tauri::command]
fn ios_messages(state: State<'_, AppState>) -> Result<Vec<reperio_engine::ios::Message>, String> {
    Ok(ios_open(&state)?.messages.clone())
}

/// Messages deleted for good, recovered from the message database's
/// leftovers: whole rows with their text, sender and date.
#[tauri::command]
async fn ios_deleted_messages(
    state: State<'_, AppState>,
) -> Result<Vec<reperio_engine::ios::Message>, String> {
    let open = ios_open(&state)?;
    tauri::async_runtime::spawn_blocking(move || open.backup.lock().unwrap().deleted_messages())
        .await
        .map_err(|e| e.to_string())?
}

/// WhatsApp's chats and messages in the open backup.
#[tauri::command]
async fn ios_whatsapp(
    state: State<'_, AppState>,
) -> Result<reperio_engine::ios_apps::WhatsApp, String> {
    let open = ios_open(&state)?;
    tauri::async_runtime::spawn_blocking(move || open.backup.lock().unwrap().whatsapp())
        .await
        .map_err(|e| e.to_string())?
}

/// WhatsApp messages deleted for good, recovered from its database's leftovers.
#[tauri::command]
async fn ios_whatsapp_deleted(
    state: State<'_, AppState>,
) -> Result<Vec<reperio_engine::ios_apps::WaMessage>, String> {
    let open = ios_open(&state)?;
    tauri::async_runtime::spawn_blocking(move || open.backup.lock().unwrap().whatsapp_deleted())
        .await
        .map_err(|e| e.to_string())?
}

/// Notes in the open backup, including the Recently Deleted ones.
#[tauri::command]
async fn ios_notes(
    state: State<'_, AppState>,
) -> Result<Vec<reperio_engine::ios_apps::Note>, String> {
    let open = ios_open(&state)?;
    tauri::async_runtime::spawn_blocking(move || open.backup.lock().unwrap().notes())
        .await
        .map_err(|e| e.to_string())?
}

/// Notes deleted for good, recovered from the Notes database's leftovers.
#[tauri::command]
async fn ios_notes_deleted(
    state: State<'_, AppState>,
) -> Result<Vec<reperio_engine::ios_apps::Note>, String> {
    let open = ios_open(&state)?;
    tauri::async_runtime::spawn_blocking(move || open.backup.lock().unwrap().notes_deleted())
        .await
        .map_err(|e| e.to_string())?
}

/// Saves notes as text files in `dest`, one each, named by their titles.
#[tauri::command]
fn ios_save_notes(notes: Vec<(String, String)>, dest: String) -> Result<usize, String> {
    let dir = Path::new(&dest);
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let mut n = 0;
    for (title, text) in notes {
        let stem: String = title
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.') {
                    c
                } else {
                    '_'
                }
            })
            .take(60)
            .collect();
        let stem = if stem.trim().is_empty() {
            "note".to_string()
        } else {
            stem.trim().to_string()
        };
        let mut path = dir.join(format!("{stem}.txt"));
        let mut k = 2;
        while path.exists() {
            path = dir.join(format!("{stem} ({k}).txt"));
            k += 1;
        }
        std::fs::write(&path, text).map_err(|e| e.to_string())?;
        n += 1;
    }
    Ok(n)
}

/// Deleted text left in the message database's free pages.
#[tauri::command]
async fn ios_fragments(
    state: State<'_, AppState>,
) -> Result<Vec<reperio_engine::ios::Fragment>, String> {
    let open = ios_open(&state)?;
    tauri::async_runtime::spawn_blocking(move || open.backup.lock().unwrap().message_fragments())
        .await
        .map_err(|e| e.to_string())
}

/// Runs analyzer plug-ins on the open backup (those whose targets include
/// `ios-backup`). Their reports go in a new folder in the app's data folder.
#[tauri::command]
async fn ios_analyze(
    names: Vec<String>,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<Vec<reperio_engine::analyzer::Report>, String> {
    let open = ios_open(&state)?;
    let cancel = state.android_cancel.clone();
    cancel.store(false, Ordering::Relaxed);
    reperio_engine::plugin::clear_stop();
    let dest = state
        .cache
        .join("analysis")
        .join(format!("ios-{}", now_secs()));
    tauri::async_runtime::spawn_blocking(move || {
        let backup = open.backup.lock().unwrap();
        let target = reperio_engine::analyzer::Target {
            device: None,
            image: None,
            backup: Some(&*backup),
        };
        let mut reports = Vec::new();
        for p in reperio_engine::analyzer::runnable_on("ios-backup")
            .into_iter()
            .filter(|p| names.contains(&p.name))
        {
            let name = p.name.clone();
            reports.push(reperio_engine::analyzer::run(
                &p,
                &target,
                "ios-backup",
                &dest,
                &reperio_engine::analyzer::RunOptions::default(),
                &mut |pr| {
                    let _ = app.emit(
                        "android-progress",
                        AndroidProgress {
                            done: pr.done,
                            total: pr.total,
                            name: format!("{name}: {}", pr.text),
                        },
                    );
                },
                &cancel,
            ));
        }
        reports
    })
    .await
    .map_err(|e| e.to_string())
}

/// Files in the backup whose path contains `search`.
#[tauri::command]
fn ios_files(
    search: String,
    state: State<'_, AppState>,
) -> Result<Vec<reperio_engine::ios::BackupFile>, String> {
    Ok(ios_open(&state)?
        .backup
        .lock()
        .unwrap()
        .files(None, &search, 2_000))
}

/// Where to copy to. Test hook: REPERIO_TEST_DEST.
#[tauri::command]
async fn ios_choose_folder(app: AppHandle) -> Result<Option<String>, String> {
    if let Some(dir) = std::env::var_os("REPERIO_TEST_DEST") {
        return Ok(Some(PathBuf::from(dir).display().to_string()));
    }
    let Some(dir) = app
        .dialog()
        .file()
        .set_title("Choose where to save")
        .blocking_pick_folder()
    else {
        return Ok(None);
    };
    Ok(Some(
        dir.into_path()
            .map_err(|e| e.to_string())?
            .display()
            .to_string(),
    ))
}

/// Copies photos (by their file ids) and files out of the backup.
#[tauri::command]
async fn ios_export(
    photos: Vec<String>,
    files: Vec<String>,
    dest: String,
    state: State<'_, AppState>,
) -> Result<usize, String> {
    let open = ios_open(&state)?;
    tauri::async_runtime::spawn_blocking(move || {
        let wanted: std::collections::HashSet<&String> = photos.iter().collect();
        let chosen: Vec<reperio_engine::ios::Photo> = open
            .photos
            .iter()
            .filter(|p| p.file_id.as_ref().is_some_and(|id| wanted.contains(id)))
            .cloned()
            .collect();
        let backup = open.backup.lock().unwrap();
        let dest = Path::new(&dest);
        Ok::<_, String>(backup.export_photos(&chosen, dest)? + backup.export_files(&files, dest)?)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// A thumbnail of a photo in the open backup, for the `thumb://` protocol.
fn ios_thumbnail(state: &AppState, file_id: &str) -> Option<Vec<u8>> {
    let open = state.ios.lock().unwrap().clone()?;
    let backup = open.backup.lock().ok()?;
    backup.thumbnail(file_id, 320)
}

#[derive(Serialize, Clone)]
struct PluginLog {
    plugin: String,
    line: String,
}

/// The limits the person chose for plug-ins: `idle_secs` (stop one that has
/// said nothing for that long) and `max_secs` (stop one that has run that
/// long). 0 is no limit, which is how Reperio starts.
#[tauri::command]
fn plugin_set_limits(idle_secs: u64, max_secs: u64) {
    let secs = |n: u64| (n > 0).then(|| Duration::from_secs(n));
    reperio_engine::plugin::set_limits(reperio_engine::plugin::Limits {
        idle: secs(idle_secs),
        hard: secs(max_secs),
    });
}

/// Installs a plug-in from a path or URL (a file, a folder with a plugin.json,
/// or a .zip). With `sha256`, a file that differs is not installed.
#[tauri::command]
async fn plugin_install(
    source: String,
    sha256: Option<String>,
) -> Result<reperio_engine::plugin_install::Installed, String> {
    tauri::async_runtime::spawn_blocking(move || {
        reperio_engine::plugin_install::install(&source, sha256.as_deref())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Asks for a file or a folder to install from.
#[tauri::command]
async fn plugin_pick(app: AppHandle, folder: bool) -> Result<Option<String>, String> {
    let dialog = app.dialog().file().set_title("Choose a plug-in");
    let picked = if folder {
        dialog.blocking_pick_folder()
    } else {
        dialog.blocking_pick_file()
    };
    match picked {
        Some(p) => Ok(Some(
            p.into_path()
                .map_err(|e| e.to_string())?
                .display()
                .to_string(),
        )),
        None => Ok(None),
    }
}

/// Removes an installed plug-in.
#[tauri::command]
async fn plugin_remove(name: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || reperio_engine::plugin_install::remove(&name))
        .await
        .map_err(|e| e.to_string())?
}

/// Says the plug-in's current file is the one you know.
#[tauri::command]
fn plugin_trust(name: String) -> Result<(), String> {
    let p = reperio_engine::plugin::discover()
        .into_iter()
        .find(|p| p.name == name)
        .ok_or("That plug-in is not installed.")?;
    reperio_engine::plugin_install::trust(&p);
    Ok(())
}

/// Checks a plug-in against the protocol: it starts, says what it is, and for
/// its kind behaves on made-up data. Nothing is written outside a scratch folder.
#[tauri::command]
async fn plugin_test(file: String) -> Result<Vec<reperio_engine::plugin_check::Check>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let program = reperio_engine::plugin::build_for_test(Path::new(&file))?;
        Ok(reperio_engine::plugin_check::check(&program))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Remembers the settings chosen for a plug-in; it is given them on every run.
#[tauri::command]
fn plugin_set_options(
    name: String,
    values: serde_json::Map<String, serde_json::Value>,
) -> Result<(), String> {
    reperio_engine::plugin::save_options(&name, &values).map_err(|e| e.to_string())
}

/// Opens the folder for the user's own plug-ins, making it first.
#[tauri::command]
fn open_plugins_folder() -> Result<(), String> {
    let dir = reperio_engine::plugin::user_folder().ok_or("No home folder.")?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    reveal(dir.display().to_string())
}

fn scans_dir() -> Option<PathBuf> {
    Some(vault::dir()?.with_file_name("scans"))
}

#[derive(Serialize)]
struct AppInfo {
    version: &'static str,
    /// Scans that stopped partway and can be continued.
    saved_scans: usize,
    saved_bytes: u64,
}

#[tauri::command]
fn app_info() -> AppInfo {
    let (mut n, mut bytes) = (0, 0);
    for e in scans_dir()
        .and_then(|d| std::fs::read_dir(d).ok())
        .into_iter()
        .flatten()
        .flatten()
    {
        n += 1;
        bytes += e.metadata().map_or(0, |m| m.len());
    }
    AppInfo {
        version: env!("CARGO_PKG_VERSION"),
        saved_scans: n,
        saved_bytes: bytes,
    }
}

/// Forgets scans that stopped partway. Never touches a drive.
#[tauri::command]
fn clear_saved_scans() -> usize {
    let mut n = 0;
    for e in scans_dir()
        .and_then(|d| std::fs::read_dir(d).ok())
        .into_iter()
        .flatten()
        .flatten()
    {
        if std::fs::remove_file(e.path()).is_ok() {
            n += 1;
        }
    }
    n
}

// ---------------------------------------------------------------------------
// Android phones (adb)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct AndroidDevices {
    /// Set when adb itself is missing: how to get it.
    adb_error: Option<String>,
    devices: Vec<AndroidDevice>,
}

#[derive(Serialize)]
struct AndroidDevice {
    serial: String,
    state: String,
    /// Why the phone cannot be used yet, if so.
    problem: Option<String>,
}

/// The phones adb can see. Cheap, so the screen asks every few seconds.
#[tauri::command]
async fn android_devices() -> AndroidDevices {
    tauri::async_runtime::spawn_blocking(|| {
        let found = reperio_engine::android::find_adb()
            .and_then(|bin| reperio_engine::android::devices(&bin));
        match found {
            Err(e) => AndroidDevices {
                adb_error: Some(e),
                devices: Vec::new(),
            },
            Ok(list) => AndroidDevices {
                adb_error: None,
                devices: list
                    .into_iter()
                    .map(|d| AndroidDevice {
                        problem: reperio_engine::android::state_problem(&d.state),
                        serial: d.serial,
                        state: d.state,
                    })
                    .collect(),
            },
        }
    })
    .await
    .unwrap_or(AndroidDevices {
        adb_error: Some("Could not look for phones.".into()),
        devices: Vec::new(),
    })
}

#[derive(Serialize)]
struct AndroidFacts {
    #[serde(flatten)]
    facts: reperio_engine::android::Facts,
    verdict: String,
}

#[tauri::command]
async fn android_facts(serial: String) -> Result<AndroidFacts, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let adb = reperio_engine::android::Adb::connect(Some(&serial))?;
        let facts = reperio_engine::android::facts(&adb);
        Ok(AndroidFacts {
            verdict: reperio_engine::android::verdict(&facts),
            facts,
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

#[derive(Serialize, Clone)]
struct AndroidProgress {
    done: u64,
    total: u64,
    name: String,
}

/// Copies what the phone keeps after a delete into `dest`.
#[tauri::command]
async fn android_pull(
    serial: String,
    dest: String,
    everything: bool,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<reperio_engine::android::Pulled, String> {
    let cancel = state.android_cancel.clone();
    cancel.store(false, Ordering::Relaxed);
    tauri::async_runtime::spawn_blocking(move || {
        let adb = reperio_engine::android::Adb::connect(Some(&serial))?;
        reperio_engine::android::pull(
            &adb,
            Path::new(&dest),
            everything,
            &mut |p| {
                let _ = app.emit(
                    "android-progress",
                    AndroidProgress {
                        done: p.done as u64,
                        total: p.total as u64,
                        name: p.name.to_string(),
                    },
                );
            },
            &cancel,
        )
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Images a rooted phone into a new file in `dir`: `what` is `whole` (the
/// whole storage chip, partition table included) or `userdata`.
#[tauri::command]
async fn android_image(
    serial: String,
    dir: String,
    what: String,
    even_if_encrypted: bool,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<reperio_engine::android::Imaged, String> {
    let cancel = state.android_cancel.clone();
    cancel.store(false, Ordering::Relaxed);
    tauri::async_runtime::spawn_blocking(move || {
        let adb = reperio_engine::android::Adb::connect(Some(&serial))?;
        let facts = reperio_engine::android::facts(&adb);
        let stem: String = facts
            .name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let (target, suffix) = if what == "whole" {
            let chip = facts
                .storage
                .as_ref()
                .ok_or("The phone did not list its storage chip.")?;
            (
                reperio_engine::android::Target::Device(chip.name.clone()),
                "full-storage",
            )
        } else {
            (
                reperio_engine::android::Target::Partition("userdata".into()),
                "userdata",
            )
        };
        let dest = Path::new(&dir).join(format!("{stem}-{suffix}.img"));
        reperio_engine::android::image(
            &adb,
            &facts,
            &dest,
            &target,
            even_if_encrypted,
            &mut |done, total| {
                let _ = app.emit(
                    "android-progress",
                    AndroidProgress {
                        done,
                        total,
                        name: String::new(),
                    },
                );
            },
            &cancel,
        )
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Images every partition of a rooted phone, one file each, into a new
/// folder in `dir`, with a manifest of hashes.
#[tauri::command]
async fn android_image_all(
    serial: String,
    dir: String,
    even_if_encrypted: bool,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<reperio_engine::android::AllImaged, String> {
    let cancel = state.android_cancel.clone();
    cancel.store(false, Ordering::Relaxed);
    tauri::async_runtime::spawn_blocking(move || {
        let adb = reperio_engine::android::Adb::connect(Some(&serial))?;
        let facts = reperio_engine::android::facts(&adb);
        let stem: String = facts
            .name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let folder = Path::new(&dir).join(format!("{stem}-all-partitions"));
        reperio_engine::android::image_all(
            &adb,
            &facts,
            &folder,
            even_if_encrypted,
            &mut |done, total, name| {
                let _ = app.emit(
                    "android-progress",
                    AndroidProgress {
                        done,
                        total,
                        name: name.to_string(),
                    },
                );
            },
            &cancel,
        )
    })
    .await
    .map_err(|e| e.to_string())?
}

#[derive(Serialize)]
struct AnalyzerInfo {
    name: String,
    file: String,
    description: String,
}

/// Forensic plug-ins that work on phones.
#[tauri::command]
async fn android_analyzers() -> Vec<AnalyzerInfo> {
    tauri::async_runtime::spawn_blocking(|| {
        reperio_engine::analyzer::runnable_on("android")
            .into_iter()
            .map(|p| AnalyzerInfo {
                name: p.name,
                file: p.path.display().to_string(),
                description: p.description,
            })
            .collect()
    })
    .await
    .unwrap_or_default()
}

/// Runs the chosen forensic plug-ins on the phone. Their reports, files,
/// and audit logs go in a new folder inside `dest`.
#[tauri::command]
async fn android_analyze(
    serial: String,
    dest: String,
    names: Vec<String>,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<Vec<reperio_engine::analyzer::Report>, String> {
    let cancel = state.android_cancel.clone();
    cancel.store(false, Ordering::Relaxed);
    tauri::async_runtime::spawn_blocking(move || {
        let adb = reperio_engine::android::Adb::connect(Some(&serial))?;
        let facts = reperio_engine::android::facts(&adb);
        let phone = reperio_engine::android::Phone { adb: &adb, facts };
        let target = reperio_engine::analyzer::Target {
            device: Some(&phone),
            image: None,
            backup: None,
        };
        let plugins: Vec<_> = reperio_engine::analyzer::runnable_on("android")
            .into_iter()
            .filter(|p| names.contains(&p.name))
            .collect();
        if plugins.is_empty() {
            return Err("No plug-in was chosen.".to_string());
        }
        let total = plugins.len() as u64;
        let mut reports = Vec::new();
        for (i, p) in plugins.iter().enumerate() {
            let name = p.name.clone();
            let report = reperio_engine::analyzer::run(
                p,
                &target,
                "android",
                Path::new(&dest),
                &reperio_engine::analyzer::RunOptions::default(),
                &mut |pr| {
                    let _ = app.emit(
                        "android-progress",
                        AndroidProgress {
                            done: i as u64 * 100
                                + (pr.done * 100).checked_div(pr.total).unwrap_or(0),
                            total: total * 100,
                            name: format!("{name}: {}", pr.text),
                        },
                    );
                },
                &cancel,
            );
            let stop = report.cancelled;
            reports.push(report);
            if stop {
                break;
            }
        }
        Ok(reports)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Runs forensic plug-ins on the opened disk or image, after a scan. Their
/// reports go in a new folder in the app's data folder.
#[tauri::command]
async fn analyze_source(
    names: Vec<String>,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<AnalyzeOutcome, String> {
    let src = source(&state)?;
    let first_id = state.items.lock().unwrap().len() as u32;
    let src_for_items = src.clone();
    let label = opened_name(&state);
    let cancel = state.android_cancel.clone();
    cancel.store(false, Ordering::Relaxed);
    reperio_engine::plugin::clear_stop();
    let dest = state
        .cache
        .join("analysis")
        .join(format!("{}-{}", names.len(), now_secs()));
    tauri::async_runtime::spawn_blocking(move || {
        let target = reperio_engine::analyzer::Target {
            device: None,
            image: Some(&*src),
            backup: None,
        };
        let mut reports = Vec::new();
        let mut items: Vec<Item> = Vec::new();
        for p in reperio_engine::analyzer::runnable_on("image")
            .into_iter()
            .filter(|p| names.contains(&p.name))
        {
            let name = p.name.clone();
            let report = reperio_engine::analyzer::run(
                &p,
                &target,
                "image",
                &dest,
                &reperio_engine::analyzer::RunOptions::default(),
                &mut |pr| {
                    let _ = app.emit(
                        "android-progress",
                        AndroidProgress {
                            done: pr.done,
                            total: pr.total,
                            name: format!("{name}: {}", pr.text),
                        },
                    );
                },
                &cancel,
            );
            // Files a plug-in offered become results, beside the ones Reperio found.
            if !report.found.is_empty() {
                items.extend(session::plugin_items(
                    &*src_for_items,
                    &report.found,
                    &name,
                    first_id + items.len() as u32,
                ));
            }
            reports.push(report);
        }
        let _ = label;
        Ok((reports, items))
    })
    .await
    .map_err(|e| e.to_string())?
    .map(|(reports, items)| {
        state.items.lock().unwrap().extend(items.iter().cloned());
        AnalyzeOutcome { reports, items }
    })
}

/// What running analyzers on the open drive or image gave: their reports, and
/// any recoverable files they offered, already added to the results.
#[derive(Serialize)]
struct AnalyzeOutcome {
    reports: Vec<reperio_engine::analyzer::Report>,
    items: Vec<Item>,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Runs a plug-in with no phone and no image (a tool), with the arguments
/// given, into a new folder inside `dest`.
#[tauri::command]
async fn run_tool(
    name: String,
    args: Vec<String>,
    dest: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<Vec<reperio_engine::analyzer::Report>, String> {
    let cancel = state.android_cancel.clone();
    cancel.store(false, Ordering::Relaxed);
    tauri::async_runtime::spawn_blocking(move || {
        let plugin = reperio_engine::analyzer::runnable_on("none")
            .into_iter()
            .find(|p| p.name == name)
            .ok_or("That plug-in is not installed any more.")?;
        let target = reperio_engine::analyzer::Target {
            device: None,
            image: None,
            backup: None,
        };
        let label = name.clone();
        let report = reperio_engine::analyzer::run(
            &plugin,
            &target,
            "none",
            Path::new(&dest),
            &reperio_engine::analyzer::RunOptions { args },
            &mut |pr| {
                let _ = app.emit(
                    "android-progress",
                    AndroidProgress {
                        done: pr.done,
                        total: pr.total,
                        name: format!("{label}: {}", pr.text),
                    },
                );
            },
            &cancel,
        );
        Ok(vec![report])
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
fn android_cancel(state: State<'_, AppState>) {
    state.android_cancel.store(true, Ordering::Relaxed);
    reperio_engine::plugin::request_stop();
}

#[tauri::command]
fn reveal(path: String) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let status = std::process::Command::new("open").arg(&path).status();
    #[cfg(not(target_os = "macos"))]
    let status = std::process::Command::new("xdg-open").arg(&path).status();
    status.map(|_| ()).map_err(|e| e.to_string())
}

#[tauri::command]
async fn eject(state: State<'_, AppState>) -> Result<(), String> {
    let id = {
        let mut opened = state.opened.lock().unwrap();
        let id = opened
            .as_ref()
            .and_then(|o| o.disk_id.clone())
            .ok_or("This is not a disk.")?;
        // Close the device (and the helper) before ejecting.
        *opened = None;
        id
    };
    disks::eject(&id)
}

#[tauri::command]
async fn save_results(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<Option<String>, String> {
    let Some(file) = app
        .dialog()
        .file()
        .set_title("Save scan results")
        .set_file_name("Reperio scan.reperio")
        .blocking_save_file()
    else {
        return Ok(None);
    };
    let path = file.into_path().map_err(|e| e.to_string())?;
    let (source, size) = {
        let o = state.opened.lock().unwrap();
        let o = o.as_ref().ok_or("Nothing is open.")?;
        (o.path.display().to_string(), o.source.len())
    };
    let result = ScanResult {
        source,
        source_len: size,
        volumes: state.volumes.lock().unwrap().clone(),
        items: state.items.lock().unwrap().clone(),
        bad_sectors: 0,
        slow_reads: 0,
        cancelled: false,
        free_space: *state.free_space.lock().unwrap(),
    };
    result.save(&path).map_err(|e| e.to_string())?;
    Ok(Some(path.display().to_string()))
}

#[derive(Serialize)]
struct Loaded {
    source: SourceInfo,
    items: Vec<Item>,
    volumes: Vec<VolumeInfo>,
    free_space: Option<FreeSpaceCheck>,
}

#[tauri::command]
async fn open_results(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<Option<Loaded>, String> {
    let Some(file) = app
        .dialog()
        .file()
        .set_title("Open saved scan results")
        .add_filter("Reperio scan", &["reperio", "json"])
        .blocking_pick_file()
    else {
        return Ok(None);
    };
    let path = file.into_path().map_err(|e| e.to_string())?;
    let result =
        ScanResult::load(&path).map_err(|e| format!("Could not read the saved scan: {e}"))?;
    let device = PathBuf::from(&result.source);
    let src = if result.source.starts_with("/dev/") {
        helper::open_device(&device, &state.cancel_open)
    } else {
        DeviceSource::open(&device).map(|s| Arc::new(s) as Arc<dyn Source>)
    }
    .map_err(|e| {
        format!(
            "The scan was of {}, which could not be opened: {e}. Connect it and try again.",
            result.source
        )
    })?;
    if src.len() != result.source_len {
        return Err(format!(
            "{} is not the same disk that was scanned (its size differs).",
            result.source
        ));
    }
    let disk_id = result
        .source
        .strip_prefix("/dev/r")
        .or(result.source.strip_prefix("/dev/"))
        .map(String::from);
    let label = path
        .file_stem()
        .map_or("Saved scan".into(), |s| s.to_string_lossy().into_owned());
    let info = reset(
        &state,
        Opened {
            source: src,
            label,
            path: device,
            disk_id,
            members: Vec::new(),
        },
    );
    *state.items.lock().unwrap() = result.items.clone();
    *state.volumes.lock().unwrap() = result.volumes.clone();
    *state.free_space.lock().unwrap() = result.free_space;
    Ok(Some(Loaded {
        source: info,
        items: result.items,
        volumes: result.volumes,
        free_space: result.free_space,
    }))
}

#[derive(Serialize, Clone)]
struct BackupProgress {
    /// Bytes settled: copied, or found unreadable sector by sector.
    done: u64,
    total: u64,
    /// Bytes copied to the image.
    finished: u64,
    /// Bytes in unreadable sectors.
    bad: u64,
    /// Bytes not read yet.
    untried: u64,
    /// Bytes in damaged areas still to be read sector by sector.
    pending: u64,
    pass: Pass,
    pass_number: u32,
    /// Where on the disk the copy is reading.
    pos: u64,
    /// Bytes read per second.
    rate: f64,
    /// Failed reads so far.
    errors: u64,
}

#[derive(Serialize, Clone, Default)]
struct BackupDone {
    path: String,
    map: String,
    /// Unreadable 512-byte sectors, left as zeros in the image.
    bad_sectors: u64,
    bad_bytes: u64,
    finished: u64,
    /// Bytes not read, after a stop: backing up to the same file resumes.
    unread: u64,
    cancelled: bool,
    /// Whether this run carried on from an earlier, stopped one.
    resumed: bool,
    /// `[offset, length]` of each unreadable area.
    bad_ranges: Vec<(u64, u64)>,
    /// With a forensic report: the image's hashes, the text report's path,
    /// and whether the drive read again matched the image.
    #[serde(skip_serializing_if = "Option::is_none")]
    hashes: Option<Hashes>,
    #[serde(skip_serializing_if = "Option::is_none")]
    report: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    verified: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// The optional forensic details from the backup sheet.
#[derive(serde::Deserialize, Default)]
struct ForensicRequest {
    #[serde(default)]
    case_number: String,
    #[serde(default)]
    examiner: String,
    #[serde(default)]
    notes: String,
    #[serde(default)]
    verify: bool,
}

impl ForensicRequest {
    fn options(self) -> ForensicOptions {
        let given = |s: String| Some(s.trim().to_string()).filter(|s| !s.is_empty());
        ForensicOptions {
            case: forensic::CaseInfo {
                case_number: given(self.case_number),
                examiner: given(self.examiner),
                evidence: None,
                notes: given(self.notes),
            },
            verify: self.verify,
            ..ForensicOptions::default()
        }
    }
}

/// Progress of hashing or verifying after a forensic backup.
#[derive(Serialize, Clone)]
struct BackupCheck {
    stage: forensic::Stage,
    done: u64,
    total: u64,
}

/// The map that records which parts of an image have been copied.
fn map_path(image: &Path) -> PathBuf {
    let mut m = image.as_os_str().to_owned();
    m.push(".map");
    PathBuf::from(m)
}

/// Copies the whole open disk to an image, healthy areas first, the way
/// ddrescue does. A map file next to the image records progress, so
/// backing up to the same file again resumes a stopped backup. With
/// `forensic`, the image is then hashed (and checked against the disk if
/// asked) and an acquisition report is written next to it.
#[tauri::command]
async fn backup(
    app: AppHandle,
    state: State<'_, AppState>,
    forensic: Option<ForensicRequest>,
) -> Result<Option<String>, String> {
    let src = source(&state)?;
    let source_path = state
        .opened
        .lock()
        .unwrap()
        .as_ref()
        .map(|o| o.path.clone())
        .unwrap_or_default();
    let Some(file) = app
        .dialog()
        .file()
        .set_title("Save a full copy of the disk")
        .set_file_name("Card backup.img")
        .blocking_save_file()
    else {
        return Ok(None);
    };
    let path = file.into_path().map_err(|e| e.to_string())?;
    if let Some(parent) = path.parent() {
        check_destination(&state, parent)?;
    }
    let map = map_path(&path);
    if let Some(o) = state.opened.lock().unwrap().as_ref() {
        if path == o.path || map == o.path || o.members.contains(&path) || o.members.contains(&map)
        {
            return Err("Choose a file other than the disk being backed up.".into());
        }
    }
    // Resume only a map that belongs with this image and fits this disk;
    // anything else is stale and the backup starts over.
    let resumed = path.exists() && Mapfile::load(&map, src.len()).is_ok();
    if !resumed {
        let _ = std::fs::remove_file(&map);
    }
    state.cancel.store(false, Ordering::Relaxed);
    let cancel = state.cancel.clone();
    let out_path = path.clone();
    std::thread::spawn(move || {
        let mut last = Instant::now() - Duration::from_secs(1);
        let opts = ImageOptions::default();
        let mut on_image = |p: &imaging::ImageProgress| {
            if last.elapsed() > Duration::from_millis(150) {
                last = Instant::now();
                let _ = app.emit(
                    "backup-progress",
                    BackupProgress {
                        done: p.done(),
                        total: p.total,
                        finished: p.finished,
                        bad: p.bad,
                        untried: p.non_tried,
                        pending: p.pending,
                        pass: p.pass,
                        pass_number: p.pass_number,
                        pos: p.pos,
                        rate: p.rate,
                        errors: p.errors,
                    },
                );
            }
        };
        let mut report = None;
        let result = match forensic {
            None => imaging::image(&*src, &out_path, &map, &opts, &cancel, on_image),
            Some(req) => forensic::acquire(
                &*src,
                &source_path,
                &out_path,
                &map,
                &opts,
                &req.options(),
                &cancel,
                |p| match p {
                    forensic::Progress::Imaging(p) => on_image(p),
                    forensic::Progress::Checking(c) => {
                        let _ = app.emit(
                            "backup-check",
                            BackupCheck {
                                stage: c.stage,
                                done: c.done,
                                total: c.total,
                            },
                        );
                    }
                },
            )
            .map(|a| {
                report = a.report.map(|r| (r, a.report_paths));
                a.outcome
            }),
        };
        let mut done = BackupDone {
            path: out_path.display().to_string(),
            map: map.display().to_string(),
            resumed,
            ..Default::default()
        };
        match result {
            Ok(out) => {
                done.bad_sectors = out.bad.div_ceil(opts.sector);
                done.bad_bytes = out.bad;
                done.finished = out.finished;
                done.unread = out.unread;
                done.cancelled = out.cancelled;
                done.bad_ranges = out.bad_ranges;
                if let Some((r, paths)) = report {
                    done.verified = r.verification.as_ref().map(|v| v.matched);
                    done.hashes = Some(r.hashes);
                    done.report = paths.map(|(_, txt)| txt.display().to_string());
                }
            }
            Err(e) => done.error = Some(e.to_string()),
        }
        let _ = app.emit("backup-done", done);
    });
    Ok(Some(path.display().to_string()))
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some(helper::HELPER_FLAG) {
        helper::run(&args[2..]);
    }
    // Run by the background task: refresh the Recovery Vault and exit.
    if args.get(1).map(String::as_str) == Some(vault::REFRESH_FLAG) {
        vault::refresh_all();
        return;
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let cache = app.path().app_cache_dir()?.join("previews");
            let _ = std::fs::remove_dir_all(&cache);
            std::fs::create_dir_all(&cache)?;
            app.manage(AppState {
                opened: Mutex::new(None),
                items: Mutex::new(Vec::new()),
                volumes: Mutex::new(Vec::new()),
                cancel: Arc::new(AtomicBool::new(false)),
                cancel_open: AtomicBool::new(false),
                thumbs: Mutex::new(HashMap::new()),
                sigs: Mutex::new(HashMap::new()),
                preview_cancel: Mutex::new(Arc::new(AtomicBool::new(false))),
                media: media::Media::start(cache.clone()),
                raid_members: Mutex::new(None),
                cache,
                ios: Mutex::new(None),
                ios_cancel: Arc::new(AtomicBool::new(false)),
                pool: Pool::new(4),
                free_space: Mutex::new(None),
                surface_cancel: Arc::new(AtomicBool::new(false)),
                surface_running: Arc::new(AtomicBool::new(false)),
                android_cancel: Arc::new(AtomicBool::new(false)),
            });
            // What plug-ins say, as they say it, for the plug-in activity view.
            let handle = app.handle().clone();
            reperio_engine::plugin::set_log_sink(Some(Arc::new(
                move |plugin: &str, line: &str| {
                    let _ = handle.emit(
                        "plugin-log",
                        PluginLog {
                            plugin: plugin.to_string(),
                            line: line.chars().take(500).collect(),
                        },
                    );
                },
            )));
            // How far a plug-in reading a volume has got, for the scan screen.
            let handle = app.handle().clone();
            reperio_engine::plugin::set_progress_sink(Some(Arc::new(
                move |p: &reperio_engine::plugin::PluginProgress| {
                    let _ = handle.emit("plugin-progress", p.clone());
                },
            )));
            // Keep the Recovery Vault fresh while the app runs.
            std::thread::spawn(|| loop {
                vault::refresh_all();
                std::thread::sleep(Duration::from_secs(15 * 60));
            });
            // Test hook: open an image file at launch.
            if let Ok(path) = std::env::var("REPERIO_OPEN_IMAGE") {
                let _ = open_image_at(PathBuf::from(path), &app.state::<AppState>());
            }
            Ok(())
        })
        .register_asynchronous_uri_scheme_protocol("thumb", |ctx, request, responder| {
            let app = ctx.app_handle().clone();
            let path = request.uri().path().trim_start_matches('/').to_string();
            let mut parts = path.split(['/', '?']);
            let first = parts.next().unwrap_or("").to_string();
            let ios_file = (first == "ios").then(|| parts.next().unwrap_or("").to_string());
            let id = first.parse().ok();
            let worker = app.clone();
            app.state::<AppState>().pool.run(move || {
                let app = worker;
                let state = app.state::<AppState>();
                let made = match &ios_file {
                    Some(file_id) => ios_thumbnail(&state, file_id).map(Arc::new),
                    None => id.and_then(|id| make_thumbnail(&state, id)),
                };
                let response = match made {
                    Some(bytes) => Response::builder()
                        .header("Content-Type", "image/jpeg")
                        .header("Cache-Control", "max-age=3600")
                        .body(bytes.to_vec()),
                    None => Response::builder()
                        .status(StatusCode::NOT_FOUND)
                        .body(Vec::new()),
                };
                responder.respond(response.unwrap());
            });
        })
        .invoke_handler(tauri::generate_handler![
            platform,
            plugin_devices,
            open_plugin_device,
            ios_status,
            ios_pair,
            ios_backup,
            ios_cancel,
            ios_open_backup,
            ios_photos,
            ios_messages,
            ios_fragments,
            ios_whatsapp,
            ios_whatsapp_deleted,
            ios_notes,
            ios_notes_deleted,
            ios_save_notes,
            ios_deleted_messages,
            ios_files,
            ios_analyze,
            ios_choose_folder,
            ios_export,
            scan_checkpoint,
            vault_drives,
            vault_protect,
            similar_groups,
            cancel_preview,
            item_bytes,
            list_disks,
            disk_health,
            drive_report,
            start_surface,
            cancel_surface,
            open_disk,
            open_image,
            open_image_path,
            detect_raid,
            open_raid,
            start_scan,
            cancel,
            cancel_open,
            open_full_disk_access,
            items,
            preview,
            choose_destination,
            recover,
            reveal,
            android_devices,
            android_facts,
            android_pull,
            android_image,
            android_image_all,
            android_cancel,
            run_tool,
            analyze_source,
            android_analyzers,
            android_analyze,
            plugins_info,
            plugin_set_options,
            plugin_set_limits,
            plugin_test,
            plugin_install,
            plugin_pick,
            plugin_remove,
            plugin_trust,
            open_plugins_folder,
            app_info,
            clear_saved_scans,
            eject,
            save_results,
            open_results,
            backup,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Reperio");
}

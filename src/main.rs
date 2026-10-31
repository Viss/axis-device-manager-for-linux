mod catalog;
mod credentials;
mod inventory;
mod logging;
mod vapix;
mod ws_discovery;

use catalog::FirmwareRelease;
use eframe::egui::{self, Color32, RichText};
use std::{collections::HashMap, sync::mpsc, thread};
use vapix::{DeviceInfo, UpgradeResult};

const APP_NAME: &str = "Axis Device Manager for Linux";
const MAX_PARALLEL_UPLOADS: usize = 2;

fn group_firmware_targets(
    targets: Vec<(String, FirmwareRelease)>,
) -> HashMap<String, (FirmwareRelease, Vec<String>)> {
    let mut groups = HashMap::new();
    for (address, release) in targets {
        groups
            .entry(release.url.clone())
            .or_insert_with(|| (release.clone(), Vec::new()))
            .1
            .push(address);
    }
    groups
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SortColumn {
    Address,
    Model,
    Installed,
    Available,
    Status,
}

fn version_is_newer(candidate: &str, installed: &str) -> bool {
    let parts = |value: &str| {
        value
            .split('.')
            .map(|part| part.parse::<u64>().unwrap_or(0))
            .collect::<Vec<_>>()
    };
    let mut candidate = parts(candidate);
    let mut installed = parts(installed);
    let length = candidate.len().max(installed.len());
    candidate.resize(length, 0);
    installed.resize(length, 0);
    candidate > installed
}

fn model_label(info: Option<&DeviceInfo>) -> String {
    let Some(info) = info else {
        return "Unknown".into();
    };
    let model = info
        .product_number
        .as_deref()
        .or(info.product_name.as_deref())
        .unwrap_or("Unknown")
        .trim();
    if model.to_ascii_uppercase().starts_with("AXIS ") {
        model.to_owned()
    } else {
        format!("AXIS {model}")
    }
}

fn status_badge(state: &str) -> (&'static str, Color32) {
    if state.starts_with("Update available") {
        ("UPDATE", Color32::from_rgb(255, 190, 80))
    } else if state.starts_with("No newer firmware") {
        ("CURRENT", Color32::from_rgb(105, 210, 145))
    } else if state.starts_with("Verified AXIS OS") {
        ("VERIFIED", Color32::from_rgb(105, 210, 145))
    } else if state.starts_with("Accepted") || state.contains("Waiting for camera") {
        ("RESTARTING", Color32::from_rgb(105, 180, 245))
    } else if state.starts_with("Queued") {
        ("QUEUED", Color32::GRAY)
    } else if state.contains("preflight") {
        ("PREFLIGHT", Color32::from_rgb(105, 180, 245))
    } else if state.starts_with("Offline:") {
        ("OFFLINE", Color32::from_rgb(255, 190, 80))
    } else if state.contains("failed") || state.starts_with("Catalog:") {
        ("ERROR", Color32::from_rgb(245, 105, 105))
    } else if state.contains("Refreshing")
        || state.contains("Inspect")
        || state.contains("Downloading")
        || state.contains("Sending")
    {
        ("CHECKING", Color32::from_rgb(105, 180, 245))
    } else {
        ("PENDING", Color32::GRAY)
    }
}

fn handle_window_resize(ctx: &egui::Context) {
    const EDGE: f32 = 14.0;
    const CORNER: f32 = 30.0;
    let (rect, pointer, pressed) = ctx.input(|input| {
        (
            input.screen_rect(),
            input.pointer.hover_pos(),
            input.pointer.primary_pressed(),
        )
    });
    let Some(pointer) = pointer else { return };

    let left = pointer.x <= rect.left() + EDGE;
    let right = pointer.x >= rect.right() - EDGE;
    let top = pointer.y <= rect.top() + EDGE;
    let bottom = pointer.y >= rect.bottom() - EDGE;
    let near_left = pointer.x <= rect.left() + CORNER;
    let near_right = pointer.x >= rect.right() - CORNER;
    let near_top = pointer.y <= rect.top() + CORNER;
    let near_bottom = pointer.y >= rect.bottom() - CORNER;

    let direction = if (left && near_top) || (top && near_left) {
        Some(egui::viewport::ResizeDirection::NorthWest)
    } else if (right && near_top) || (top && near_right) {
        Some(egui::viewport::ResizeDirection::NorthEast)
    } else if (left && near_bottom) || (bottom && near_left) {
        Some(egui::viewport::ResizeDirection::SouthWest)
    } else if (right && near_bottom) || (bottom && near_right) {
        Some(egui::viewport::ResizeDirection::SouthEast)
    } else if left {
        Some(egui::viewport::ResizeDirection::West)
    } else if right {
        Some(egui::viewport::ResizeDirection::East)
    } else if top {
        Some(egui::viewport::ResizeDirection::North)
    } else if bottom {
        Some(egui::viewport::ResizeDirection::South)
    } else {
        None
    };

    if let Some(direction) = direction {
        let cursor = match direction {
            egui::viewport::ResizeDirection::North | egui::viewport::ResizeDirection::South => {
                egui::CursorIcon::ResizeVertical
            }
            egui::viewport::ResizeDirection::East | egui::viewport::ResizeDirection::West => {
                egui::CursorIcon::ResizeHorizontal
            }
            egui::viewport::ResizeDirection::NorthEast
            | egui::viewport::ResizeDirection::SouthWest => egui::CursorIcon::ResizeNeSw,
            egui::viewport::ResizeDirection::NorthWest
            | egui::viewport::ResizeDirection::SouthEast => egui::CursorIcon::ResizeNwSe,
        };
        ctx.set_cursor_icon(cursor);
        if pressed {
            ctx.send_viewport_cmd(egui::ViewportCommand::BeginResize(direction));
        }
    }
}

#[derive(Clone)]
struct DeviceRow {
    address: String,
    info: Option<DeviceInfo>,
    selected: bool,
    state: String,
    releases: Vec<FirmwareRelease>,
    release_index: usize,
    progress: Option<f32>,
    last_seen: Option<u64>,
}

#[derive(Clone)]
struct RollbackPlan {
    address: String,
    active_version: String,
    inactive_version: String,
    pending_commit: Option<String>,
    time_to_rollback: Option<u64>,
    catalog_releases: Vec<CatalogRollbackOption>,
}

#[derive(Clone)]
struct CatalogRollbackOption {
    release: FirmwareRelease,
    cached: bool,
}

#[derive(Clone)]
struct CatalogInstallPlan {
    address: String,
    previous_version: String,
    release: FirmwareRelease,
}

enum Event {
    Discovered(Vec<String>),
    Inspected(String, Result<DeviceInfo, String>),
    Upgraded(String, Result<UpgradeResult, String>),
    Catalog(String, Result<Vec<FirmwareRelease>, String>),
    UpgradeProgress(String, String, Option<f32>),
    Verified(String, Result<DeviceInfo, String>),
    RollbackChecked(String, Result<vapix::FirmwareStatus, String>),
    RollbackRequested(RollbackPlan, Result<(), String>),
    RollbackVerified(RollbackPlan, Result<DeviceInfo, String>),
    CatalogInstallRequested(CatalogInstallPlan, Result<UpgradeResult, String>),
    CatalogInstallVerified(CatalogInstallPlan, Result<DeviceInfo, String>),
    AxisDownloadHost(Result<(), String>),
}

struct App {
    devices: Vec<DeviceRow>,
    address: String,
    username: String,
    password: String,
    upgrade_confirmation_open: bool,
    rollback_confirmation: Option<RollbackPlan>,
    rollback_catalog_selection: Option<usize>,
    remember_password: bool,
    busy: bool,
    pending_upgrades: usize,
    batch_total: usize,
    batch_verified: usize,
    batch_failed: usize,
    status: String,
    axis_download_status: String,
    catalog_cache: HashMap<String, Vec<FirmwareRelease>>,
    catalog_pending: HashMap<String, Vec<String>>,
    sort_column: SortColumn,
    sort_ascending: bool,
    tx: mpsc::Sender<Event>,
    rx: mpsc::Receiver<Event>,
}

impl Default for App {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        let saved_credentials = credentials::load().ok().flatten();
        let saved_devices = inventory::load().unwrap_or_else(|error| {
            logging::log("ERROR", &format!("loading inventory: {error}"));
            Vec::new()
        });
        let devices: Vec<_> = saved_devices
            .into_iter()
            .map(|saved| DeviceRow {
                address: saved.address,
                info: saved.info,
                selected: false,
                state: "Saved device; refresh queued…".into(),
                releases: Vec::new(),
                release_index: 0,
                progress: None,
                last_seen: saved.last_seen,
            })
            .collect();
        let mut app = Self {
            devices,
            address: String::new(),
            username: saved_credentials
                .as_ref()
                .map(|v| v.0.clone())
                .unwrap_or_else(|| "root".into()),
            password: saved_credentials
                .as_ref()
                .map(|v| v.1.clone())
                .unwrap_or_default(),
            upgrade_confirmation_open: false,
            rollback_confirmation: None,
            rollback_catalog_selection: None,
            remember_password: saved_credentials.is_some(),
            busy: false,
            pending_upgrades: 0,
            batch_total: 0,
            batch_verified: 0,
            batch_failed: 0,
            status: "Add a device by address or discover devices on the local network.".into(),
            axis_download_status: "Checking…".into(),
            catalog_cache: HashMap::new(),
            catalog_pending: HashMap::new(),
            sort_column: SortColumn::Address,
            sort_ascending: true,
            tx,
            rx,
        };
        let restored: Vec<_> = app
            .devices
            .iter()
            .map(|device| device.address.clone())
            .collect();
        if !restored.is_empty() {
            app.status = format!(
                "Loaded {} saved camera(s); refreshing inventory…",
                restored.len()
            );
            app.inspect(restored);
        }
        let connectivity_tx = app.tx.clone();
        thread::spawn(move || {
            let _ = connectivity_tx.send(Event::AxisDownloadHost(catalog::check_download_host()));
        });
        app
    }
}

impl App {
    fn normalized_address(input: &str) -> String {
        let s = input.trim().trim_end_matches('/');
        if s.starts_with("http://") || s.starts_with("https://") {
            s.to_owned()
        } else {
            format!("http://{s}")
        }
    }

    fn add_address(&mut self, raw: &str) {
        let address = Self::normalized_address(raw);
        let host = url::Url::parse(&address)
            .ok()
            .and_then(|url| url.host_str().map(str::to_owned));
        let already_present = self.devices.iter().any(|device| {
            device.address == address
                || host.as_deref()
                    == url::Url::parse(&device.address)
                        .ok()
                        .and_then(|url| url.host_str().map(str::to_owned))
                        .as_deref()
        });
        if address == "http://" || already_present {
            return;
        }
        self.devices.push(DeviceRow {
            address,
            info: None,
            selected: false,
            state: "Not inspected".into(),
            releases: vec![],
            release_index: 0,
            progress: None,
            last_seen: None,
        });
        self.save_inventory();
    }

    fn save_inventory(&self) {
        let devices: Vec<_> = self
            .devices
            .iter()
            .map(|device| inventory::SavedDevice {
                address: device.address.clone(),
                info: device.info.clone(),
                last_seen: device.last_seen,
            })
            .collect();
        if let Err(error) = inventory::save(&devices) {
            logging::log("ERROR", &format!("saving inventory: {error}"));
        }
    }

    fn inspect(&mut self, addresses: Vec<String>) {
        if addresses.is_empty() {
            return;
        }
        self.busy = true;
        self.status = format!("Inspecting {} device(s)…", addresses.len());
        for address in addresses {
            let tx = self.tx.clone();
            let user = self.username.clone();
            let pass = self.password.clone();
            thread::spawn(move || {
                let _ = tx.send(Event::Inspected(
                    address.clone(),
                    vapix::inspect(&address, &user, &pass),
                ));
            });
        }
    }

    fn save_credentials(&mut self) {
        let result = if self.remember_password {
            credentials::save(&self.username, &self.password)
        } else {
            credentials::clear()
        };
        match result {
            Ok(()) => {
                self.status = if self.remember_password {
                    "Credentials saved in the desktop keyring."
                } else {
                    "Saved credentials removed."
                }
                .into()
            }
            Err(error) => {
                logging::log("ERROR", &format!("keyring: {error}"));
                self.status = format!("Could not use the desktop keyring: {error}");
            }
        }
    }

    /// Returns true when this address is waiting for an asynchronous catalog result.
    fn queue_catalog(&mut self, address: String, model: String) -> bool {
        let key = model.trim().to_ascii_uppercase();
        if let Some(releases) = self.catalog_cache.get(&key).cloned() {
            self.apply_catalog(&address, Ok(releases));
            return false;
        }
        let pending = self.catalog_pending.entry(key.clone()).or_default();
        pending.push(address);
        if pending.len() > 1 {
            return true;
        }
        let tx = self.tx.clone();
        thread::spawn(move || {
            let _ = tx.send(Event::Catalog(key, catalog::fetch(&model)));
        });
        true
    }

    fn apply_catalog(&mut self, address: &str, result: Result<Vec<FirmwareRelease>, String>) {
        if let Some(row) = self.devices.iter_mut().find(|d| d.address == address) {
            match result {
                Ok(releases) => {
                    let installed = row
                        .info
                        .as_ref()
                        .and_then(|i| i.firmware_version.as_deref())
                        .unwrap_or("");
                    let releases: Vec<_> = releases
                        .into_iter()
                        .filter(|release| version_is_newer(&release.version, installed))
                        .collect();
                    let newer = releases
                        .iter()
                        .enumerate()
                        .find(|(_, release)| version_is_newer(&release.version, installed));
                    row.state = if let Some((index, release)) = newer {
                        row.release_index = index;
                        format!("Update available: {installed} -> {}", release.version)
                    } else {
                        format!("No newer catalog release (installed {installed})")
                    };
                    row.releases = releases;
                }
                Err(error) => {
                    logging::log("ERROR", &format!("catalog {address}: {error}"));
                    row.state = format!("Catalog: {error}");
                }
            }
        }
    }

    fn check_catalog(&mut self) {
        let targets: Vec<_> = self
            .devices
            .iter()
            .filter_map(|d| {
                let info = d.info.as_ref()?;
                Some((
                    d.address.clone(),
                    info.product_number
                        .clone()
                        .or_else(|| info.product_name.clone())?,
                ))
            })
            .collect();
        if targets.is_empty() {
            self.status = "No inventoried devices are available yet.".into();
            return;
        }
        self.busy = true;
        self.status = "Checking the official Axis software catalog…".into();
        for row in &mut self.devices {
            if row.info.is_some() {
                row.state = "Refreshing Axis catalog…".into();
            }
        }
        for (address, model) in targets {
            let _ = self.queue_catalog(address, model);
        }
    }

    fn set_sort(&mut self, column: SortColumn) {
        if self.sort_column == column {
            self.sort_ascending = !self.sort_ascending;
        } else {
            self.sort_column = column;
            self.sort_ascending = true;
        }
        let column = self.sort_column;
        let ascending = self.sort_ascending;
        self.devices.sort_by(|a, b| {
            let order = match column {
                SortColumn::Address => a.address.cmp(&b.address),
                SortColumn::Model => a
                    .info
                    .as_ref()
                    .and_then(|i| i.product_name.as_ref())
                    .cmp(&b.info.as_ref().and_then(|i| i.product_name.as_ref())),
                SortColumn::Installed => a
                    .info
                    .as_ref()
                    .and_then(|i| i.firmware_version.as_ref())
                    .cmp(&b.info.as_ref().and_then(|i| i.firmware_version.as_ref())),
                SortColumn::Available => a
                    .releases
                    .get(a.release_index)
                    .map(|r| &r.version)
                    .cmp(&b.releases.get(b.release_index).map(|r| &r.version)),
                SortColumn::Status => a.state.cmp(&b.state),
            };
            if ascending {
                order
            } else {
                order.reverse()
            }
        });
    }

    fn start_upgrade(&mut self) {
        let targets: Vec<_> = self
            .devices
            .iter()
            .filter(|d| d.selected)
            .map(|d| (d.address.clone(), d.releases.get(d.release_index).cloned()))
            .collect();
        if targets.is_empty() {
            self.status = "Select at least one device.".into();
            return;
        }
        if targets.iter().any(|(_, release)| release.is_none()) {
            self.status =
                "Every selected camera must have a newer, exact-model catalog release.".into();
            return;
        }
        if targets.iter().any(|(address, release)| {
            let installed = self
                .devices
                .iter()
                .find(|row| &row.address == address)
                .and_then(|row| row.info.as_ref())
                .and_then(|info| info.firmware_version.as_deref())
                .unwrap_or("");
            release
                .as_ref()
                .is_none_or(|release| !version_is_newer(&release.version, installed))
        }) {
            self.status =
                "Upgrade blocked: every target must be newer than the installed AXIS OS.".into();
            return;
        }
        self.busy = true;
        self.pending_upgrades = targets.len();
        self.batch_total = targets.len();
        self.batch_verified = 0;
        self.batch_failed = 0;
        self.status = format!(
            "Preparing firmware for {} device(s). Keep them powered and connected.",
            targets.len()
        );
        for (address, _) in &targets {
            if let Some(row) = self.devices.iter_mut().find(|d| &d.address == address) {
                row.state = "Queued for firmware safety preflight…".into();
                row.progress = None;
            }
        }
        let tx = self.tx.clone();
        let user = self.username.clone();
        let pass = self.password.clone();
        thread::spawn(move || {
            let mut safe_targets = Vec::new();
            for (address, release) in targets {
                let release = release.expect("catalog release was validated");
                let _ = tx.send(Event::UpgradeProgress(
                    address.clone(),
                    "Running firmware safety preflight…".into(),
                    None,
                ));
                match vapix::firmware_status(&address, &user, &pass) {
                    Ok(status) => {
                        logging::log(
                            "INFO",
                            &format!(
                                "firmware preflight {address}: active={:?}, committed={:?}",
                                status.active_version, status.committed
                            ),
                        );
                        if status.committed == Some(false) {
                            let _ = tx.send(Event::Upgraded(
                                address,
                                Err("Camera firmware is uncommitted; upgrade blocked to preserve rollback safety".into()),
                            ));
                        } else if status
                            .active_version
                            .as_deref()
                            .is_some_and(|active| !version_is_newer(&release.version, active))
                        {
                            let _ = tx.send(Event::Upgraded(
                                address,
                                Err(format!(
                                    "Upgrade blocked: target {} is not newer than active AXIS OS {}",
                                    release.version,
                                    status.active_version.as_deref().unwrap_or("unknown")
                                )),
                            ));
                        } else {
                            safe_targets.push((address, release));
                        }
                    }
                    Err(error) => {
                        let _ = tx.send(Event::Upgraded(address, Err(error)));
                    }
                }
            }

            let releases = group_firmware_targets(safe_targets);
            let mut upload_jobs = Vec::new();
            for (_, (release, addresses)) in releases {
                for address in &addresses {
                    let _ = tx.send(Event::UpgradeProgress(
                        address.clone(),
                        format!("Queued to download AXIS OS {}…", release.version),
                        Some(0.0),
                    ));
                }
                let progress_tx = tx.clone();
                let progress_addresses = addresses.clone();
                let release_version = release.version.clone();
                let downloaded =
                    catalog::download_with_progress(&release, move |downloaded, total| {
                        let fraction = total
                            .filter(|total| *total > 0)
                            .map(|total| downloaded as f32 / total as f32);
                        let message = match total {
                            Some(total) => format!(
                                "Downloading firmware from Axis ({}): {} / {} MB",
                                release_version,
                                downloaded / 1_048_576,
                                total / 1_048_576
                            ),
                            None => format!(
                                "Downloading firmware from Axis ({}): {} MB",
                                release_version,
                                downloaded / 1_048_576
                            ),
                        };
                        for address in &progress_addresses {
                            let _ = progress_tx.send(Event::UpgradeProgress(
                                address.clone(),
                                message.clone(),
                                fraction,
                            ));
                        }
                    });
                match downloaded {
                    Ok(path) => {
                        for address in addresses {
                            let _ = tx.send(Event::UpgradeProgress(
                                address.clone(),
                                "Verified firmware cached; queued to send to camera…".into(),
                                Some(0.0),
                            ));
                            upload_jobs.push((address, path.clone()));
                        }
                    }
                    Err(error) => {
                        for address in addresses {
                            let _ = tx.send(Event::Upgraded(address, Err(error.clone())));
                        }
                    }
                }
            }

            for pair in upload_jobs.chunks(MAX_PARALLEL_UPLOADS) {
                let handles: Vec<_> = pair
                    .iter()
                    .cloned()
                    .map(|(address, path)| {
                        let tx = tx.clone();
                        let user = user.clone();
                        let pass = pass.clone();
                        thread::spawn(move || {
                            let upload_tx = tx.clone();
                            let upload_address = address.clone();
                            let result = vapix::upgrade(
                                &address,
                                &user,
                                &pass,
                                &path,
                                move |sent, total| {
                                    let fraction =
                                        (total > 0).then_some(sent as f32 / total as f32);
                                    let _ = upload_tx.send(Event::UpgradeProgress(
                                        upload_address.clone(),
                                        format!(
                                            "Sending firmware to camera: {} / {} MB",
                                            sent / 1_048_576,
                                            total / 1_048_576
                                        ),
                                        fraction,
                                    ));
                                },
                            );
                            let _ = tx.send(Event::Upgraded(address, result));
                        })
                    })
                    .collect();
                for handle in handles {
                    let _ = handle.join();
                }
            }
        });
    }

    fn check_rollback(&mut self, address: String) {
        if self.busy {
            self.status = "Wait for the active firmware operation to finish.".into();
            return;
        }
        if let Some(row) = self.devices.iter_mut().find(|row| row.address == address) {
            row.state = "Checking rollback availability…".into();
        }
        self.status = format!("Checking rollback availability for {address}…");
        let tx = self.tx.clone();
        let user = self.username.clone();
        let pass = self.password.clone();
        thread::spawn(move || {
            let result = vapix::firmware_status(&address, &user, &pass);
            let _ = tx.send(Event::RollbackChecked(address, result));
        });
    }

    fn start_rollback(&mut self, plan: RollbackPlan) {
        self.busy = true;
        if let Some(row) = self
            .devices
            .iter_mut()
            .find(|row| row.address == plan.address)
        {
            row.state = format!(
                "Requesting rollback: {} → {}…",
                plan.active_version, plan.inactive_version
            );
            row.progress = None;
        }
        self.status = format!("Requesting firmware rollback for {}…", plan.address);
        let tx = self.tx.clone();
        let user = self.username.clone();
        let pass = self.password.clone();
        thread::spawn(move || {
            let result = vapix::rollback(&plan.address, &user, &pass);
            let _ = tx.send(Event::RollbackRequested(plan, result));
        });
    }

    fn verify_rollback(&mut self, plan: RollbackPlan, uncertain: Option<String>) {
        if let Some(row) = self
            .devices
            .iter_mut()
            .find(|row| row.address == plan.address)
        {
            row.state = match uncertain {
                Some(error) => {
                    format!("Rollback response lost ({error}); waiting for camera and verifying…")
                }
                None => "Rollback accepted; waiting for camera to restart…".into(),
            };
        }
        let tx = self.tx.clone();
        let user = self.username.clone();
        let pass = self.password.clone();
        thread::spawn(move || {
            let mut last_error = "camera has not returned yet".to_owned();
            for _ in 0..36 {
                thread::sleep(std::time::Duration::from_secs(5));
                match vapix::inspect(&plan.address, &user, &pass) {
                    Ok(info) => {
                        let _ = tx.send(Event::RollbackVerified(plan, Ok(info)));
                        return;
                    }
                    Err(error) => last_error = error,
                }
            }
            let _ = tx.send(Event::RollbackVerified(
                plan,
                Err(format!(
                    "Camera did not return within 3 minutes: {last_error}"
                )),
            ));
        });
    }

    fn start_catalog_install(&mut self, plan: CatalogInstallPlan) {
        if self.busy {
            self.status = "Wait for the active firmware operation to finish.".into();
            return;
        }
        if plan.release.version == plan.previous_version {
            self.status = "Selected catalog version is already installed.".into();
            return;
        }
        self.busy = true;
        if let Some(row) = self
            .devices
            .iter_mut()
            .find(|row| row.address == plan.address)
        {
            row.state = format!(
                "Checking catalog install {} → {}…",
                plan.previous_version, plan.release.version
            );
            row.progress = None;
        }
        let tx = self.tx.clone();
        let user = self.username.clone();
        let pass = self.password.clone();
        thread::spawn(move || {
            match vapix::firmware_status(&plan.address, &user, &pass) {
                Ok(status) if status.committed == Some(false) => {
                    let _ = tx.send(Event::CatalogInstallRequested(
                        plan,
                        Err("Camera firmware is uncommitted; catalog install blocked to preserve rollback safety".into()),
                    ));
                    return;
                }
                Err(error) => {
                    let _ = tx.send(Event::CatalogInstallRequested(
                        plan,
                        Err(format!("Firmware preflight failed: {error}")),
                    ));
                    return;
                }
                _ => {}
            }
            let progress_tx = tx.clone();
            let progress_address = plan.address.clone();
            let progress_version = plan.release.version.clone();
            let path = match catalog::download_with_progress(&plan.release, move |done, total| {
                let fraction = total
                    .filter(|total| *total > 0)
                    .map(|total| done as f32 / total as f32);
                let message = match total {
                    Some(total) => format!(
                        "Downloading AXIS OS {} from Axis: {} / {} MB",
                        progress_version,
                        done / 1_048_576,
                        total / 1_048_576
                    ),
                    None => format!(
                        "Downloading AXIS OS {} from Axis: {} MB",
                        progress_version,
                        done / 1_048_576
                    ),
                };
                let _ = progress_tx.send(Event::UpgradeProgress(
                    progress_address.clone(),
                    message,
                    fraction,
                ));
            }) {
                Ok(path) => path,
                Err(error) => {
                    let _ = tx.send(Event::CatalogInstallRequested(
                        plan,
                        Err(format!("Catalog download failed: {error}")),
                    ));
                    return;
                }
            };
            let upload_tx = tx.clone();
            let upload_address = plan.address.clone();
            let result = vapix::upgrade(&plan.address, &user, &pass, &path, move |sent, total| {
                let fraction = (total > 0).then_some(sent as f32 / total as f32);
                let _ = upload_tx.send(Event::UpgradeProgress(
                    upload_address.clone(),
                    format!(
                        "Sending selected firmware to camera: {} / {} MB",
                        sent / 1_048_576,
                        total / 1_048_576
                    ),
                    fraction,
                ));
            });
            let _ = tx.send(Event::CatalogInstallRequested(plan, result));
        });
    }

    fn verify_catalog_install(&mut self, plan: CatalogInstallPlan, uncertain: Option<String>) {
        if let Some(row) = self
            .devices
            .iter_mut()
            .find(|row| row.address == plan.address)
        {
            row.progress = None;
            row.state = match uncertain {
                Some(error) => {
                    format!("Firmware response lost ({error}); waiting for camera and verifying…")
                }
                None => "Firmware accepted; waiting for camera to restart…".into(),
            };
        }
        let tx = self.tx.clone();
        let user = self.username.clone();
        let pass = self.password.clone();
        thread::spawn(move || {
            let mut last_error = "camera has not returned yet".to_owned();
            for _ in 0..36 {
                thread::sleep(std::time::Duration::from_secs(5));
                match vapix::inspect(&plan.address, &user, &pass) {
                    Ok(info) => {
                        let _ = tx.send(Event::CatalogInstallVerified(plan, Ok(info)));
                        return;
                    }
                    Err(error) => last_error = error,
                }
            }
            let _ = tx.send(Event::CatalogInstallVerified(
                plan,
                Err(format!(
                    "Camera did not return within 3 minutes: {last_error}"
                )),
            ));
        });
    }

    fn poll(&mut self) {
        while let Ok(event) = self.rx.try_recv() {
            match event {
                Event::Discovered(addresses) => {
                    let discovered_count = addresses.len();
                    for a in &addresses {
                        self.add_address(&a);
                    }
                    let inventory_targets: Vec<_> = self
                        .devices
                        .iter()
                        .map(|device| device.address.clone())
                        .collect();
                    let count = inventory_targets.len();
                    logging::log(
                        "INFO",
                        &format!(
                            "discovery returned {discovered_count} endpoint(s), {count} unique saved camera(s)"
                        ),
                    );
                    self.status = format!("Found {count} Axis device(s); reading inventory…");
                    self.inspect(inventory_targets);
                }
                Event::Inspected(address, result) => {
                    let mut catalog_model = None;
                    if let Some(row) = self.devices.iter_mut().find(|d| d.address == address) {
                        match result {
                            Ok(info) => {
                                catalog_model = info
                                    .product_number
                                    .clone()
                                    .or_else(|| info.product_name.clone());
                                row.state = "Checking Axis catalog…".into();
                                row.info = Some(info);
                                row.last_seen = Some(
                                    std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .unwrap_or_default()
                                        .as_secs(),
                                );
                            }
                            Err(e) => row.state = format!("Offline: {e}"),
                        }
                    }
                    self.save_inventory();
                    if let Some(model) = catalog_model {
                        if self.queue_catalog(address, model) {
                            self.status =
                                "Inventory read; checking the Axis catalog automatically…".into();
                        } else {
                            // A cached catalog result is applied synchronously, so no
                            // Catalog event will arrive later to release the UI lock.
                            self.busy = false;
                            self.status = "Camera inventory and catalog status refreshed.".into();
                        }
                    } else {
                        self.busy = false;
                        self.status = "Inspection finished.".into();
                    }
                }
                Event::Upgraded(address, result) => {
                    if let Some(row) = self.devices.iter_mut().find(|d| d.address == address) {
                        row.progress = None;
                        match result {
                            Ok(r) => {
                                row.state = format!(
                                    "Accepted {}. Waiting for camera to restart…",
                                    r.firmware_version
                                        .unwrap_or_else(|| "device rebooting".into())
                                );
                                let tx = self.tx.clone();
                                let user = self.username.clone();
                                let pass = self.password.clone();
                                let verify_address = address.clone();
                                thread::spawn(move || {
                                    let mut last_error = "camera has not returned yet".to_owned();
                                    for _ in 0..36 {
                                        thread::sleep(std::time::Duration::from_secs(5));
                                        match vapix::inspect(&verify_address, &user, &pass) {
                                            Ok(info) => {
                                                let _ = tx.send(Event::Verified(
                                                    verify_address.clone(),
                                                    Ok(info),
                                                ));
                                                return;
                                            }
                                            Err(error) => last_error = error,
                                        }
                                    }
                                    let _ = tx.send(Event::Verified(
                                        verify_address,
                                        Err(format!(
                                            "Camera did not return within 3 minutes: {last_error}"
                                        )),
                                    ));
                                });
                            }
                            Err(e) => {
                                logging::log("ERROR", &format!("firmware upgrade {address}: {e}"));
                                row.state = format!("Upgrade failed: {e}");
                                self.status = format!("Upgrade failed for {address}: {e}");
                                self.batch_failed += 1;
                            }
                        }
                    }
                    let failed = self
                        .devices
                        .iter()
                        .find(|row| row.address == address)
                        .map(|row| row.state.starts_with("Upgrade failed:"))
                        .unwrap_or(true);
                    if failed {
                        self.pending_upgrades = self.pending_upgrades.saturating_sub(1);
                    }
                    if self.pending_upgrades == 0 {
                        self.busy = false;
                        self.status = format!(
                            "Batch complete: {} verified, {} failed, {} total.",
                            self.batch_verified, self.batch_failed, self.batch_total
                        );
                    } else {
                        self.status = format!(
                            "Batch in progress: {} verified, {} failed, {} remaining.",
                            self.batch_verified, self.batch_failed, self.pending_upgrades
                        );
                    }
                }
                Event::Catalog(model, result) => {
                    let addresses = self.catalog_pending.remove(&model).unwrap_or_default();
                    if let Ok(releases) = &result {
                        self.catalog_cache.insert(model.clone(), releases.clone());
                    }
                    for address in addresses {
                        self.apply_catalog(&address, result.clone());
                    }
                    self.busy = false;
                    self.status = "Inventory and automatic Axis catalog checks completed.".into();
                }
                Event::UpgradeProgress(address, message, progress) => {
                    if let Some(row) = self.devices.iter_mut().find(|d| d.address == address) {
                        row.state = message.clone();
                        row.progress = progress;
                    }
                    self.status = format!("{address}: {message}");
                }
                Event::Verified(address, result) => {
                    let mut verified = false;
                    if let Some(row) = self.devices.iter_mut().find(|d| d.address == address) {
                        row.progress = None;
                        match result {
                            Ok(info) => {
                                let installed = info.firmware_version.clone().unwrap_or_default();
                                let expected = row
                                    .releases
                                    .get(row.release_index)
                                    .map(|release| release.version.as_str())
                                    .unwrap_or("");
                                if installed == expected {
                                    row.state = format!("Verified AXIS OS {installed}");
                                    row.info = Some(info);
                                    row.selected = false;
                                    row.releases.retain(|release| {
                                        version_is_newer(&release.version, &installed)
                                    });
                                    row.release_index = 0;
                                    row.last_seen = Some(
                                        std::time::SystemTime::now()
                                            .duration_since(std::time::UNIX_EPOCH)
                                            .unwrap_or_default()
                                            .as_secs(),
                                    );
                                    verified = true;
                                } else {
                                    row.state = format!(
                                        "Upgrade failed: camera returned with {installed}; expected {expected}"
                                    );
                                }
                            }
                            Err(error) => row.state = format!("Upgrade failed: {error}"),
                        }
                    }
                    self.save_inventory();
                    if verified {
                        self.batch_verified += 1;
                    } else {
                        self.batch_failed += 1;
                    }
                    self.pending_upgrades = self.pending_upgrades.saturating_sub(1);
                    if self.pending_upgrades == 0 {
                        self.busy = false;
                        self.status = format!(
                            "Batch complete: {} verified, {} failed, {} total.",
                            self.batch_verified, self.batch_failed, self.batch_total
                        );
                    } else {
                        self.status = format!(
                            "Batch in progress: {} verified, {} failed, {} remaining.",
                            self.batch_verified, self.batch_failed, self.pending_upgrades
                        );
                    }
                }
                Event::RollbackChecked(address, result) => match result {
                    Ok(firmware) => {
                        let active = firmware.active_version.unwrap_or_else(|| "Unknown".into());
                        let inactive = firmware.inactive_version.unwrap_or_default();
                        let catalog_releases = self
                            .devices
                            .iter()
                            .find(|row| row.address == address)
                            .and_then(|row| row.info.as_ref())
                            .and_then(|info| {
                                info.product_number.as_ref().or(info.product_name.as_ref())
                            })
                            .and_then(|model| {
                                self.catalog_cache.get(&model.trim().to_ascii_uppercase())
                            })
                            .map(|releases| {
                                releases
                                    .iter()
                                    .map(|release| CatalogRollbackOption {
                                        release: release.clone(),
                                        cached: catalog::is_cached_verified(release),
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                        if let Some(row) =
                            self.devices.iter_mut().find(|row| row.address == address)
                        {
                            row.state = if inactive.is_empty() {
                                "Rollback unavailable: no inactive firmware".into()
                            } else {
                                format!("Rollback available: {active} → {inactive}")
                            };
                        }
                        self.status = if inactive.is_empty() {
                            format!("No on-camera rollback image for {address}; showing catalog and cache availability.")
                        } else {
                            format!("Rollback is available for {address}: {active} → {inactive}.")
                        };
                        self.rollback_confirmation = Some(RollbackPlan {
                            address: address.clone(),
                            active_version: active,
                            inactive_version: inactive,
                            pending_commit: firmware.pending_commit,
                            time_to_rollback: firmware.time_to_rollback,
                            catalog_releases,
                        });
                        self.rollback_catalog_selection = None;
                    }
                    Err(error) => {
                        logging::log("ERROR", &format!("rollback check {address}: {error}"));
                        if let Some(row) =
                            self.devices.iter_mut().find(|row| row.address == address)
                        {
                            row.state = format!("Rollback check failed: {error}");
                        }
                        self.status = format!("Could not check rollback for {address}: {error}");
                    }
                },
                Event::RollbackRequested(plan, result) => match result {
                    Ok(()) => self.verify_rollback(plan, None),
                    Err(error)
                        if error.starts_with("Rollback rejected:")
                            || error.starts_with("Rollback failed with HTTP") =>
                    {
                        logging::log(
                            "ERROR",
                            &format!("firmware rollback {}: {error}", plan.address),
                        );
                        if let Some(row) = self
                            .devices
                            .iter_mut()
                            .find(|row| row.address == plan.address)
                        {
                            row.state = format!("Rollback rejected: {error}");
                        }
                        self.busy = false;
                        self.status = format!("Rollback rejected for {}: {error}", plan.address);
                    }
                    Err(error) => {
                        logging::log(
                            "WARN",
                            &format!(
                                "rollback response uncertain for {}; verifying: {error}",
                                plan.address
                            ),
                        );
                        self.verify_rollback(plan, Some(error));
                    }
                },
                Event::RollbackVerified(plan, result) => {
                    self.busy = false;
                    match result {
                        Ok(info) => {
                            let installed = info.firmware_version.clone().unwrap_or_default();
                            if installed == plan.inactive_version {
                                if let Some(row) = self
                                    .devices
                                    .iter_mut()
                                    .find(|row| row.address == plan.address)
                                {
                                    row.info = Some(info);
                                    row.selected = false;
                                    row.releases.clear();
                                    row.release_index = 0;
                                    row.state = format!(
                                        "Rollback verified: {} → {}",
                                        plan.active_version, installed
                                    );
                                    row.last_seen = Some(
                                        std::time::SystemTime::now()
                                            .duration_since(std::time::UNIX_EPOCH)
                                            .unwrap_or_default()
                                            .as_secs(),
                                    );
                                }
                                self.save_inventory();
                                self.status = format!(
                                    "Rollback verified for {}: now running {}. Refresh catalog before upgrading again.",
                                    plan.address, installed
                                );
                            } else {
                                if let Some(row) = self
                                    .devices
                                    .iter_mut()
                                    .find(|row| row.address == plan.address)
                                {
                                    row.state = format!(
                                        "Rollback not verified: running {installed}, expected {}",
                                        plan.inactive_version
                                    );
                                }
                                self.status = format!(
                                    "Rollback could not be verified for {}: running {installed}, expected {}.",
                                    plan.address, plan.inactive_version
                                );
                            }
                        }
                        Err(error) => {
                            if let Some(row) = self
                                .devices
                                .iter_mut()
                                .find(|row| row.address == plan.address)
                            {
                                row.state = format!("Rollback verification failed: {error}");
                            }
                            self.status = format!(
                                "Rollback verification failed for {}: {error}",
                                plan.address
                            );
                        }
                    }
                }
                Event::CatalogInstallRequested(plan, result) => match result {
                    Ok(_) => self.verify_catalog_install(plan, None),
                    Err(error)
                        if error.starts_with('{')
                            || error.starts_with("HTTP ")
                            || error.starts_with("Firmware preflight failed:")
                            || error.starts_with("Camera firmware is uncommitted;")
                            || error.starts_with("Catalog download failed:") =>
                    {
                        logging::log(
                            "ERROR",
                            &format!("catalog firmware install {}: {error}", plan.address),
                        );
                        if let Some(row) = self
                            .devices
                            .iter_mut()
                            .find(|row| row.address == plan.address)
                        {
                            row.progress = None;
                            row.state = if error.contains("\"code\":409") {
                                "Downgrade refused: camera requires factory-default settings; no reset was performed".into()
                            } else {
                                format!("Catalog firmware install failed: {error}")
                            };
                        }
                        self.busy = false;
                        self.status = if error.contains("\"code\":409") {
                            format!("Downgrade blocked by {} because it requires a factory reset. {} preserved all settings.", plan.address, APP_NAME)
                        } else {
                            format!(
                                "Catalog firmware install failed for {}: {error}",
                                plan.address
                            )
                        };
                    }
                    Err(error) => self.verify_catalog_install(plan, Some(error)),
                },
                Event::CatalogInstallVerified(plan, result) => {
                    self.busy = false;
                    match result {
                        Ok(info) => {
                            let installed = info.firmware_version.clone().unwrap_or_default();
                            if installed == plan.release.version {
                                if let Some(row) = self
                                    .devices
                                    .iter_mut()
                                    .find(|row| row.address == plan.address)
                                {
                                    row.info = Some(info);
                                    row.selected = false;
                                    row.releases.clear();
                                    row.release_index = 0;
                                    row.progress = None;
                                    row.state = format!(
                                        "Catalog firmware verified: {} → {}",
                                        plan.previous_version, installed
                                    );
                                    row.last_seen = Some(
                                        std::time::SystemTime::now()
                                            .duration_since(std::time::UNIX_EPOCH)
                                            .unwrap_or_default()
                                            .as_secs(),
                                    );
                                }
                                self.save_inventory();
                                self.status = format!(
                                    "Catalog firmware install verified for {}: now running {}.",
                                    plan.address, installed
                                );
                            } else {
                                if let Some(row) = self
                                    .devices
                                    .iter_mut()
                                    .find(|row| row.address == plan.address)
                                {
                                    row.progress = None;
                                    row.state = format!(
                                        "Catalog install not verified: running {installed}, expected {}",
                                        plan.release.version
                                    );
                                }
                                self.status = format!(
                                    "Catalog install not verified for {}: running {installed}, expected {}.",
                                    plan.address, plan.release.version
                                );
                            }
                        }
                        Err(error) => {
                            if let Some(row) = self
                                .devices
                                .iter_mut()
                                .find(|row| row.address == plan.address)
                            {
                                row.progress = None;
                                row.state = format!("Catalog install verification failed: {error}");
                            }
                            self.status = format!(
                                "Catalog install verification failed for {}: {error}",
                                plan.address
                            );
                        }
                    }
                }
                Event::AxisDownloadHost(result) => {
                    self.axis_download_status = match result {
                        Ok(()) => "Reachable".into(),
                        Err(error) => {
                            logging::log(
                                "WARN",
                                &format!("Axis firmware host connectivity: {error}"),
                            );
                            "Unavailable".into()
                        }
                    };
                }
            }
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        logging::set_camera_password(&self.password);
        self.poll();
        handle_window_resize(ctx);
        ctx.request_repaint_after(std::time::Duration::from_millis(250));
        egui::TopBottomPanel::top("top")
            .exact_height(72.0)
            .show(ctx, |ui| {
                ui.horizontal_top(|ui| {
                    let title_width = 440.0_f32.min(ui.available_width() * 0.48);
                    ui.add_sized(
                        [title_width, 32.0],
                        egui::Label::new(RichText::new(APP_NAME).size(24.0)).truncate(),
                    );
                    ui.separator();
                    let status_width = ui.available_width().max(1.0);
                    ui.allocate_ui_with_layout(
                        egui::vec2(status_width, 68.0),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            ui.set_min_width(0.0);
                            ui.set_max_width(status_width);
                            ui.spacing_mut().item_spacing.y = 1.0;
                            ui.horizontal(|ui| {
                                ui.strong("Version");
                                ui.monospace(env!("CARGO_PKG_VERSION"));
                                ui.separator();
                                ui.strong("Axis downloads");
                                let color = if self.axis_download_status == "Reachable" {
                                    Color32::from_rgb(105, 210, 145)
                                } else if self.axis_download_status == "Checking…" {
                                    Color32::from_rgb(245, 190, 90)
                                } else {
                                    Color32::from_rgb(245, 105, 105)
                                };
                                ui.colored_label(color, &self.axis_download_status)
                                    .on_hover_text("https://ftp.axis.com/pub/axis/software/");
                            });
                            let database_path = inventory::path().display().to_string();
                            let row_width = ui.available_width().max(1.0);
                            ui.add_sized(
                                [row_width, 20.0],
                                egui::Label::new(
                                    RichText::new(format!("Database  {database_path}")).monospace(),
                                )
                                .truncate(),
                            )
                            .on_hover_text(database_path);
                            if logging::available() {
                                let path = logging::path().display().to_string();
                                let row_width = ui.available_width().max(1.0);
                                ui.add_sized(
                                    [row_width, 20.0],
                                    egui::Label::new(
                                        RichText::new(format!("Debug log  {path}")).monospace(),
                                    )
                                    .truncate(),
                                )
                                .on_hover_text(path);
                            } else {
                                ui.colored_label(Color32::RED, "Debug log  Unavailable");
                            }
                        },
                    );
                });
            });
        egui::TopBottomPanel::bottom("upgrade")
            .resizable(false)
            .show(ctx, |ui| {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    let selected = self.devices.iter().filter(|d| d.selected).count();
                    ui.horizontal(|ui| {
                        ui.heading("Firmware upgrade");
                        ui.label(
                            RichText::new(format!("{selected} selected")).color(Color32::GRAY),
                        );
                        if ui
                            .add_enabled(!self.busy, egui::Button::new("Refresh catalog"))
                            .clicked()
                        {
                            self.catalog_cache.clear();
                            self.check_catalog();
                        }
                    });
                    ui.label(
                        RichText::new(
                            "Safe mode: upgrades only, settings preserved, catalog firmware only",
                        )
                        .color(Color32::from_rgb(105, 210, 145)),
                    );
                    ui.horizontal(|ui| {
                        let button = egui::Button::new(
                            RichText::new("Download and upgrade selected devices").strong(),
                        )
                        .fill(Color32::from_rgb(35, 105, 150));
                        if ui.add_enabled(!self.busy, button).clicked() {
                            let valid = self.devices.iter().any(|row| row.selected)
                                && self.devices.iter().filter(|row| row.selected).all(|row| {
                                    let installed = row
                                        .info
                                        .as_ref()
                                        .and_then(|info| info.firmware_version.as_deref())
                                        .unwrap_or("");
                                    row.releases.get(row.release_index).is_some_and(|release| {
                                        version_is_newer(&release.version, installed)
                                    })
                                });
                            if valid {
                                self.upgrade_confirmation_open = true;
                            } else {
                                self.status =
                                    "Select at least one camera with a newer catalog release."
                                        .into();
                            }
                        }
                        ui.label(&self.status);
                    });
                    for row in self
                        .devices
                        .iter()
                        .filter(|row| row.state.starts_with("Upgrade failed:"))
                    {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(
                                RichText::new("UPGRADE FAILED")
                                    .strong()
                                    .color(Color32::from_rgb(245, 105, 105)),
                            );
                            ui.monospace(&row.address);
                            ui.label(row.state.trim_start_matches("Upgrade failed: "));
                        });
                    }
                });
            });
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.strong("Add device");
                    let response = ui.add_sized(
                        [260.0, 28.0],
                        egui::TextEdit::singleline(&mut self.address)
                            .hint_text("IP address or URL"),
                    );
                    let enter =
                        response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if ui.button("Add").clicked() || enter {
                        let a = self.address.clone();
                        self.add_address(&a);
                        self.address.clear();
                    }
                    if ui
                        .add_enabled(!self.busy, egui::Button::new("Discover LAN"))
                        .clicked()
                    {
                        self.busy = true;
                        self.status = "Discovering Axis devices on the LAN…".into();
                        let tx = self.tx.clone();
                        thread::spawn(move || {
                            let _ = tx.send(Event::Discovered(ws_discovery::discover(
                                std::time::Duration::from_secs(4),
                            )));
                        });
                    }
                });
                ui.horizontal(|ui| {
                    ui.strong("Credentials");
                    ui.add_sized(
                        [150.0, 28.0],
                        egui::TextEdit::singleline(&mut self.username).hint_text("Administrator"),
                    );
                    ui.add_sized(
                        [190.0, 28.0],
                        egui::TextEdit::singleline(&mut self.password)
                            .password(true)
                            .hint_text("Password"),
                    );
                    ui.checkbox(&mut self.remember_password, "Remember in keyring");
                    if ui.button("Save").clicked() {
                        self.save_credentials();
                    }
                    let all: Vec<_> = self.devices.iter().map(|d| d.address.clone()).collect();
                    if ui
                        .add_enabled(
                            !self.busy && !all.is_empty(),
                            egui::Button::new("Re-inventory all"),
                        )
                        .clicked()
                    {
                        self.inspect(all);
                    }
                });
            });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.heading("Devices");
                ui.label(
                    RichText::new(format!("{} found", self.devices.len())).color(Color32::GRAY),
                );
            });
            let mut sort_request = None;
            let mut refresh_request = None;
            let mut forget_request = None;
            let mut rollback_request = None;
            let mut select_all =
                !self.devices.is_empty() && self.devices.iter().all(|d| d.selected);
            let original_select_all = select_all;
            let table_height = ui.available_height().max(220.0);
            egui::ScrollArea::vertical()
                .max_height(table_height)
                .show(ui, |ui| {
                    egui_extras::TableBuilder::new(ui)
                        .striped(true)
                        .column(egui_extras::Column::exact(38.0))
                        .column(egui_extras::Column::exact(145.0))
                        .column(egui_extras::Column::exact(130.0))
                        .column(egui_extras::Column::exact(150.0))
                        .column(egui_extras::Column::exact(190.0))
                        .column(egui_extras::Column::remainder())
                        // Both row actions must remain fully visible. The previous
                        // 90 px column could only fit Refresh and clipped Forget
                        // against the right edge of the viewport.
                        .column(egui_extras::Column::exact(150.0))
                        .header(32.0, |mut h| {
                            h.col(|ui| {
                                ui.checkbox(&mut select_all, "")
                                    .on_hover_text("Select all devices");
                            });
                            h.col(|ui| {
                                if ui.button("Address").clicked() {
                                    sort_request = Some(SortColumn::Address);
                                }
                            });
                            h.col(|ui| {
                                if ui.button("Model").clicked() {
                                    sort_request = Some(SortColumn::Model);
                                }
                            });
                            h.col(|ui| {
                                if ui.button("Installed").clicked() {
                                    sort_request = Some(SortColumn::Installed);
                                }
                            });
                            h.col(|ui| {
                                if ui.button("Target AXIS OS").clicked() {
                                    sort_request = Some(SortColumn::Available);
                                }
                            });
                            h.col(|ui| {
                                if ui.button("Status").clicked() {
                                    sort_request = Some(SortColumn::Status);
                                }
                            });
                            h.col(|ui| {
                                ui.strong("Actions");
                            });
                        })
                        .body(|mut body| {
                            for d in &mut self.devices {
                                body.row(34.0, |mut r| {
                                    r.col(|ui| {
                                        ui.checkbox(&mut d.selected, "");
                                    });
                                    r.col(|ui| {
                                        ui.add(egui::Label::new(&d.address).truncate())
                                            .on_hover_text(&d.address);
                                    });
                                    let model = model_label(d.info.as_ref());
                                    r.col(|ui| {
                                        ui.add(egui::Label::new(&model).truncate())
                                            .on_hover_text(&model);
                                    });
                                    let installed = d
                                        .info
                                        .as_ref()
                                        .and_then(|i| i.firmware_version.as_deref())
                                        .unwrap_or("Unknown");
                                    r.col(|ui| {
                                        ui.add(egui::Label::new(installed).truncate());
                                    });
                                    r.col(|ui| {
                                        if d.releases.is_empty() {
                                            ui.label("Not checked");
                                        } else {
                                            let selected = d
                                                .releases
                                                .get(d.release_index)
                                                .map(|v| format!("{} · {}", v.version, v.track))
                                                .unwrap_or_default();
                                            egui::ComboBox::from_id_salt(format!(
                                                "release-{}",
                                                d.address
                                            ))
                                            .selected_text(selected)
                                            .show_ui(
                                                ui,
                                                |ui| {
                                                    for (index, release) in
                                                        d.releases.iter().enumerate()
                                                    {
                                                        ui.selectable_value(
                                                            &mut d.release_index,
                                                            index,
                                                            format!(
                                                                "{} · {}",
                                                                release.version, release.track
                                                            ),
                                                        );
                                                    }
                                                },
                                            );
                                        }
                                    });
                                    r.col(|ui| {
                                        if let Some(progress) = d.progress {
                                            let percent = progress.clamp(0.0, 1.0) * 100.0;
                                            ui.add(
                                                egui::ProgressBar::new(progress.clamp(0.0, 1.0))
                                                    .text(format!("{} — {percent:.0}%", d.state))
                                                    .animate(true),
                                            )
                                            .on_hover_text(&d.state);
                                        } else if d.state.contains("Sending") {
                                            ui.horizontal(|ui| {
                                                ui.spinner();
                                                ui.label("SENDING FIRMWARE TO CAMERA");
                                            })
                                            .response
                                            .on_hover_text(&d.state);
                                        } else if let Some(error) =
                                            d.state.strip_prefix("Upgrade failed: ")
                                        {
                                            ui.add(
                                                egui::Label::new(
                                                    RichText::new(format!("ERROR: {error}"))
                                                        .color(Color32::from_rgb(245, 105, 105)),
                                                )
                                                .truncate(),
                                            )
                                            .on_hover_text(&d.state);
                                        } else {
                                            let (badge, color) = status_badge(&d.state);
                                            ui.label(RichText::new(badge).strong().color(color))
                                                .on_hover_text(&d.state);
                                        }
                                    });
                                    r.col(|ui| {
                                        ui.horizontal(|ui| {
                                            if ui
                                                .add_enabled(
                                                    d.progress.is_none(),
                                                    egui::Button::new("Refresh"),
                                                )
                                                .on_hover_text(
                                                    "Re-read this camera and check its catalog status",
                                                )
                                                .clicked()
                                            {
                                                refresh_request = Some(d.address.clone());
                                            }
                                            ui.menu_button("More...", |ui| {
                                                if ui
                                                    .add_enabled(
                                                        !self.busy,
                                                        egui::Button::new("Check rollback…"),
                                                    )
                                                    .on_hover_text(
                                                        "Ask this camera whether it retains a previous firmware image",
                                                    )
                                                    .clicked()
                                                {
                                                    rollback_request = Some(d.address.clone());
                                                    ui.close_menu();
                                                }
                                                if ui
                                                    .add_enabled(
                                                        !self.busy,
                                                        egui::Button::new("Forget camera"),
                                                    )
                                                    .on_hover_text(
                                                        "Remove this camera from the saved inventory",
                                                    )
                                                    .clicked()
                                                {
                                                    forget_request = Some(d.address.clone());
                                                    ui.close_menu();
                                                }
                                            });
                                        });
                                    });
                                });
                            }
                        });
                });
            if select_all != original_select_all {
                for row in &mut self.devices {
                    row.selected = select_all;
                }
            }
            if let Some(column) = sort_request {
                self.set_sort(column);
            }
            if let Some(address) = refresh_request {
                self.inspect(vec![address]);
            }
            if let Some(address) = forget_request {
                self.devices.retain(|device| device.address != address);
                self.save_inventory();
                self.status = format!("Forgot saved camera {address}.");
            }
            if let Some(address) = rollback_request {
                self.check_rollback(address);
            }
            });
        if self.upgrade_confirmation_open {
            let mut confirm = false;
            let mut cancel = false;
            egui::Window::new("Confirm safe firmware upgrade")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .show(ctx, |ui| {
                    ui.label(
                        RichText::new("Settings will be preserved. Factory reset and downgrade are disabled.")
                            .strong()
                            .color(Color32::from_rgb(105, 210, 145)),
                    );
                    ui.separator();
                    egui::Grid::new("upgrade-plan").striped(true).show(ui, |ui| {
                        ui.strong("Camera");
                        ui.strong("Model");
                        ui.strong("Installed");
                        ui.strong("Target");
                        ui.end_row();
                        for row in self.devices.iter().filter(|row| row.selected) {
                            ui.monospace(&row.address);
                            ui.label(model_label(row.info.as_ref()));
                            ui.label(
                                row.info
                                    .as_ref()
                                    .and_then(|info| info.firmware_version.as_deref())
                                    .unwrap_or("Unknown"),
                            );
                            ui.label(
                                row.releases
                                    .get(row.release_index)
                                    .map(|release| release.version.as_str())
                                    .unwrap_or("Unavailable"),
                            );
                            ui.end_row();
                        }
                    });
                    ui.separator();
                    ui.label("The camera will reboot. Keep power and network connected until verification completes.");
                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() { cancel = true; }
                        if ui
                            .button(RichText::new("Confirm upgrade").strong())
                            .clicked()
                        {
                            confirm = true;
                        }
                    });
                });
            if cancel {
                self.upgrade_confirmation_open = false;
            }
            if confirm {
                self.upgrade_confirmation_open = false;
                self.start_upgrade();
            }
        }
        if let Some(plan) = self.rollback_confirmation.clone() {
            let mut confirm = false;
            let mut catalog_install = false;
            let mut cancel = false;
            egui::Window::new("Firmware rollback and catalog availability")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .show(ctx, |ui| {
                    ui.colored_label(
                        Color32::from_rgb(245, 190, 90),
                        RichText::new("Rollback changes firmware and restores configuration state associated with the previous firmware.").strong(),
                    );
                    ui.separator();
                    egui::Grid::new("rollback-plan").striped(true).show(ui, |ui| {
                        ui.strong("Camera");
                        ui.monospace(&plan.address);
                        ui.end_row();
                        ui.strong("Current AXIS OS");
                        ui.label(&plan.active_version);
                        ui.end_row();
                        ui.strong("Rollback AXIS OS");
                        ui.label(if plan.inactive_version.is_empty() {
                            "Unavailable"
                        } else {
                            &plan.inactive_version
                        });
                        ui.end_row();
                        if let Some(pending) = &plan.pending_commit {
                            ui.strong("Pending commit");
                            ui.label(pending);
                            ui.end_row();
                        }
                        if let Some(seconds) = plan.time_to_rollback {
                            ui.strong("Automatic rollback timer");
                            ui.label(format!("{seconds} seconds remaining"));
                            ui.end_row();
                        }
                    });
                    ui.separator();
                    ui.strong("Firmware sources");
                    egui::Grid::new("rollback-sources")
                        .striped(true)
                        .show(ui, |ui| {
                            if !plan.inactive_version.is_empty() {
                                ui.label(&plan.inactive_version);
                                ui.label("ON CAMERA · rollback available");
                                ui.end_row();
                            }
                            for (index, option) in plan.catalog_releases.iter().enumerate() {
                                ui.radio_value(
                                    &mut self.rollback_catalog_selection,
                                    Some(index),
                                    format!(
                                        "{} · {}",
                                        option.release.version, option.release.track
                                    ),
                                );
                                if option.cached {
                                    ui.colored_label(
                                        Color32::from_rgb(105, 210, 145),
                                        "DOWNLOADED · checksum verified",
                                    );
                                } else {
                                    ui.label("DOWNLOAD REQUIRED");
                                }
                                ui.end_row();
                            }
                    });
                    ui.small(format!("Selecting an older catalog image performs a settings-preserving downgrade attempt, not an on-camera rollback. If the camera requires a hard factory reset, {APP_NAME} stops without changing settings."));
                    ui.separator();
                    if !plan.inactive_version.is_empty() {
                        ui.label("The camera will reboot. This does not request a factory reset, but settings changed since the previous firmware state may be reverted.");
                        ui.label("Keep power and network connected until verification completes.");
                    } else {
                        ui.label("This camera does not currently offer an on-camera rollback image.");
                    }
                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            cancel = true;
                        }
                        if ui
                            .add_enabled(
                                !plan.inactive_version.is_empty(),
                                egui::Button::new(RichText::new("Confirm rollback").strong()),
                            )
                            .clicked()
                        {
                            confirm = true;
                        }
                        let selected_release = self
                            .rollback_catalog_selection
                            .and_then(|index| plan.catalog_releases.get(index));
                        let can_install = selected_release
                            .is_some_and(|option| option.release.version != plan.active_version);
                        let label = match selected_release {
                            Some(option) if option.cached => {
                                "Install selected downloaded version"
                            }
                            Some(_) => "Download and install selected version",
                            None => "Select a catalog version",
                        };
                        if ui
                            .add_enabled(can_install, egui::Button::new(label))
                            .clicked()
                        {
                            catalog_install = true;
                        }
                    });
                });
            if cancel {
                self.rollback_confirmation = None;
                self.rollback_catalog_selection = None;
            }
            if confirm {
                self.rollback_confirmation = None;
                self.rollback_catalog_selection = None;
                self.start_rollback(plan.clone());
            }
            if catalog_install {
                if let Some(option) = self
                    .rollback_catalog_selection
                    .and_then(|index| plan.catalog_releases.get(index))
                    .cloned()
                {
                    self.rollback_confirmation = None;
                    self.rollback_catalog_selection = None;
                    self.start_catalog_install(CatalogInstallPlan {
                        address: plan.address,
                        previous_version: plan.active_version,
                        release: option.release,
                    });
                }
            }
        }
    }
}

fn main() -> eframe::Result {
    logging::init();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1400.0, 900.0])
            .with_min_inner_size([960.0, 650.0]),
        persist_window: true,
        ..Default::default()
    };
    eframe::run_native(
        APP_NAME,
        options,
        Box::new(|cc| {
            cc.egui_ctx.set_pixels_per_point(1.5);
            let mut visuals = egui::Visuals::dark();
            visuals.panel_fill = Color32::from_rgb(22, 25, 30);
            visuals.extreme_bg_color = Color32::from_rgb(13, 16, 20);
            visuals.selection.bg_fill = Color32::from_rgb(35, 105, 150);
            cc.egui_ctx.set_visuals(visuals);
            let mut style = (*cc.egui_ctx.style()).clone();
            style.spacing.item_spacing = egui::vec2(10.0, 8.0);
            style.spacing.button_padding = egui::vec2(10.0, 6.0);
            cc.egui_ctx.set_style(style);
            Ok(Box::<App>::default())
        }),
    )
}

#[cfg(test)]
mod batch_tests {
    use super::*;

    fn release(model: &str, version: &str) -> FirmwareRelease {
        FirmwareRelease {
            version: version.into(),
            track: "AXIS OS LTS".into(),
            url: format!("https://ftp.axis.com/{model}_{version}.bin"),
            sha256: "a".repeat(64),
        }
    }

    #[test]
    fn batch_groups_shared_firmware_without_losing_camera_mapping() {
        let shared = release("M3116-LVE", "10.12.338");
        let distinct = release("P3715-PLVE", "11.11.238");
        let groups = group_firmware_targets(vec![
            ("http://10.0.0.201".into(), shared.clone()),
            ("http://10.0.0.202".into(), shared.clone()),
            ("http://10.0.0.200".into(), distinct.clone()),
        ]);

        assert_eq!(groups.len(), 2);
        assert_eq!(groups[&shared.url].1.len(), 2);
        assert!(groups[&shared.url].1.contains(&"http://10.0.0.201".into()));
        assert!(groups[&shared.url].1.contains(&"http://10.0.0.202".into()));
        assert_eq!(groups[&distinct.url].1, vec!["http://10.0.0.200"]);
        assert_eq!(MAX_PARALLEL_UPLOADS, 2);
    }
}

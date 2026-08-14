use std::{
    fs,
    io::Cursor,
    path::{Path, PathBuf},
    sync::mpsc::{Receiver, Sender},
    thread,
};

use anyhow::{Context, Result};
use image::{
    ImageFormat, ImageReader, Limits, RgbaImage,
    imageops::{self, FilterType},
};
use tray_icon::{
    Icon, TrayIcon, TrayIconBuilder,
    menu::{Menu, MenuEvent, MenuId, MenuItem},
};
use winit::{
    application::ApplicationHandler,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy},
    window::WindowId,
};

use crate::{
    controller::{ControllerPhase, ControllerSnapshot},
    dsh::update::{DshUpdatePhase, DshUpdateSnapshot},
};

const STATUS_ID: &str = "status";
const OPEN_DSH_ID: &str = "open-dsh";
const START_ID: &str = "start";
const STOP_ID: &str = "stop";
const RESTART_ID: &str = "restart";
const CHECK_DSH_UPDATE_ID: &str = "check-dsh-update";
const INSTALL_DSH_UPDATE_ID: &str = "install-dsh-update";
const EXIT_ID: &str = "exit";
const TRAY_ICON_SIZE: u32 = 32;
const MAX_CUSTOM_ICON_BYTES: u64 = 4 * 1024 * 1024;
const MAX_CUSTOM_ICON_DIMENSION: u32 = 1024;
const BUNDLED_BLUE_ICON: &[u8] = include_bytes!("../../../assets/icons/deepseek-blue.ico");

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TrayIconOrigin {
    BundledBlue,
    Custom(PathBuf),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrayIconAsset {
    rgba: Vec<u8>,
    width: u32,
    height: u32,
    origin: TrayIconOrigin,
}

pub fn load_tray_icon(atelier_root: &Path) -> Result<TrayIconAsset> {
    for (name, format) in [
        ("icon.png", ImageFormat::Png),
        ("icon.ico", ImageFormat::Ico),
    ] {
        let path = atelier_root.join(name);
        if !path.is_file() {
            continue;
        }
        match decode_icon_file(&path, format) {
            Ok(rgba) => {
                tracing::info!(path = %path.display(), "using custom Tray icon");
                return Ok(TrayIconAsset {
                    rgba,
                    width: TRAY_ICON_SIZE,
                    height: TRAY_ICON_SIZE,
                    origin: TrayIconOrigin::Custom(path),
                });
            }
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "custom Tray icon is invalid");
            }
        }
    }

    bundled_tray_icon()
}

fn bundled_tray_icon() -> Result<TrayIconAsset> {
    let rgba = decode_icon(BUNDLED_BLUE_ICON, ImageFormat::Ico)
        .context("decode the bundled DeepSeek blue Tray icon")?;
    Ok(TrayIconAsset {
        rgba,
        width: TRAY_ICON_SIZE,
        height: TRAY_ICON_SIZE,
        origin: TrayIconOrigin::BundledBlue,
    })
}

fn decode_icon_file(path: &Path, format: ImageFormat) -> Result<Vec<u8>> {
    let metadata =
        fs::metadata(path).with_context(|| format!("inspect custom icon {}", path.display()))?;
    if metadata.len() > MAX_CUSTOM_ICON_BYTES {
        anyhow::bail!(
            "custom icon is {} bytes; the maximum is {MAX_CUSTOM_ICON_BYTES}",
            metadata.len()
        );
    }
    let bytes = fs::read(path).with_context(|| format!("read custom icon {}", path.display()))?;
    decode_icon(&bytes, format)
}

fn decode_icon(bytes: &[u8], format: ImageFormat) -> Result<Vec<u8>> {
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_CUSTOM_ICON_DIMENSION);
    limits.max_image_height = Some(MAX_CUSTOM_ICON_DIMENSION);
    limits.max_alloc = Some(16 * 1024 * 1024);
    reader.limits(limits);
    let source = reader.decode().context("decode icon image")?.to_rgba8();
    Ok(fit_icon_to_square(&source).into_raw())
}

fn fit_icon_to_square(source: &RgbaImage) -> RgbaImage {
    let (width, height) = source.dimensions();
    let scale =
        (TRAY_ICON_SIZE as f64 / f64::from(width)).min(TRAY_ICON_SIZE as f64 / f64::from(height));
    let scaled_width = (f64::from(width) * scale).round().max(1.0) as u32;
    let scaled_height = (f64::from(height) * scale).round().max(1.0) as u32;
    let resized = imageops::resize(source, scaled_width, scaled_height, FilterType::Lanczos3);
    let mut canvas = RgbaImage::new(TRAY_ICON_SIZE, TRAY_ICON_SIZE);
    imageops::overlay(
        &mut canvas,
        &resized,
        i64::from((TRAY_ICON_SIZE - scaled_width) / 2),
        i64::from((TRAY_ICON_SIZE - scaled_height) / 2),
    );
    canvas
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrayCommand {
    OpenDsh,
    Start,
    Stop,
    Restart,
    CheckDshUpdate,
    InstallDshUpdate,
    Exit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrayStatus {
    Unknown,
    Stopped,
    Starting,
    Running,
    Restarting,
    Failed,
    Stopping,
}

impl TrayStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "DSH: Unknown",
            Self::Stopped => "DSH: Stopped",
            Self::Starting => "DSH: Starting…",
            Self::Running => "DSH: Running",
            Self::Restarting => "DSH: Restarting…",
            Self::Failed => "DSH: Failed",
            Self::Stopping => "DSH: Stopping…",
        }
    }
}

impl From<&ControllerSnapshot> for TrayStatus {
    fn from(snapshot: &ControllerSnapshot) -> Self {
        match snapshot.phase {
            ControllerPhase::Stopped | ControllerPhase::Shutdown => Self::Stopped,
            ControllerPhase::Starting => Self::Starting,
            ControllerPhase::Running => Self::Running,
            ControllerPhase::RestartBackoff => Self::Restarting,
            ControllerPhase::Failed => Self::Failed,
            ControllerPhase::ShuttingDown => Self::Stopping,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TrayStateUpdate {
    Dsh(TrayStatus),
    Update(DshUpdateSnapshot),
}

#[derive(Debug)]
enum TrayEvent {
    Menu(MenuId),
    State(TrayStateUpdate),
}

pub fn run_tray(command_sender: Sender<TrayCommand>) -> Result<()> {
    let (_state_sender, state_receiver) = std::sync::mpsc::channel();
    run_tray_with_ready(command_sender, state_receiver, bundled_tray_icon()?, || {
        Ok(())
    })
}

pub fn run_tray_with_ready(
    command_sender: Sender<TrayCommand>,
    state_receiver: Receiver<TrayStateUpdate>,
    icon: TrayIconAsset,
    on_ready: impl FnOnce() -> Result<()> + 'static,
) -> Result<()> {
    let mut event_loop_builder = EventLoop::<TrayEvent>::with_user_event();
    #[cfg(target_os = "macos")]
    {
        use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};

        event_loop_builder.with_activation_policy(ActivationPolicy::Accessory);
    }
    let event_loop = event_loop_builder
        .build()
        .context("failed to create the tray event loop")?;
    let proxy = event_loop.create_proxy();
    install_menu_event_handler(proxy.clone());
    thread::Builder::new()
        .name("atelier-tray-status".to_owned())
        .spawn(move || {
            while let Ok(state) = state_receiver.recv() {
                if proxy.send_event(TrayEvent::State(state)).is_err() {
                    break;
                }
            }
        })
        .context("failed to start the tray status bridge")?;

    let mut application = TrayApplication::new(command_sender, icon, Box::new(on_ready));
    event_loop
        .run_app(&mut application)
        .context("tray event loop failed")?;
    match application.startup_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn install_menu_event_handler(proxy: EventLoopProxy<TrayEvent>) {
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let _ = proxy.send_event(TrayEvent::Menu(event.id));
    }));
}

struct TrayApplication {
    command_sender: Sender<TrayCommand>,
    tray: Option<TrayIcon>,
    status_item: Option<MenuItem>,
    update_item: Option<MenuItem>,
    check_update_item: Option<MenuItem>,
    current_status: TrayStatus,
    current_update: DshUpdateSnapshot,
    icon: TrayIconAsset,
    on_ready: Option<Box<dyn FnOnce() -> Result<()>>>,
    startup_error: Option<anyhow::Error>,
}

impl TrayApplication {
    fn new(
        command_sender: Sender<TrayCommand>,
        icon: TrayIconAsset,
        on_ready: Box<dyn FnOnce() -> Result<()>>,
    ) -> Self {
        Self {
            command_sender,
            tray: None,
            status_item: None,
            update_item: None,
            check_update_item: None,
            current_status: TrayStatus::Unknown,
            current_update: DshUpdateSnapshot::default(),
            icon,
            on_ready: Some(on_ready),
            startup_error: None,
        }
    }

    fn handle_menu(&self, id: &MenuId, event_loop: &ActiveEventLoop) {
        let Some(command) = command_for_menu_id(id.as_ref()) else {
            return;
        };

        let should_exit = command == TrayCommand::Exit;
        if self.command_sender.send(command).is_err() || should_exit {
            event_loop.exit();
        }
    }
}

impl ApplicationHandler<TrayEvent> for TrayApplication {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.tray.is_some() {
            return;
        }

        match create_tray(self.current_status, &self.current_update, &self.icon) {
            Ok((tray, status_item, update_item, check_update_item)) => {
                self.tray = Some(tray);
                self.status_item = Some(status_item);
                self.update_item = Some(update_item);
                self.check_update_item = Some(check_update_item);
                if let Some(on_ready) = self.on_ready.take()
                    && let Err(error) = on_ready()
                {
                    tracing::error!(%error, "tray readiness callback failed");
                    self.startup_error = Some(error);
                    event_loop.exit();
                }
            }
            Err(error) => {
                tracing::error!(%error, "failed to create tray");
                self.startup_error = Some(error);
                event_loop.exit();
            }
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: TrayEvent) {
        match event {
            TrayEvent::Menu(id) => self.handle_menu(&id, event_loop),
            TrayEvent::State(state) => match state {
                TrayStateUpdate::Dsh(status) => {
                    self.current_status = status;
                    if let Some(item) = &self.status_item {
                        item.set_text(status.as_str());
                    }
                }
                TrayStateUpdate::Update(update) => {
                    self.current_update = update;
                    let presentation = update_menu_presentation(&self.current_update);
                    if let Some(item) = &self.update_item {
                        item.set_text(&presentation.text);
                        item.set_enabled(presentation.enabled);
                    }
                    if let Some(item) = &self.check_update_item {
                        item.set_enabled(!matches!(
                            self.current_update.phase,
                            DshUpdatePhase::Checking | DshUpdatePhase::Installing
                        ));
                    }
                }
            },
        }
    }

    fn window_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        _event: WindowEvent,
    ) {
    }
}

fn create_tray(
    initial_status: TrayStatus,
    initial_update: &DshUpdateSnapshot,
    icon: &TrayIconAsset,
) -> Result<(TrayIcon, MenuItem, MenuItem, MenuItem)> {
    let menu = Menu::new();
    let status = MenuItem::with_id(STATUS_ID, initial_status.as_str(), false, None);
    let open_dsh = MenuItem::with_id(OPEN_DSH_ID, "Open DSH", true, None);
    let start = MenuItem::with_id(START_ID, "Start", true, None);
    let stop = MenuItem::with_id(STOP_ID, "Stop", true, None);
    let restart = MenuItem::with_id(RESTART_ID, "Restart", true, None);
    let update_presentation = update_menu_presentation(initial_update);
    let install_update = MenuItem::with_id(
        INSTALL_DSH_UPDATE_ID,
        &update_presentation.text,
        update_presentation.enabled,
        None,
    );
    let check_update = MenuItem::with_id(
        CHECK_DSH_UPDATE_ID,
        "Check for DSH updates",
        !matches!(
            initial_update.phase,
            DshUpdatePhase::Checking | DshUpdatePhase::Installing
        ),
        None,
    );
    let exit = MenuItem::with_id(EXIT_ID, "Exit", true, None);

    for item in [
        &status,
        &open_dsh,
        &start,
        &stop,
        &restart,
        &install_update,
        &check_update,
        &exit,
    ] {
        menu.append(item).context("failed to build tray menu")?;
    }

    let tray = TrayIconBuilder::new()
        .with_tooltip("DSH Atelier")
        .with_menu(Box::new(menu))
        .with_icon(
            Icon::from_rgba(icon.rgba.clone(), icon.width, icon.height)
                .context("Tray icon is invalid")?,
        )
        .build()
        .context("failed to create the system tray icon")?;
    Ok((tray, status, install_update, check_update))
}

struct UpdateMenuPresentation {
    text: String,
    enabled: bool,
}

fn update_menu_presentation(update: &DshUpdateSnapshot) -> UpdateMenuPresentation {
    let version = update
        .available_version
        .as_ref()
        .or(update.current_version.as_ref())
        .map(ToString::to_string)
        .unwrap_or_else(|| "unknown".to_owned());
    let (text, enabled) = match update.phase {
        DshUpdatePhase::Idle => ("DSH updates: Not checked".to_owned(), false),
        DshUpdatePhase::Checking => ("Checking for DSH updates…".to_owned(), false),
        DshUpdatePhase::UpToDate => (format!("DSH {version} is up to date"), false),
        DshUpdatePhase::Available if update.can_install => {
            (format!("Install DSH {version}…"), true)
        }
        DshUpdatePhase::Available => (
            format!("DSH {version} available — update externally"),
            false,
        ),
        DshUpdatePhase::Installing => (format!("Installing DSH {version}…"), false),
        DshUpdatePhase::RestartRequired => (format!("Applying DSH {version}…"), false),
        DshUpdatePhase::CheckFailed => ("DSH update check failed".to_owned(), false),
        DshUpdatePhase::InstallFailed => (format!("DSH {version} update failed"), false),
    };
    UpdateMenuPresentation { text, enabled }
}

fn command_for_menu_id(id: &str) -> Option<TrayCommand> {
    match id {
        OPEN_DSH_ID => Some(TrayCommand::OpenDsh),
        START_ID => Some(TrayCommand::Start),
        STOP_ID => Some(TrayCommand::Stop),
        RESTART_ID => Some(TrayCommand::Restart),
        CHECK_DSH_UPDATE_ID => Some(TrayCommand::CheckDshUpdate),
        INSTALL_DSH_UPDATE_ID => Some(TrayCommand::InstallDshUpdate),
        EXIT_ID => Some(TrayCommand::Exit),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use image::{ImageFormat, Rgba, RgbaImage};
    use tempfile::tempdir;

    use super::*;
    use crate::controller::{ControllerPhase, ControllerSnapshot};

    #[test]
    fn maps_action_menu_ids_to_commands() {
        assert_eq!(command_for_menu_id(OPEN_DSH_ID), Some(TrayCommand::OpenDsh));
        assert_eq!(command_for_menu_id(START_ID), Some(TrayCommand::Start));
        assert_eq!(command_for_menu_id(STOP_ID), Some(TrayCommand::Stop));
        assert_eq!(command_for_menu_id(RESTART_ID), Some(TrayCommand::Restart));
        assert_eq!(command_for_menu_id(EXIT_ID), Some(TrayCommand::Exit));
        assert_eq!(
            command_for_menu_id(CHECK_DSH_UPDATE_ID),
            Some(TrayCommand::CheckDshUpdate)
        );
        assert_eq!(
            command_for_menu_id(INSTALL_DSH_UPDATE_ID),
            Some(TrayCommand::InstallDshUpdate)
        );
    }

    #[test]
    fn ignores_status_and_unknown_menu_ids() {
        assert_eq!(command_for_menu_id(STATUS_ID), None);
        assert_eq!(command_for_menu_id("unknown"), None);
    }

    #[test]
    fn loads_icon_png_from_the_atelier_root() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("icon.png");
        RgbaImage::from_pixel(8, 8, Rgba([210, 20, 30, 255]))
            .save_with_format(&path, ImageFormat::Png)
            .unwrap();

        let icon = load_tray_icon(directory.path()).unwrap();

        assert_eq!(icon.origin, TrayIconOrigin::Custom(path));
        assert_eq!((icon.width, icon.height), (32, 32));
        let center = ((16 * 32 + 16) * 4) as usize;
        assert_eq!(&icon.rgba[center..center + 4], &[210, 20, 30, 255]);
    }

    #[test]
    fn invalid_custom_icon_falls_back_to_the_bundled_blue_icon() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("icon.png"), b"not an image").unwrap();

        let icon = load_tray_icon(directory.path()).unwrap();

        assert_eq!(icon.origin, TrayIconOrigin::BundledBlue);
        assert_eq!((icon.width, icon.height), (32, 32));
        assert_eq!(icon.rgba.len(), 32 * 32 * 4);
        assert!(icon.rgba.chunks_exact(4).any(|pixel| pixel[3] != 0));
    }

    #[test]
    fn loads_icon_ico_when_png_is_absent() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("icon.ico");
        RgbaImage::from_pixel(16, 16, Rgba([15, 30, 220, 255]))
            .save_with_format(&path, ImageFormat::Ico)
            .unwrap();

        let icon = load_tray_icon(directory.path()).unwrap();

        assert_eq!(icon.origin, TrayIconOrigin::Custom(path));
        let center = ((16 * 32 + 16) * 4) as usize;
        assert_eq!(&icon.rgba[center..center + 4], &[15, 30, 220, 255]);
    }

    #[test]
    fn bundled_official_icon_contains_blue_visible_pixels() {
        let icon = bundled_tray_icon().unwrap();
        let (red, green, blue) = icon
            .rgba
            .chunks_exact(4)
            .filter(|pixel| pixel[3] > 128)
            .fold((0_u64, 0_u64, 0_u64), |totals, pixel| {
                (
                    totals.0 + u64::from(pixel[0]),
                    totals.1 + u64::from(pixel[1]),
                    totals.2 + u64::from(pixel[2]),
                )
            });

        assert!(blue > red);
        assert!(blue > green);
    }

    #[test]
    fn packaged_black_icon_is_monochrome_and_visible() {
        let rgba = decode_icon(
            include_bytes!("../../../assets/icons/deepseek-black.ico"),
            ImageFormat::Ico,
        )
        .unwrap();
        let visible = rgba
            .chunks_exact(4)
            .filter(|pixel| pixel[3] > 128)
            .collect::<Vec<_>>();

        assert!(!visible.is_empty());
        assert!(
            visible
                .iter()
                .all(|pixel| pixel[0] == 0 && pixel[1] == 0 && pixel[2] == 0)
        );
    }

    #[test]
    fn maps_controller_phases_to_stable_tray_status_text() {
        let mut snapshot = ControllerSnapshot::default();
        assert_eq!(TrayStatus::from(&snapshot).as_str(), "DSH: Stopped");

        snapshot.phase = ControllerPhase::Starting;
        assert_eq!(TrayStatus::from(&snapshot).as_str(), "DSH: Starting…");
        snapshot.phase = ControllerPhase::Running;
        assert_eq!(TrayStatus::from(&snapshot).as_str(), "DSH: Running");
        snapshot.phase = ControllerPhase::RestartBackoff;
        assert_eq!(TrayStatus::from(&snapshot).as_str(), "DSH: Restarting…");
        snapshot.phase = ControllerPhase::Failed;
        assert_eq!(TrayStatus::from(&snapshot).as_str(), "DSH: Failed");
        snapshot.phase = ControllerPhase::ShuttingDown;
        assert_eq!(TrayStatus::from(&snapshot).as_str(), "DSH: Stopping…");
        snapshot.phase = ControllerPhase::Shutdown;
        assert_eq!(TrayStatus::from(&snapshot).as_str(), "DSH: Stopped");
    }

    #[test]
    fn managed_updates_are_actionable_from_the_tray() {
        let presentation = update_menu_presentation(&crate::dsh::update::DshUpdateSnapshot {
            phase: crate::dsh::update::DshUpdatePhase::Available,
            current_version: Some(semver::Version::parse("0.1.0").unwrap()),
            available_version: Some(semver::Version::parse("0.2.0").unwrap()),
            can_install: true,
            last_error: None,
        });

        assert_eq!(presentation.text, "Install DSH 0.2.0…");
        assert!(presentation.enabled);
    }

    #[test]
    fn external_updates_explain_that_atelier_will_not_replace_them() {
        let presentation = update_menu_presentation(&crate::dsh::update::DshUpdateSnapshot {
            phase: crate::dsh::update::DshUpdatePhase::Available,
            current_version: Some(semver::Version::parse("0.1.0").unwrap()),
            available_version: Some(semver::Version::parse("0.2.0").unwrap()),
            can_install: false,
            last_error: None,
        });

        assert_eq!(presentation.text, "DSH 0.2.0 available — update externally");
        assert!(!presentation.enabled);
    }
}

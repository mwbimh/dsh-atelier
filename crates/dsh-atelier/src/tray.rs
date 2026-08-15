use std::{
    fs,
    io::Cursor,
    path::{Path, PathBuf},
    rc::Rc,
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
    window::{Theme, WindowId},
};

use crate::{
    atelier_update::{AtelierUpdatePhase, AtelierUpdateSnapshot},
    config::{SurfaceConfig, ThemePreference},
    controller::{ControllerPhase, ControllerSnapshot},
    dsh::update::{DshUpdatePhase, DshUpdateSnapshot},
    paths::AtelierPaths,
    platform::{NativeBrowser, NativeNotifier},
    ports::{Browser, Notifier},
    surface::{DshWebSurface, SurfaceAction, SurfaceRequest},
};

const STATUS_ID: &str = "status";
const OPEN_DSH_ID: &str = "open-dsh";
const START_ID: &str = "start";
const STOP_ID: &str = "stop";
const RESTART_ID: &str = "restart";
const CHECK_DSH_UPDATE_ID: &str = "check-dsh-update";
const INSTALL_DSH_UPDATE_ID: &str = "install-dsh-update";
const CHECK_ATELIER_UPDATE_ID: &str = "check-atelier-update";
const INSTALL_ATELIER_UPDATE_ID: &str = "install-atelier-update";
const EXIT_ID: &str = "exit";
const TRAY_ICON_SIZE: u32 = 32;
const MAX_CUSTOM_ICON_BYTES: u64 = 4 * 1024 * 1024;
const MAX_CUSTOM_ICON_DIMENSION: u32 = 1024;
#[cfg(not(target_os = "macos"))]
const BUNDLED_BLUE_ICON: &[u8] = include_bytes!("../../../assets/icons/deepseek-blue.ico");
#[cfg(target_os = "macos")]
const BUNDLED_BLACK_ICON: &[u8] = include_bytes!("../../../assets/icons/deepseek-black.ico");

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TrayIconOrigin {
    BundledBlue,
    BundledBlack,
    Custom(PathBuf),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrayIconAsset {
    rgba: Vec<u8>,
    width: u32,
    height: u32,
    origin: TrayIconOrigin,
    is_template: bool,
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
                    is_template: false,
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
    #[cfg(target_os = "macos")]
    let (bytes, origin, description, is_template) = (
        BUNDLED_BLACK_ICON,
        TrayIconOrigin::BundledBlack,
        "black",
        true,
    );
    #[cfg(not(target_os = "macos"))]
    let (bytes, origin, description, is_template) = (
        BUNDLED_BLUE_ICON,
        TrayIconOrigin::BundledBlue,
        "blue",
        false,
    );
    let rgba = decode_icon(bytes, ImageFormat::Ico)
        .with_context(|| format!("decode the bundled DeepSeek {description} Tray icon"))?;
    Ok(TrayIconAsset {
        rgba,
        width: TRAY_ICON_SIZE,
        height: TRAY_ICON_SIZE,
        origin,
        is_template,
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
    OpenDshInBrowser,
    Start,
    Stop,
    Restart,
    CheckDshUpdate,
    InstallDshUpdate,
    CheckAtelierUpdate,
    InstallAtelierUpdate,
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
    DshUpdate(DshUpdateSnapshot),
    AtelierUpdate(AtelierUpdateSnapshot),
    RestartAtelier,
    Terminate,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TrayExitReason {
    #[default]
    Exit,
    RestartForUpdate,
}

#[derive(Debug)]
enum TrayEvent {
    Menu(MenuId),
    State(TrayStateUpdate),
    Surface(SurfaceRequest),
    SurfaceAction(SurfaceAction),
}

pub fn run_tray(command_sender: Sender<TrayCommand>) -> Result<()> {
    let (_state_sender, state_receiver) = std::sync::mpsc::channel();
    let (_surface_sender, surface_receiver) = std::sync::mpsc::channel();
    let paths = AtelierPaths::discover()?;
    run_tray_with_ready(
        command_sender,
        state_receiver,
        surface_receiver,
        SurfaceHostConfig::new(
            paths.dsh_surface_dir,
            paths.root,
            SurfaceConfig::default(),
            ThemePreference::default(),
        ),
        bundled_tray_icon()?,
        || Ok(()),
    )
    .map(|_| ())
}

#[derive(Clone, Debug)]
pub struct SurfaceHostConfig {
    profile_directory: PathBuf,
    atelier_root: PathBuf,
    presentation: SurfaceConfig,
    theme: ThemePreference,
}

impl SurfaceHostConfig {
    #[must_use]
    pub fn new(
        profile_directory: PathBuf,
        atelier_root: PathBuf,
        presentation: SurfaceConfig,
        theme: ThemePreference,
    ) -> Self {
        Self {
            profile_directory,
            atelier_root,
            presentation,
            theme,
        }
    }
}

pub fn run_tray_with_ready(
    command_sender: Sender<TrayCommand>,
    state_receiver: Receiver<TrayStateUpdate>,
    surface_receiver: Receiver<SurfaceRequest>,
    surface_host: SurfaceHostConfig,
    icon: TrayIconAsset,
    on_ready: impl FnOnce() -> Result<()> + 'static,
) -> Result<TrayExitReason> {
    let mut event_loop_builder = EventLoop::<TrayEvent>::with_user_event();
    #[cfg(target_os = "macos")]
    {
        use winit::platform::macos::EventLoopBuilderExtMacOS;

        event_loop_builder.with_activation_policy(macos_activation_policy());
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

    let surface_proxy = event_loop.create_proxy();
    thread::Builder::new()
        .name("atelier-surface-requests".to_owned())
        .spawn(move || {
            while let Ok(request) = surface_receiver.recv() {
                if surface_proxy
                    .send_event(TrayEvent::Surface(request))
                    .is_err()
                {
                    break;
                }
            }
        })
        .context("failed to start the DSH Surface request bridge")?;

    let mut application = TrayApplication::new(
        command_sender,
        icon,
        surface_host,
        event_loop.create_proxy(),
        Box::new(on_ready),
    );
    event_loop
        .run_app(&mut application)
        .context("tray event loop failed")?;
    match application.startup_error {
        Some(error) => Err(error),
        None => Ok(application.exit_reason),
    }
}

fn install_menu_event_handler(proxy: EventLoopProxy<TrayEvent>) {
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let _ = proxy.send_event(TrayEvent::Menu(event.id));
    }));
}

#[cfg(target_os = "macos")]
fn macos_activation_policy() -> winit::platform::macos::ActivationPolicy {
    winit::platform::macos::ActivationPolicy::Regular
}

struct TrayApplication {
    command_sender: Sender<TrayCommand>,
    tray: Option<TrayIcon>,
    status_item: Option<MenuItem>,
    dsh_update_item: Option<MenuItem>,
    check_dsh_update_item: Option<MenuItem>,
    atelier_update_item: Option<MenuItem>,
    check_atelier_update_item: Option<MenuItem>,
    current_status: TrayStatus,
    current_dsh_update: DshUpdateSnapshot,
    current_atelier_update: AtelierUpdateSnapshot,
    icon: TrayIconAsset,
    surface_host: SurfaceHostConfig,
    surface: Option<DshWebSurface>,
    event_proxy: EventLoopProxy<TrayEvent>,
    browser: NativeBrowser,
    notifier: NativeNotifier,
    on_ready: Option<Box<dyn FnOnce() -> Result<()>>>,
    startup_error: Option<anyhow::Error>,
    exit_reason: TrayExitReason,
}

impl TrayApplication {
    fn new(
        command_sender: Sender<TrayCommand>,
        icon: TrayIconAsset,
        surface_host: SurfaceHostConfig,
        event_proxy: EventLoopProxy<TrayEvent>,
        on_ready: Box<dyn FnOnce() -> Result<()>>,
    ) -> Self {
        Self {
            command_sender,
            tray: None,
            status_item: None,
            dsh_update_item: None,
            check_dsh_update_item: None,
            atelier_update_item: None,
            check_atelier_update_item: None,
            current_status: TrayStatus::Unknown,
            current_dsh_update: DshUpdateSnapshot::default(),
            current_atelier_update: AtelierUpdateSnapshot::default(),
            icon,
            surface_host,
            surface: None,
            event_proxy,
            browser: NativeBrowser::default(),
            notifier: NativeNotifier::default(),
            on_ready: Some(on_ready),
            startup_error: None,
            exit_reason: TrayExitReason::Exit,
        }
    }

    fn handle_menu(&mut self, id: &MenuId, event_loop: &ActiveEventLoop) {
        let Some(command) = command_for_menu_id(id.as_ref()) else {
            return;
        };

        let should_exit = command == TrayCommand::Exit;
        if should_exit {
            self.exit_reason = TrayExitReason::Exit;
        }
        if self.command_sender.send(command).is_err() || should_exit {
            event_loop.exit();
        }
    }

    fn create_loading_surface(&self, event_loop: &ActiveEventLoop) -> Result<DshWebSurface> {
        let proxy = self.event_proxy.clone();
        let sink = Rc::new(move |action| {
            let _ = proxy.send_event(TrayEvent::SurfaceAction(action));
        });
        DshWebSurface::new_loading(
            event_loop,
            self.surface_host.profile_directory.clone(),
            &self.surface_host.atelier_root,
            &self.surface_host.presentation,
            window_theme_override(self.surface_host.theme),
            sink,
        )
    }

    fn report_surface_creation_error(&self, error: &anyhow::Error) {
        tracing::error!(%error, "failed to create the DSH Surface");
        let _ = self.notifier.notify(
            "DSH Surface unavailable",
            "Atelier could not create the DSH window. DSH is still running from the Tray.",
        );
    }

    fn handle_surface_request(&mut self, event_loop: &ActiveEventLoop, request: SurfaceRequest) {
        match request {
            SurfaceRequest::ShowLoading => {
                if let Some(surface) = self.surface.as_mut() {
                    if let Err(error) = surface.show_loading() {
                        tracing::error!(%error, "failed to show the DSH loading page");
                    }
                    return;
                }

                match self.create_loading_surface(event_loop) {
                    Ok(surface) => self.surface = Some(surface),
                    Err(error) => self.report_surface_creation_error(&error),
                }
            }
            SurfaceRequest::Show(url) => {
                if self.surface.is_none() {
                    match self.create_loading_surface(event_loop) {
                        Ok(surface) => self.surface = Some(surface),
                        Err(error) => {
                            self.report_surface_creation_error(&error);
                            return;
                        }
                    }
                }

                if let Some(surface) = self.surface.as_mut()
                    && let Err(error) = surface.show(&url)
                {
                    tracing::error!(%error, "failed to show the DSH Surface");
                }
            }
            SurfaceRequest::Navigate(url) => {
                if let Some(surface) = self.surface.as_mut()
                    && let Err(error) = surface.navigate(&url)
                {
                    tracing::error!(%error, "failed to navigate the DSH Surface");
                }
            }
            SurfaceRequest::Hide => {
                if let Some(surface) = self.surface.as_mut() {
                    surface.hide();
                }
            }
        }
    }

    fn handle_surface_action(&mut self, action: SurfaceAction) {
        match action {
            SurfaceAction::OpenInBrowser => {
                let _ = self.command_sender.send(TrayCommand::OpenDshInBrowser);
            }
            SurfaceAction::OpenExternal(url) => {
                if let Err(error) = self.browser.open_external(&url) {
                    tracing::error!(%error, "failed to open an external DSH link");
                }
            }
            action => {
                if let Some(surface) = self.surface.as_mut()
                    && let Err(error) = surface.handle_action(action)
                {
                    tracing::error!(%error, "DSH Surface action failed");
                }
            }
        }
    }
}

impl ApplicationHandler<TrayEvent> for TrayApplication {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.tray.is_some() {
            return;
        }

        match create_tray(
            self.current_status,
            &self.current_dsh_update,
            &self.current_atelier_update,
            &self.icon,
        ) {
            Ok(items) => {
                self.tray = Some(items.tray);
                self.status_item = Some(items.status);
                self.dsh_update_item = Some(items.dsh_update);
                self.check_dsh_update_item = Some(items.check_dsh_update);
                self.atelier_update_item = Some(items.atelier_update);
                self.check_atelier_update_item = Some(items.check_atelier_update);
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
            TrayEvent::Surface(request) => self.handle_surface_request(event_loop, request),
            TrayEvent::SurfaceAction(action) => self.handle_surface_action(action),
            TrayEvent::State(state) if state_requests_exit(&state) => {
                self.exit_reason = if matches!(state, TrayStateUpdate::RestartAtelier) {
                    TrayExitReason::RestartForUpdate
                } else {
                    TrayExitReason::Exit
                };
                event_loop.exit();
            }
            TrayEvent::State(state) => match state {
                TrayStateUpdate::Dsh(status) => {
                    self.current_status = status;
                    if let Some(item) = &self.status_item {
                        item.set_text(status.as_str());
                    }
                }
                TrayStateUpdate::DshUpdate(update) => {
                    self.current_dsh_update = update;
                    let presentation = update_menu_presentation(&self.current_dsh_update);
                    if let Some(item) = &self.dsh_update_item {
                        item.set_text(&presentation.text);
                        item.set_enabled(presentation.enabled);
                    }
                    if let Some(item) = &self.check_dsh_update_item {
                        item.set_enabled(!matches!(
                            self.current_dsh_update.phase,
                            DshUpdatePhase::Checking | DshUpdatePhase::Installing
                        ));
                    }
                }
                TrayStateUpdate::AtelierUpdate(update) => {
                    self.current_atelier_update = update;
                    let presentation =
                        atelier_update_menu_presentation(&self.current_atelier_update);
                    if let Some(item) = &self.atelier_update_item {
                        item.set_text(&presentation.text);
                        item.set_enabled(presentation.enabled);
                    }
                    if let Some(item) = &self.check_atelier_update_item {
                        item.set_enabled(!matches!(
                            self.current_atelier_update.phase,
                            AtelierUpdatePhase::Checking
                                | AtelierUpdatePhase::Installing
                                | AtelierUpdatePhase::RestartRequired
                                | AtelierUpdatePhase::Disabled
                        ));
                    }
                }
                TrayStateUpdate::RestartAtelier | TrayStateUpdate::Terminate => {
                    unreachable!("handled before state dispatch")
                }
            },
        }
    }

    fn window_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        if let Some(surface) = self.surface.as_mut()
            && surface.window_id() == window_id
        {
            let result = match event {
                WindowEvent::ThemeChanged(system_theme) => {
                    match system_theme_update(self.surface_host.theme, system_theme) {
                        Some(theme) => surface.set_theme(theme),
                        None => Ok(()),
                    }
                }
                event => surface.handle_window_event(&event),
            };
            if let Err(error) = result {
                tracing::error!(%error, "DSH Surface window event failed");
            }
        }
    }
}

fn system_theme_update(preference: ThemePreference, system_theme: Theme) -> Option<Theme> {
    (preference == ThemePreference::System).then_some(system_theme)
}

fn window_theme_override(preference: ThemePreference) -> Option<Theme> {
    match preference {
        ThemePreference::Light => Some(Theme::Light),
        ThemePreference::Dark => Some(Theme::Dark),
        ThemePreference::System => None,
    }
}

fn state_requests_exit(state: &TrayStateUpdate) -> bool {
    matches!(
        state,
        TrayStateUpdate::Terminate | TrayStateUpdate::RestartAtelier
    )
}

struct TrayItems {
    tray: TrayIcon,
    status: MenuItem,
    dsh_update: MenuItem,
    check_dsh_update: MenuItem,
    atelier_update: MenuItem,
    check_atelier_update: MenuItem,
}

fn create_tray(
    initial_status: TrayStatus,
    initial_dsh_update: &DshUpdateSnapshot,
    initial_atelier_update: &AtelierUpdateSnapshot,
    icon: &TrayIconAsset,
) -> Result<TrayItems> {
    let menu = Menu::new();
    let status = MenuItem::with_id(STATUS_ID, initial_status.as_str(), false, None);
    let open_dsh = MenuItem::with_id(OPEN_DSH_ID, "Open DSH", true, None);
    let start = MenuItem::with_id(START_ID, "Start", true, None);
    let stop = MenuItem::with_id(STOP_ID, "Stop", true, None);
    let restart = MenuItem::with_id(RESTART_ID, "Restart", true, None);
    let dsh_update_presentation = update_menu_presentation(initial_dsh_update);
    let install_dsh_update = MenuItem::with_id(
        INSTALL_DSH_UPDATE_ID,
        &dsh_update_presentation.text,
        dsh_update_presentation.enabled,
        None,
    );
    let check_dsh_update = MenuItem::with_id(
        CHECK_DSH_UPDATE_ID,
        "Check for DSH updates",
        !matches!(
            initial_dsh_update.phase,
            DshUpdatePhase::Checking | DshUpdatePhase::Installing
        ),
        None,
    );
    let atelier_update_presentation = atelier_update_menu_presentation(initial_atelier_update);
    let install_atelier_update = MenuItem::with_id(
        INSTALL_ATELIER_UPDATE_ID,
        &atelier_update_presentation.text,
        atelier_update_presentation.enabled,
        None,
    );
    let check_atelier_update = MenuItem::with_id(
        CHECK_ATELIER_UPDATE_ID,
        "Check for Atelier updates",
        !matches!(
            initial_atelier_update.phase,
            AtelierUpdatePhase::Checking
                | AtelierUpdatePhase::Installing
                | AtelierUpdatePhase::RestartRequired
                | AtelierUpdatePhase::Disabled
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
        &install_dsh_update,
        &check_dsh_update,
        &install_atelier_update,
        &check_atelier_update,
        &exit,
    ] {
        menu.append(item).context("failed to build tray menu")?;
    }

    let tray = TrayIconBuilder::new()
        .with_tooltip("DSH Atelier")
        .with_menu(Box::new(menu))
        .with_icon_as_template(icon.is_template)
        .with_icon(
            Icon::from_rgba(icon.rgba.clone(), icon.width, icon.height)
                .context("Tray icon is invalid")?,
        )
        .build()
        .context("failed to create the system tray icon")?;
    Ok(TrayItems {
        tray,
        status,
        dsh_update: install_dsh_update,
        check_dsh_update,
        atelier_update: install_atelier_update,
        check_atelier_update,
    })
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

fn atelier_update_menu_presentation(update: &AtelierUpdateSnapshot) -> UpdateMenuPresentation {
    let version = update
        .available_version
        .as_ref()
        .or(update.current_version.as_ref())
        .map(ToString::to_string)
        .unwrap_or_else(|| "unknown".to_owned());
    let (text, enabled) = match update.phase {
        AtelierUpdatePhase::Disabled => ("Atelier updates: Disabled".to_owned(), false),
        AtelierUpdatePhase::Idle => ("Atelier updates: Not checked".to_owned(), false),
        AtelierUpdatePhase::Checking => ("Checking for Atelier updates…".to_owned(), false),
        AtelierUpdatePhase::UpToDate => (format!("Atelier {version} is up to date"), false),
        AtelierUpdatePhase::Available => (format!("Install Atelier {version}…"), true),
        AtelierUpdatePhase::FullPackageRequired => {
            (format!("Atelier {version} requires full download"), false)
        }
        AtelierUpdatePhase::Installing => (format!("Installing Atelier {version}…"), false),
        AtelierUpdatePhase::RestartRequired => {
            (format!("Restarting into Atelier {version}…"), false)
        }
        AtelierUpdatePhase::CheckFailed => ("Atelier update check failed".to_owned(), false),
        AtelierUpdatePhase::InstallFailed => (format!("Retry Atelier {version} update…"), true),
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
        CHECK_ATELIER_UPDATE_ID => Some(TrayCommand::CheckAtelierUpdate),
        INSTALL_ATELIER_UPDATE_ID => Some(TrayCommand::InstallAtelierUpdate),
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
    use crate::config::ThemePreference;
    use crate::controller::{ControllerPhase, ControllerSnapshot};

    #[test]
    fn fixed_theme_preferences_ignore_system_theme_changes() {
        assert_eq!(
            system_theme_update(ThemePreference::Light, winit::window::Theme::Dark),
            None
        );
        assert_eq!(
            system_theme_update(ThemePreference::Dark, winit::window::Theme::Light),
            None
        );
    }

    #[test]
    fn theme_preferences_map_to_native_window_overrides() {
        assert_eq!(
            window_theme_override(ThemePreference::Light),
            Some(winit::window::Theme::Light)
        );
        assert_eq!(
            window_theme_override(ThemePreference::Dark),
            Some(winit::window::Theme::Dark)
        );
        assert_eq!(window_theme_override(ThemePreference::System), None);
    }

    #[test]
    fn system_theme_preference_follows_system_theme_changes() {
        assert_eq!(
            system_theme_update(ThemePreference::System, winit::window::Theme::Light),
            Some(winit::window::Theme::Light)
        );
        assert_eq!(
            system_theme_update(ThemePreference::System, winit::window::Theme::Dark),
            Some(winit::window::Theme::Dark)
        );
    }

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
        assert_eq!(
            command_for_menu_id(CHECK_ATELIER_UPDATE_ID),
            Some(TrayCommand::CheckAtelierUpdate)
        );
        assert_eq!(
            command_for_menu_id(INSTALL_ATELIER_UPDATE_ID),
            Some(TrayCommand::InstallAtelierUpdate)
        );
    }

    #[test]
    fn ignores_status_and_unknown_menu_ids() {
        assert_eq!(command_for_menu_id(STATUS_ID), None);
        assert_eq!(command_for_menu_id("unknown"), None);
    }

    #[test]
    fn macos_termination_update_requests_event_loop_exit() {
        assert!(state_requests_exit(&TrayStateUpdate::Terminate));
        assert!(state_requests_exit(&TrayStateUpdate::RestartAtelier));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_runtime_uses_a_regular_activation_policy_for_its_dock_icon() {
        assert!(matches!(
            macos_activation_policy(),
            winit::platform::macos::ActivationPolicy::Regular
        ));
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
        assert!(!icon.is_template);
        assert_eq!((icon.width, icon.height), (32, 32));
        let center = ((16 * 32 + 16) * 4) as usize;
        assert_eq!(&icon.rgba[center..center + 4], &[210, 20, 30, 255]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_bundled_icon_uses_the_black_template_asset() {
        let icon = bundled_tray_icon().unwrap();

        assert_eq!(icon.origin, TrayIconOrigin::BundledBlack);
        assert!(icon.is_template);
        let visible = icon
            .rgba
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
    fn invalid_custom_icon_falls_back_to_the_platform_bundled_icon() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("icon.png"), b"not an image").unwrap();

        let icon = load_tray_icon(directory.path()).unwrap();
        let bundled = bundled_tray_icon().unwrap();

        assert_eq!(icon.origin, bundled.origin);
        assert_eq!(icon.is_template, bundled.is_template);
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
    #[cfg(not(target_os = "macos"))]
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

    #[test]
    fn atelier_runtime_updates_are_actionable_without_affecting_dsh_updates() {
        let presentation =
            atelier_update_menu_presentation(&crate::atelier_update::AtelierUpdateSnapshot {
                phase: crate::atelier_update::AtelierUpdatePhase::Available,
                current_version: Some(semver::Version::parse("0.1.0").unwrap()),
                available_version: Some(semver::Version::parse("0.2.0").unwrap()),
                min_bootstrap_generation: Some(1),
                last_error: None,
            });

        assert_eq!(presentation.text, "Install Atelier 0.2.0…");
        assert!(presentation.enabled);
    }

    #[test]
    fn atelier_updates_requiring_a_new_bootstrap_use_the_full_package() {
        let presentation =
            atelier_update_menu_presentation(&crate::atelier_update::AtelierUpdateSnapshot {
                phase: crate::atelier_update::AtelierUpdatePhase::FullPackageRequired,
                current_version: Some(semver::Version::parse("0.1.0").unwrap()),
                available_version: Some(semver::Version::parse("0.2.0").unwrap()),
                min_bootstrap_generation: Some(2),
                last_error: None,
            });

        assert_eq!(presentation.text, "Atelier 0.2.0 requires full download");
        assert!(!presentation.enabled);
    }
}

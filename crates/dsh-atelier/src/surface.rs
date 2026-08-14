use std::{cell::RefCell, fmt, fs, net::IpAddr, path::PathBuf, rc::Rc};

use anyhow::{Context, Result};
use image::ImageFormat;
use url::{Host, Url};
use winit::{
    dpi::{LogicalSize, PhysicalPosition, PhysicalSize},
    event::WindowEvent,
    event_loop::ActiveEventLoop,
    window::{Icon, Window, WindowId},
};
use wry::{NewWindowResponse, Rect, WebContext, WebView, WebViewBuilder};

use crate::dsh::readiness::LoopbackUrl;

const WINDOW_TITLE: &str = "DeepSeek Harness";
const INITIAL_WIDTH: f64 = 1100.0;
const INITIAL_HEIGHT: f64 = 760.0;
const MINIMUM_WIDTH: f64 = 720.0;
const MINIMUM_HEIGHT: f64 = 480.0;
const TOOLBAR_HEIGHT: f64 = 42.0;
const SURFACE_WINDOW_ICON: &[u8] = include_bytes!("../../../assets/icons/deepseek-black.ico");

/// The only origin that the DSH WebView may load in-process.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AllowedOrigin {
    port: u16,
}

impl AllowedOrigin {
    #[must_use]
    pub fn from_loopback(url: &LoopbackUrl) -> Self {
        Self { port: url.port() }
    }

    /// Classifies an absolute URL reported by the WebView.
    #[must_use]
    pub fn decide(&self, requested: &str) -> NavigationDecision {
        let Ok(url) = Url::parse(requested) else {
            return NavigationDecision::Reject;
        };
        if has_credentials(&url) {
            return NavigationDecision::Reject;
        }

        if url.scheme() == "http"
            && url.host_str() == Some("127.0.0.1")
            && url.port_or_known_default() == Some(self.port)
        {
            return NavigationDecision::Allow;
        }

        match ExternalUrl::from_url(url) {
            Ok(url) => NavigationDecision::OpenExternal(url),
            Err(_) => NavigationDecision::Reject,
        }
    }
}

/// A browser-safe external URL. Loopback URLs can never inhabit this type.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExternalUrl(Url);

impl ExternalUrl {
    pub fn parse(value: &str) -> Result<Self, ExternalUrlError> {
        Self::from_url(Url::parse(value)?)
    }

    fn from_url(url: Url) -> Result<Self, ExternalUrlError> {
        let is_http = matches!(url.scheme(), "http" | "https");
        let has_host = url.host().is_some();
        if !is_http || !has_host || has_credentials(&url) || is_loopback_host(url.host()) {
            return Err(ExternalUrlError::Unsafe(url.to_string()));
        }
        Ok(Self(url))
    }

    #[must_use]
    pub fn as_url(&self) -> &Url {
        &self.0
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Display for ExternalUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ExternalUrlError {
    #[error("external URL is malformed: {0}")]
    Invalid(#[from] url::ParseError),
    #[error("external URL must be non-loopback HTTP(S) without credentials: {0}")]
    Unsafe(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NavigationDecision {
    Allow,
    OpenExternal(ExternalUrl),
    Reject,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SurfaceRequest {
    ShowLoading,
    Show(LoopbackUrl),
    Navigate(LoopbackUrl),
    Hide,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SurfaceAction {
    Refresh,
    OpenInBrowser,
    Minimize,
    ToggleMaximize,
    Close,
    StartDrag,
    OpenExternal(ExternalUrl),
}

/// The single native DSH Surface owned by the winit main thread.
///
/// The toolbar and DSH content are separate WebViews. Only the local toolbar
/// has an IPC handler; the DSH view receives no injected script or IPC bridge.
pub struct DshWebSurface {
    // Field order is deliberate: child WebViews and their context must be
    // dropped before the native parent window.
    toolbar_webview: WebView,
    dsh_webview: WebView,
    _toolbar_context: WebContext,
    _dsh_context: WebContext,
    window: Window,
    allowed_origin: Rc<RefCell<Option<AllowedOrigin>>>,
    current_url: Option<LoopbackUrl>,
}

impl DshWebSurface {
    pub fn new_loading(
        event_loop: &ActiveEventLoop,
        profile_dir: PathBuf,
        action_sink: Rc<dyn Fn(SurfaceAction)>,
    ) -> Result<Self> {
        fs::create_dir_all(&profile_dir).with_context(|| {
            format!(
                "failed to create the DSH Surface profile at {}",
                profile_dir.display()
            )
        })?;
        let window_icon = surface_window_icon()?;
        let window = event_loop
            .create_window(
                Window::default_attributes()
                    .with_title(WINDOW_TITLE)
                    .with_window_icon(Some(window_icon))
                    .with_inner_size(LogicalSize::new(INITIAL_WIDTH, INITIAL_HEIGHT))
                    .with_min_inner_size(LogicalSize::new(MINIMUM_WIDTH, MINIMUM_HEIGHT))
                    .with_decorations(!cfg!(windows))
                    .with_visible(false),
            )
            .context("failed to create the DSH Surface window")?;
        let (toolbar_bounds, dsh_bounds) =
            webview_bounds(window.inner_size(), window.scale_factor());

        let toolbar_sink = action_sink.clone();
        let mut toolbar_context = surface_web_context(profile_dir.join("toolbar"));
        let toolbar_webview = WebViewBuilder::new_with_web_context(&mut toolbar_context)
            .with_html(toolbar_html())
            .with_incognito(cfg!(target_os = "macos"))
            .with_bounds(toolbar_bounds)
            .with_ipc_handler(move |request| {
                if let Some(action) = toolbar_action(request.body()) {
                    toolbar_sink(action);
                }
            })
            .with_new_window_req_handler(|_, _| NewWindowResponse::Deny)
            .build_as_child(&window)
            .context("failed to create the DSH Surface toolbar")?;

        let allowed_origin = Rc::new(RefCell::new(None));
        let navigation_origin = allowed_origin.clone();
        let navigation_sink = action_sink.clone();
        let new_window_origin = allowed_origin.clone();
        let new_window_sink = action_sink;
        let mut dsh_context = surface_web_context(profile_dir.join("webview"));
        let dsh_webview = WebViewBuilder::new_with_web_context(&mut dsh_context)
            .with_html(loading_html())
            .with_incognito(cfg!(target_os = "macos"))
            .with_bounds(dsh_bounds)
            .with_navigation_handler(move |requested| {
                allow_navigation(
                    navigation_origin.borrow().as_ref(),
                    &navigation_sink,
                    &requested,
                )
            })
            .with_new_window_req_handler(move |requested, _| {
                if let NavigationDecision::OpenExternal(url) =
                    decide_surface_navigation(new_window_origin.borrow().as_ref(), &requested)
                {
                    new_window_sink(SurfaceAction::OpenExternal(url));
                }
                NewWindowResponse::Deny
            })
            .build_as_child(&window)
            .context("failed to create the DSH WebView")?;

        let mut surface = Self {
            toolbar_webview,
            dsh_webview,
            _toolbar_context: toolbar_context,
            _dsh_context: dsh_context,
            window,
            allowed_origin,
            current_url: None,
        };
        surface.show_loading()?;
        Ok(surface)
    }

    #[must_use]
    pub fn window_id(&self) -> WindowId {
        self.window.id()
    }

    pub fn show(&mut self, url: &LoopbackUrl) -> Result<()> {
        if self.current_url.as_ref() != Some(url) {
            self.navigate(url)?;
        }
        self.show_window()
    }

    pub fn show_loading(&mut self) -> Result<()> {
        if self.current_url.is_some() {
            let previous_origin = self.allowed_origin.replace(None);
            if let Err(error) = self.dsh_webview.load_html(&loading_html()) {
                self.allowed_origin.replace(previous_origin);
                return Err(error).context("failed to show the DSH loading page");
            }
            self.current_url = None;
        }
        self.show_window()
    }

    fn show_window(&self) -> Result<()> {
        self.window.set_minimized(false);
        self.window.set_visible(true);
        self.window.focus_window();
        self.dsh_webview
            .focus()
            .context("failed to focus the DSH WebView")
    }

    pub fn navigate(&mut self, url: &LoopbackUrl) -> Result<()> {
        let next_origin = AllowedOrigin::from_loopback(url);
        let previous_origin = self.allowed_origin.replace(Some(next_origin));
        if let Err(error) = self.dsh_webview.load_url(url.as_str()) {
            self.allowed_origin.replace(previous_origin);
            return Err(error).context("failed to navigate the DSH WebView");
        }
        self.current_url = Some(url.clone());
        Ok(())
    }

    pub fn hide(&self) {
        self.window.set_visible(false);
    }

    pub fn handle_request(&mut self, request: &SurfaceRequest) -> Result<()> {
        match request {
            SurfaceRequest::ShowLoading => self.show_loading(),
            SurfaceRequest::Show(url) => self.show(url),
            SurfaceRequest::Navigate(url) => self.navigate(url),
            SurfaceRequest::Hide => {
                self.hide();
                Ok(())
            }
        }
    }

    pub fn handle_action(&self, action: SurfaceAction) -> Result<()> {
        match action {
            SurfaceAction::Refresh => self
                .dsh_webview
                .reload()
                .context("failed to refresh the DSH WebView"),
            SurfaceAction::Minimize => {
                self.window.set_minimized(true);
                Ok(())
            }
            SurfaceAction::ToggleMaximize => {
                let maximized = !self.window.is_maximized();
                self.window.set_maximized(maximized);
                self.sync_maximize_button(maximized)
            }
            SurfaceAction::Close => {
                self.hide();
                Ok(())
            }
            SurfaceAction::StartDrag => self
                .window
                .drag_window()
                .context("failed to drag the DSH Surface window"),
            SurfaceAction::OpenInBrowser | SurfaceAction::OpenExternal(_) => Ok(()),
        }
    }

    pub fn handle_window_event(&self, event: &WindowEvent) -> Result<()> {
        match event {
            WindowEvent::CloseRequested => self.handle_action(SurfaceAction::Close),
            WindowEvent::Resized(size) => {
                self.layout(*size)?;
                self.sync_maximize_button(self.window.is_maximized())
            }
            WindowEvent::ScaleFactorChanged { .. } => self.layout(self.window.inner_size()),
            _ => Ok(()),
        }
    }

    fn layout(&self, size: PhysicalSize<u32>) -> Result<()> {
        let (toolbar_bounds, dsh_bounds) = webview_bounds(size, self.window.scale_factor());
        self.toolbar_webview
            .set_bounds(toolbar_bounds)
            .context("failed to resize the DSH Surface toolbar")?;
        self.dsh_webview
            .set_bounds(dsh_bounds)
            .context("failed to resize the DSH WebView")
    }

    fn sync_maximize_button(&self, maximized: bool) -> Result<()> {
        self.toolbar_webview
            .evaluate_script(maximize_state_script(maximized))
            .context("failed to update the DSH Surface maximize button")
    }
}

fn surface_web_context(profile_dir: PathBuf) -> WebContext {
    // WKWebView ignores wry's data_directory and would otherwise persist
    // Atelier-owned browsing state in the system default data store. Use a
    // non-persistent data store on macOS; Windows keeps its profile under
    // ~/.atelier as requested by the supplied directory.
    if cfg!(target_os = "macos") {
        WebContext::new(None)
    } else {
        WebContext::new(Some(profile_dir))
    }
}

fn surface_window_icon_rgba() -> Result<(Vec<u8>, u32, u32)> {
    let image = image::load_from_memory_with_format(SURFACE_WINDOW_ICON, ImageFormat::Ico)
        .context("decode the packaged monochrome DeepSeek Surface icon")?
        .to_rgba8();
    let (width, height) = image.dimensions();
    Ok((image.into_raw(), width, height))
}

fn surface_window_icon() -> Result<Icon> {
    let (rgba, width, height) = surface_window_icon_rgba()?;
    Icon::from_rgba(rgba, width, height).context("create the DSH Surface window icon")
}

fn has_credentials(url: &Url) -> bool {
    !url.username().is_empty() || url.password().is_some()
}

fn is_loopback_host(host: Option<Host<&str>>) -> bool {
    match host {
        Some(Host::Ipv4(address)) => IpAddr::V4(address).is_loopback(),
        Some(Host::Ipv6(address)) => {
            IpAddr::V6(address).is_loopback()
                || address
                    .to_ipv4_mapped()
                    .is_some_and(|address| address.is_loopback())
        }
        Some(Host::Domain(domain)) => {
            domain.eq_ignore_ascii_case("localhost")
                || domain.to_ascii_lowercase().ends_with(".localhost")
        }
        None => false,
    }
}

fn allow_navigation(
    origin: Option<&AllowedOrigin>,
    action_sink: &Rc<dyn Fn(SurfaceAction)>,
    requested: &str,
) -> bool {
    match decide_surface_navigation(origin, requested) {
        NavigationDecision::Allow => true,
        NavigationDecision::OpenExternal(url) => {
            action_sink(SurfaceAction::OpenExternal(url));
            false
        }
        NavigationDecision::Reject => false,
    }
}

fn decide_surface_navigation(
    origin: Option<&AllowedOrigin>,
    requested: &str,
) -> NavigationDecision {
    match origin {
        Some(origin) => origin.decide(requested),
        None if requested == "about:blank" || requested.starts_with("data:text/html") => {
            NavigationDecision::Allow
        }
        None => NavigationDecision::Reject,
    }
}

fn toolbar_action(message: &str) -> Option<SurfaceAction> {
    match message {
        "refresh" => Some(SurfaceAction::Refresh),
        "open-browser" => Some(SurfaceAction::OpenInBrowser),
        "minimize" => Some(SurfaceAction::Minimize),
        "toggle-maximize" => Some(SurfaceAction::ToggleMaximize),
        "close" => Some(SurfaceAction::Close),
        "start-drag" => Some(SurfaceAction::StartDrag),
        _ => None,
    }
}

fn webview_bounds(size: PhysicalSize<u32>, scale_factor: f64) -> (Rect, Rect) {
    let toolbar_height = ((TOOLBAR_HEIGHT * scale_factor).round() as u32).min(size.height);
    let toolbar = Rect {
        position: PhysicalPosition::new(0, 0).into(),
        size: PhysicalSize::new(size.width, toolbar_height).into(),
    };
    let dsh = Rect {
        position: PhysicalPosition::new(0, toolbar_height).into(),
        size: PhysicalSize::new(size.width, size.height.saturating_sub(toolbar_height)).into(),
    };
    (toolbar, dsh)
}

fn toolbar_html() -> String {
    format!(
        "{TOOLBAR_HTML_PREFIX}{TOOLBAR_DRAG_STYLE}{TOOLBAR_HTML_STYLE_SUFFIX}{TOOLBAR_BRAND_ICON}{TOOLBAR_HTML_BRAND_SUFFIX}{TOOLBAR_ACTIONS}{TOOLBAR_HTML_SCRIPT_PREFIX}{TOOLBAR_DRAG_SCRIPT}{TOOLBAR_HTML_SUFFIX}"
    )
}

fn loading_html() -> String {
    format!(
        r#"<!doctype html>
<html><head><meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'">
<style>
*{{box-sizing:border-box}}html,body{{margin:0;width:100%;height:100%;overflow:hidden;background:#f7f8fa;color:#171717;font-family:system-ui,-apple-system,"Segoe UI",sans-serif}}
body{{display:grid;place-items:center}}.loading{{display:flex;flex-direction:column;align-items:center;text-align:center}}
.mark{{width:58px;height:50px;color:#111;animation:pulse 1.8s ease-in-out infinite}}.mark svg{{display:block;width:100%;height:100%}}.mark path{{fill:currentColor}}
h1{{margin:20px 0 7px;font-size:20px;line-height:1.25;font-weight:650;letter-spacing:.01em}}p{{margin:0;color:#6b7280;font-size:13px}}
@keyframes pulse{{0%,100%{{opacity:.62;transform:scale(.98)}}50%{{opacity:1;transform:scale(1)}}}}
</style></head><body><main class="loading" role="status" aria-live="polite"><div class="mark">{TOOLBAR_BRAND_ICON}</div><h1>DeepSeek Harness</h1><p>正在启动…</p></main></body></html>"#
    )
}

fn maximize_state_script(maximized: bool) -> &'static str {
    if maximized {
        "window.setMaximized?.(true);"
    } else {
        "window.setMaximized?.(false);"
    }
}

const TOOLBAR_HTML_PREFIX: &str = r#"<!doctype html>
<html><head><meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'">
<style>
*{box-sizing:border-box}html,body{margin:0;height:100%;overflow:hidden;font:13px system-ui,sans-serif;color:#dce8ff;background:#101726}
#bar{position:relative;height:100%;display:flex;align-items:center;border-bottom:1px solid #27344d;user-select:none}
#drag{height:100%;flex:1;cursor:default;"#;

#[cfg(windows)]
const TOOLBAR_DRAG_STYLE: &str = "app-region:drag";

#[cfg(not(windows))]
const TOOLBAR_DRAG_STYLE: &str = "";

const TOOLBAR_HTML_STYLE_SUFFIX: &str = r#"}
#identity{position:absolute;left:50%;transform:translateX(-50%);display:flex;align-items:center;gap:9px;pointer-events:none;white-space:nowrap}
#mark{width:18px;height:18px;display:flex;align-items:center;justify-content:center}#mark svg{width:18px;height:auto}#mark path{fill:currentColor}.name{font-weight:600}
.actions{z-index:1;height:100%;display:flex}button{--button-bg:#101726;width:44px;height:100%;border:0;color:#c9d6ec;background:var(--button-bg);font:16px system-ui;cursor:default}
button:hover{--button-bg:#25314a}button.close:hover{--button-bg:#c42b1c;color:white}
.caption-icon{position:relative;display:block;width:10px;height:10px;margin:auto}.caption-icon::before{content:"";position:absolute;inset:0;border:1px solid currentColor}
#maximize-button.restore .caption-icon::before{z-index:2;left:0;top:3px;width:8px;height:8px;background:var(--button-bg)}#maximize-button.restore .caption-icon::after{content:"";position:absolute;left:3px;top:0;width:8px;height:8px;border:1px solid currentColor}
</style></head><body><div id="bar"><div id="drag"></div><div id="identity"><span id="mark">"#;

const TOOLBAR_BRAND_ICON: &str = include_str!("../../../assets/icons/deepseek-black.svg");

const TOOLBAR_HTML_BRAND_SUFFIX: &str =
    r#"</span><span class="name">DeepSeek Harness</span></div><div class="actions">"#;

#[cfg(windows)]
const TOOLBAR_ACTIONS: &str = r#"<button data-action="refresh" title="Refresh" aria-label="Refresh">&#x21bb;</button><button data-action="open-browser" title="Open in browser" aria-label="Open in browser">&#x2197;</button><button data-action="minimize" title="Minimize" aria-label="Minimize">&#x2212;</button><button id="maximize-button" data-action="toggle-maximize" title="Maximize" aria-label="Maximize"><span class="caption-icon" aria-hidden="true"></span></button><button class="close" data-action="close" title="Close" aria-label="Close">&#xd7;</button>"#;

#[cfg(not(windows))]
const TOOLBAR_ACTIONS: &str = r#"<button data-action="refresh" title="Refresh" aria-label="Refresh">&#x21bb;</button><button data-action="open-browser" title="Open in browser" aria-label="Open in browser">&#x2197;</button>"#;

const TOOLBAR_HTML_SCRIPT_PREFIX: &str = r#"</div></div>
<script>
const send=value=>window.ipc.postMessage(value);
"#;

#[cfg(windows)]
const TOOLBAR_DRAG_SCRIPT: &str = "";

#[cfg(not(windows))]
const TOOLBAR_DRAG_SCRIPT: &str = r#"
const drag=document.getElementById('drag');
const DRAG_THRESHOLD=4;
let dragOrigin=null;
drag.addEventListener('mousedown',event=>{if(event.button===0)dragOrigin={x:event.screenX,y:event.screenY}});
drag.addEventListener('mousemove',event=>{if(!dragOrigin||(event.buttons&1)===0)return;const dx=event.screenX-dragOrigin.x;const dy=event.screenY-dragOrigin.y;if(Math.hypot(dx,dy)<DRAG_THRESHOLD)return;dragOrigin=null;send('start-drag')});
drag.addEventListener('mouseup',()=>{dragOrigin=null});
drag.addEventListener('mouseleave',event=>{if((event.buttons&1)===0)dragOrigin=null});
drag.addEventListener('dblclick',event=>{if(event.button===0){dragOrigin=null;send('toggle-maximize')}});
"#;

const TOOLBAR_HTML_SUFFIX: &str = r#"
document.querySelectorAll('[data-action]').forEach(button=>button.addEventListener('click',()=>send(button.dataset.action)));
window.setMaximized=maximized=>{const button=document.getElementById('maximize-button');if(!button)return;button.classList.toggle('restore',maximized);const label=maximized?'Restore':'Maximize';button.title=label;button.setAttribute('aria-label',label)};
</script></body></html>"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn allowed() -> AllowedOrigin {
        AllowedOrigin::from_loopback(
            &LoopbackUrl::parse("http://127.0.0.1:43127").expect("trusted readiness URL"),
        )
    }

    #[test]
    fn allows_paths_on_the_controller_validated_origin() {
        for requested in [
            "http://127.0.0.1:43127/",
            "http://127.0.0.1:43127/chat/one?model=dsh#latest",
        ] {
            assert_eq!(allowed().decide(requested), NavigationDecision::Allow);
        }
    }

    #[test]
    fn turns_only_non_loopback_http_links_into_external_urls() {
        for requested in ["https://example.com/docs?q=dsh", "http://192.0.2.10/path"] {
            assert_eq!(
                allowed().decide(requested),
                NavigationDecision::OpenExternal(ExternalUrl::parse(requested).unwrap())
            );
        }
    }

    #[test]
    fn rejects_other_loopback_origins_credentials_and_unknown_schemes() {
        for requested in [
            "http://127.0.0.1:43128/",
            "http://127.0.0.2:43127/",
            "http://localhost:43127/",
            "http://app.localhost:43127/",
            "http://[::1]:43127/",
            "https://[::ffff:127.0.0.1]/",
            "http://user@127.0.0.1:43127/",
            "https://user@example.com/",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "not a URL",
        ] {
            assert_eq!(
                allowed().decide(requested),
                NavigationDecision::Reject,
                "unexpected decision for {requested}"
            );
        }
    }

    #[test]
    fn loading_surface_allows_only_its_internal_html_document() {
        for requested in ["about:blank", "data:text/html,hello"] {
            assert_eq!(
                decide_surface_navigation(None, requested),
                NavigationDecision::Allow
            );
        }
        for requested in [
            "http://127.0.0.1:43127/",
            "https://example.com/",
            "data:text/plain,hello",
            "data:application/javascript,alert(1)",
            "file:///etc/passwd",
        ] {
            assert_eq!(
                decide_surface_navigation(None, requested),
                NavigationDecision::Reject,
                "unexpected loading-page decision for {requested}"
            );
        }
    }

    #[test]
    fn preserves_an_explicit_default_port_in_the_allowed_origin() {
        let origin = AllowedOrigin::from_loopback(
            &LoopbackUrl::parse("http://127.0.0.1:80").expect("trusted readiness URL"),
        );

        assert_eq!(
            origin.decide("http://127.0.0.1/dashboard"),
            NavigationDecision::Allow
        );
    }

    #[test]
    fn parses_only_known_toolbar_actions() {
        assert_eq!(toolbar_action("refresh"), Some(SurfaceAction::Refresh));
        assert_eq!(
            toolbar_action("open-browser"),
            Some(SurfaceAction::OpenInBrowser)
        );
        assert_eq!(toolbar_action("unknown"), None);
    }

    #[test]
    fn toolbar_uses_the_full_centered_brand_and_official_monochrome_icon() {
        let html = toolbar_html();

        assert_eq!(WINDOW_TITLE, "DeepSeek Harness");
        assert!(html.contains("DeepSeek Harness"));
        assert!(html.contains("viewBox=\"0 0 27 23\""));
        assert!(html.contains("left:50%;transform:translateX(-50%)"));
        assert!(!html.contains(">DSH<"));
        assert!(!html.contains("#4d8dff"));
    }

    #[test]
    fn loading_page_is_local_centered_and_uses_the_monochrome_brand() {
        let html = loading_html();

        assert!(html.contains("DeepSeek Harness"));
        assert!(html.contains("正在启动…"));
        assert!(html.contains("viewBox=\"0 0 27 23\""));
        assert!(html.contains("place-items:center"));
        assert!(html.contains("default-src 'none'"));
        assert!(!html.contains("src=\"http"));
        assert!(!html.contains("href=\"http"));
        assert!(!html.contains("#4d8dff"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_maximize_button_switches_to_the_restore_state() {
        let html = toolbar_html();

        assert!(html.contains("id=\"maximize-button\""));
        assert!(html.contains("window.setMaximized"));
        assert_eq!(
            maximize_state_script(false),
            "window.setMaximized?.(false);"
        );
        assert_eq!(maximize_state_script(true), "window.setMaximized?.(true);");
    }

    #[cfg(windows)]
    #[test]
    fn windows_header_uses_the_native_non_client_drag_region() {
        let html = toolbar_html();

        assert!(html.contains("app-region:drag"));
        assert!(!html.contains("send('start-drag')"));
        assert!(!html.contains("addEventListener('dblclick'"));
    }

    #[test]
    fn surface_window_icon_uses_the_packaged_monochrome_deepseek_asset() {
        let (rgba, width, height) = surface_window_icon_rgba().expect("decode Surface icon");
        let visible = rgba
            .chunks_exact(4)
            .filter(|pixel| pixel[3] > 128)
            .collect::<Vec<_>>();

        assert_eq!((width, height), (225, 225));
        assert!(!visible.is_empty());
        assert!(
            visible
                .iter()
                .all(|pixel| pixel[0] == pixel[1] && pixel[1] == pixel[2])
        );
    }

    #[test]
    fn splits_toolbar_and_content_without_overlap() {
        let (toolbar, dsh) = webview_bounds(PhysicalSize::new(1200, 800), 2.0);

        assert_eq!(toolbar.position, PhysicalPosition::new(0, 0).into());
        assert_eq!(toolbar.size, PhysicalSize::new(1200, 84).into());
        assert_eq!(dsh.position, PhysicalPosition::new(0, 84).into());
        assert_eq!(dsh.size, PhysicalSize::new(1200, 716).into());
    }
}

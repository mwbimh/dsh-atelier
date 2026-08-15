//! Operating-system adapters used by the controller.
//!
//! These types are deliberately separate from the controller so unit tests can
//! replace every desktop effect with a fake implementation.

use std::path::{Path, PathBuf};

use crate::ports::PlatformError;

#[cfg(windows)]
pub use windows::{WindowsAutostart, WindowsBrowser, WindowsNotifier};
#[cfg(windows)]
pub type NativeBrowser = WindowsBrowser;
#[cfg(windows)]
pub type NativeNotifier = WindowsNotifier;
#[cfg(windows)]
pub type NativeAutostart = WindowsAutostart;

#[cfg(target_os = "macos")]
pub use macos::{MacAutostart, MacBrowser, MacNotifier};
#[cfg(target_os = "macos")]
pub type NativeBrowser = MacBrowser;
#[cfg(target_os = "macos")]
pub type NativeNotifier = MacNotifier;
#[cfg(target_os = "macos")]
pub type NativeAutostart = MacAutostart;

fn io_error(action: &str, error: std::io::Error) -> PlatformError {
    PlatformError::new(format!("{action}: {error}"))
}

#[must_use]
pub fn portable_bootstrap_sibling(runtime_executable: &Path) -> Option<PathBuf> {
    let directory = runtime_executable.parent()?;
    #[cfg(target_os = "macos")]
    let names = vec![
        "DSH Atelier".to_owned(),
        format!("dsh-atelier{}", std::env::consts::EXE_SUFFIX),
    ];
    #[cfg(not(target_os = "macos"))]
    let names = vec![format!("dsh-atelier{}", std::env::consts::EXE_SUFFIX)];
    names
        .into_iter()
        .map(|name| directory.join(name))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod portable_tests {
    use std::fs;

    use super::portable_bootstrap_sibling;

    #[test]
    fn locates_a_portable_bootstrap_beside_the_runtime() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = directory.path().join(format!(
            "dsh-atelier-runtime{}",
            std::env::consts::EXE_SUFFIX
        ));
        let bootstrap_name = if cfg!(target_os = "macos") {
            "DSH Atelier".to_owned()
        } else {
            format!("dsh-atelier{}", std::env::consts::EXE_SUFFIX)
        };
        let bootstrap = directory.path().join(bootstrap_name);
        fs::write(&runtime, []).unwrap();
        fs::write(&bootstrap, []).unwrap();

        assert_eq!(portable_bootstrap_sibling(&runtime), Some(bootstrap));
    }
}

#[cfg(windows)]
mod windows {
    use std::{
        env, fs,
        path::{Path, PathBuf},
    };

    use windows::{
        UI::Notifications::{ToastNotification, ToastNotificationManager, ToastTemplateType},
        Win32::{
            Foundation::RPC_E_CHANGED_MODE,
            System::Com::{
                CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
                CoUninitialize, IPersistFile,
            },
            UI::{
                Shell::{IShellLinkW, ShellExecuteW, ShellLink},
                WindowsAndMessaging::SW_SHOWNORMAL,
            },
        },
        core::{HSTRING, Interface, PCWSTR},
    };

    use super::{PlatformError, io_error};
    use crate::{
        dsh::readiness::LoopbackUrl,
        ports::{Autostart, Browser, Notifier},
        surface::ExternalUrl,
    };

    const SHORTCUT_NAME: &str = "DSH Atelier.lnk";
    const APP_ID: &str = "DSH.Atelier";

    #[derive(Clone, Copy, Debug, Default)]
    pub struct WindowsBrowser;

    impl Browser for WindowsBrowser {
        fn open(&self, url: &LoopbackUrl) -> Result<(), PlatformError> {
            open_in_default_browser(url.as_str())
        }

        fn open_external(&self, url: &ExternalUrl) -> Result<(), PlatformError> {
            open_in_default_browser(url.as_str())
        }
    }

    fn open_in_default_browser(value: &str) -> Result<(), PlatformError> {
        let url = HSTRING::from(value);
        // SAFETY: all pointers are borrowed from live HSTRING values for the
        // duration of this synchronous call. Callers only pass URL wrappers
        // that validate either the managed DSH origin or an external HTTP(S) URL.
        let result = unsafe {
            ShellExecuteW(
                None,
                windows::core::w!("open"),
                PCWSTR(url.as_ptr()),
                None,
                None,
                SW_SHOWNORMAL,
            )
        };
        let code = result.0 as isize;
        if code > 32 {
            Ok(())
        } else {
            Err(PlatformError::new(format!(
                "failed to open the default browser (ShellExecuteW returned {code})"
            )))
        }
    }

    #[derive(Clone, Copy, Debug, Default)]
    pub struct WindowsNotifier;

    impl Notifier for WindowsNotifier {
        fn notify(&self, title: &str, body: &str) -> Result<(), PlatformError> {
            show_toast(title, body).map_err(|error| {
                PlatformError::new(format!("failed to show a Windows toast: {error}"))
            })
        }
    }

    fn show_toast(title: &str, body: &str) -> windows::core::Result<()> {
        let content = ToastNotificationManager::GetTemplateContent(ToastTemplateType::ToastText02)?;
        let text_nodes = content.GetElementsByTagName(&HSTRING::from("text"))?;
        let title_node = text_nodes.Item(0)?;
        title_node.AppendChild(&content.CreateTextNode(&HSTRING::from(title))?)?;
        let body_node = text_nodes.Item(1)?;
        body_node.AppendChild(&content.CreateTextNode(&HSTRING::from(body))?)?;

        let toast = ToastNotification::CreateToastNotification(&content)?;
        let notifier = ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(APP_ID))?;
        notifier.Show(&toast)
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct WindowsAutostart {
        executable: PathBuf,
        shortcut_path: PathBuf,
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct ShortcutSpec {
        executable: PathBuf,
        arguments: String,
        working_directory: PathBuf,
        shortcut_path: PathBuf,
    }

    impl WindowsAutostart {
        pub fn new(executable: impl Into<PathBuf>) -> Result<Self, PlatformError> {
            let app_data = env::var_os("APPDATA").ok_or_else(|| {
                PlatformError::new("APPDATA is unavailable; cannot locate the Startup directory")
            })?;
            let startup_directory = PathBuf::from(app_data)
                .join("Microsoft")
                .join("Windows")
                .join("Start Menu")
                .join("Programs")
                .join("Startup");

            Ok(Self::from_paths(executable, startup_directory))
        }

        pub fn from_paths(
            executable: impl Into<PathBuf>,
            startup_directory: impl AsRef<Path>,
        ) -> Self {
            Self {
                executable: executable.into(),
                shortcut_path: startup_directory.as_ref().join(SHORTCUT_NAME),
            }
        }

        pub fn shortcut_path(&self) -> &Path {
            &self.shortcut_path
        }

        fn shortcut_spec(&self) -> Result<ShortcutSpec, PlatformError> {
            let working_directory = self.executable.parent().ok_or_else(|| {
                PlatformError::new("the autostart executable has no parent directory")
            })?;
            Ok(ShortcutSpec {
                executable: self.executable.clone(),
                arguments: "--autostart".to_owned(),
                working_directory: working_directory.to_owned(),
                shortcut_path: self.shortcut_path.clone(),
            })
        }

        fn enable(&self) -> Result<(), PlatformError> {
            if !self.executable.is_file() {
                return Err(PlatformError::new(format!(
                    "autostart target is not a file: {}",
                    self.executable.display()
                )));
            }
            let startup_directory = self.shortcut_path.parent().ok_or_else(|| {
                PlatformError::new("the autostart shortcut has no parent directory")
            })?;
            fs::create_dir_all(startup_directory)
                .map_err(|error| io_error("failed to create the Startup directory", error))?;

            let spec = self.shortcut_spec()?;
            create_shortcut(&spec).map_err(|error| {
                PlatformError::new(format!("failed to create the autostart shortcut: {error}"))
            })
        }

        fn disable(&self) -> Result<(), PlatformError> {
            match fs::remove_file(&self.shortcut_path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(io_error("failed to remove the autostart shortcut", error)),
            }
        }
    }

    impl Autostart for WindowsAutostart {
        fn is_enabled(&self) -> Result<bool, PlatformError> {
            match fs::metadata(&self.shortcut_path) {
                Ok(metadata) => Ok(metadata.is_file()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(error) => Err(io_error("failed to inspect the autostart shortcut", error)),
            }
        }

        fn set_enabled(&self, enabled: bool) -> Result<(), PlatformError> {
            if enabled {
                self.enable()
            } else {
                self.disable()
            }
        }
    }

    fn create_shortcut(spec: &ShortcutSpec) -> windows::core::Result<()> {
        let _com = ComApartment::enter()?;
        // SAFETY: COM is initialized for this thread by `ComApartment`, the
        // class and requested interface are the documented Shell Link pair.
        let link: IShellLinkW =
            unsafe { CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)? };
        let executable = HSTRING::from(spec.executable.as_os_str());
        let arguments = HSTRING::from(&spec.arguments);
        let working_directory = HSTRING::from(spec.working_directory.as_os_str());
        // SAFETY: the HSTRING arguments remain alive for every synchronous COM
        // call, and the shell link object is owned by this function.
        unsafe {
            link.SetPath(&executable)?;
            link.SetArguments(&arguments)?;
            link.SetDescription(&HSTRING::from("Start DSH Atelier at login"))?;
            link.SetIconLocation(&executable, 0)?;
            link.SetWorkingDirectory(&working_directory)?;
        }
        let persist: IPersistFile = link.cast()?;
        let shortcut_path = HSTRING::from(spec.shortcut_path.as_os_str());
        // SAFETY: the path is a valid, null-safe HSTRING and `persist` is the
        // IPersistFile interface queried from the live Shell Link object.
        unsafe { persist.Save(&shortcut_path, true) }
    }

    struct ComApartment {
        must_uninitialize: bool,
    }

    impl ComApartment {
        fn enter() -> windows::core::Result<Self> {
            // SAFETY: the call initializes COM only for the current thread. The
            // matching `CoUninitialize` is handled by Drop when initialization
            // succeeds. RPC_E_CHANGED_MODE means this thread is already usable.
            let result = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
            if result.is_ok() {
                Ok(Self {
                    must_uninitialize: true,
                })
            } else if result == RPC_E_CHANGED_MODE {
                Ok(Self {
                    must_uninitialize: false,
                })
            } else {
                Err(result.into())
            }
        }
    }

    impl Drop for ComApartment {
        fn drop(&mut self) {
            if self.must_uninitialize {
                // SAFETY: paired with a successful CoInitializeEx call on this
                // same thread and executed before the apartment guard is dropped.
                unsafe { CoUninitialize() };
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use std::path::Path;

        use tempfile::tempdir;

        use super::WindowsAutostart;
        use crate::ports::Autostart;

        #[test]
        fn uses_a_fixed_shortcut_inside_the_injected_startup_directory() {
            let adapter = WindowsAutostart::from_paths(
                "C:/portable/dsh-atelier.exe",
                "C:/Users/test/AppData/Roaming/Microsoft/Windows/Start Menu/Programs/Startup",
            );

            assert_eq!(
                adapter.shortcut_path(),
                Path::new(
                    "C:/Users/test/AppData/Roaming/Microsoft/Windows/Start Menu/Programs/Startup/DSH Atelier.lnk"
                )
            );
        }

        #[test]
        fn constructs_structured_shortcut_values_without_a_shell_command() {
            let adapter = WindowsAutostart::from_paths(
                "C:/Portable & Tools/DSH Atelier/dsh-atelier.exe",
                "C:/Startup Folder",
            );

            let spec = adapter.shortcut_spec().expect("shortcut specification");

            assert_eq!(
                spec.executable,
                Path::new("C:/Portable & Tools/DSH Atelier/dsh-atelier.exe")
            );
            assert_eq!(spec.arguments, "--autostart");
            assert_eq!(
                spec.working_directory,
                Path::new("C:/Portable & Tools/DSH Atelier")
            );
            assert_eq!(
                spec.shortcut_path,
                Path::new("C:/Startup Folder/DSH Atelier.lnk")
            );
        }

        #[test]
        fn enabling_creates_a_shortcut_only_in_the_injected_directory() {
            let startup = tempdir().expect("temporary startup directory");
            let adapter = WindowsAutostart::from_paths(
                std::env::current_exe().expect("current test executable"),
                startup.path(),
            );

            adapter.set_enabled(true).expect("enable autostart");

            assert!(adapter.shortcut_path().is_file());
            assert!(adapter.is_enabled().expect("autostart status"));
        }

        #[test]
        fn disabling_removes_only_the_atelier_shortcut() {
            let startup = tempdir().expect("temporary startup directory");
            let shortcut = startup.path().join("DSH Atelier.lnk");
            let unrelated = startup.path().join("Unrelated.lnk");
            std::fs::write(&shortcut, b"placeholder").expect("Atelier shortcut fixture");
            std::fs::write(&unrelated, b"keep").expect("unrelated shortcut fixture");
            let adapter =
                WindowsAutostart::from_paths("C:/portable/dsh-atelier.exe", startup.path());

            assert!(adapter.is_enabled().expect("autostart status"));
            adapter.set_enabled(false).expect("disable autostart");

            assert!(!shortcut.exists());
            assert!(unrelated.exists());
            assert!(!adapter.is_enabled().expect("autostart status"));
        }
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::{
        env, fs,
        path::{Path, PathBuf},
        process::Command,
    };

    use super::{PlatformError, io_error};
    use crate::{
        dsh::readiness::LoopbackUrl,
        ports::{Autostart, Browser, Notifier},
        surface::ExternalUrl,
    };

    const BUNDLE_IDENTIFIER: &str = "com.deepseek-harness.atelier";
    const LAUNCH_AGENT_NAME: &str = "com.deepseek-harness.atelier.plist";

    #[derive(Clone, Copy, Debug, Default)]
    pub struct MacBrowser;

    impl Browser for MacBrowser {
        fn open(&self, url: &LoopbackUrl) -> Result<(), PlatformError> {
            open_in_default_browser(url.as_str())
        }

        fn open_external(&self, url: &ExternalUrl) -> Result<(), PlatformError> {
            open_in_default_browser(url.as_str())
        }
    }

    fn open_in_default_browser(value: &str) -> Result<(), PlatformError> {
        run_command(&mut browser_command(value), "open the default browser")
    }

    fn browser_command(url: &str) -> Command {
        let mut command = Command::new("/usr/bin/open");
        command.arg(url);
        command
    }

    #[derive(Clone, Copy, Debug, Default)]
    pub struct MacNotifier;

    impl Notifier for MacNotifier {
        fn notify(&self, title: &str, body: &str) -> Result<(), PlatformError> {
            run_command(
                &mut notification_command(title, body),
                "show a macOS notification",
            )
        }
    }

    fn notification_command(title: &str, body: &str) -> Command {
        let mut command = Command::new("/usr/bin/osascript");
        command
            .arg("-e")
            .arg("on run argv")
            .arg("-e")
            .arg("display notification (item 2 of argv) with title (item 1 of argv)")
            .arg("-e")
            .arg("end run")
            .arg("--")
            .arg(title)
            .arg(body);
        command
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct MacAutostart {
        executable: PathBuf,
        launch_agent_path: PathBuf,
    }

    impl MacAutostart {
        pub fn new(executable: impl Into<PathBuf>) -> Result<Self, PlatformError> {
            let home = env::var_os("HOME").ok_or_else(|| {
                PlatformError::new("HOME is unavailable; cannot locate the LaunchAgents directory")
            })?;
            Ok(Self::from_paths(
                executable,
                PathBuf::from(home).join("Library").join("LaunchAgents"),
            ))
        }

        pub fn from_paths(
            executable: impl Into<PathBuf>,
            launch_agents_directory: impl AsRef<Path>,
        ) -> Self {
            Self {
                executable: executable.into(),
                launch_agent_path: launch_agents_directory.as_ref().join(LAUNCH_AGENT_NAME),
            }
        }

        pub fn launch_agent_path(&self) -> &Path {
            &self.launch_agent_path
        }

        fn enable(&self) -> Result<(), PlatformError> {
            if !self.executable.is_file() {
                return Err(PlatformError::new(format!(
                    "autostart target is not a file: {}",
                    self.executable.display()
                )));
            }
            let directory = self.launch_agent_path.parent().ok_or_else(|| {
                PlatformError::new("the LaunchAgent plist has no parent directory")
            })?;
            fs::create_dir_all(directory)
                .map_err(|error| io_error("failed to create the LaunchAgents directory", error))?;
            let temporary = self.launch_agent_path.with_extension("plist.tmp");
            fs::write(&temporary, launch_agent_plist(&self.executable))
                .map_err(|error| io_error("failed to write the LaunchAgent plist", error))?;
            if let Err(error) = fs::rename(&temporary, &self.launch_agent_path) {
                let _ = fs::remove_file(&temporary);
                return Err(io_error("failed to activate the LaunchAgent plist", error));
            }
            Ok(())
        }

        fn disable(&self) -> Result<(), PlatformError> {
            match fs::remove_file(&self.launch_agent_path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(io_error("failed to remove the LaunchAgent plist", error)),
            }
        }
    }

    impl Autostart for MacAutostart {
        fn is_enabled(&self) -> Result<bool, PlatformError> {
            match fs::metadata(&self.launch_agent_path) {
                Ok(metadata) => Ok(metadata.is_file()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(error) => Err(io_error("failed to inspect the LaunchAgent plist", error)),
            }
        }

        fn set_enabled(&self, enabled: bool) -> Result<(), PlatformError> {
            if enabled {
                self.enable()
            } else {
                self.disable()
            }
        }
    }

    fn run_command(command: &mut Command, action: &str) -> Result<(), PlatformError> {
        let output = command
            .output()
            .map_err(|error| io_error(&format!("failed to {action}"), error))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(PlatformError::new(format!(
                "failed to {action}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )))
        }
    }

    fn launch_agent_plist(executable: &Path) -> String {
        let arguments = launch_agent_program_arguments(executable)
            .into_iter()
            .map(|argument| format!("<string>{}</string>", escape_xml(&argument)))
            .collect::<String>();
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
             \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\">\n\
             <dict>\n\
               <key>Label</key><string>{BUNDLE_IDENTIFIER}</string>\n\
               <key>ProgramArguments</key>\n\
               <array>{arguments}</array>\n\
               <key>RunAtLoad</key><true/>\n\
               <key>KeepAlive</key><false/>\n\
               <key>LimitLoadToSessionType</key><string>Aqua</string>\n\
               <key>ProcessType</key><string>Interactive</string>\n\
             </dict>\n\
             </plist>\n"
        )
    }

    fn launch_agent_program_arguments(executable: &Path) -> Vec<String> {
        if let Some(app_bundle) = app_bundle_for_executable(executable) {
            vec![
                "/usr/bin/open".to_owned(),
                "-n".to_owned(),
                app_bundle.to_string_lossy().into_owned(),
                "--args".to_owned(),
                "--autostart".to_owned(),
            ]
        } else {
            vec![
                executable.to_string_lossy().into_owned(),
                "--autostart".to_owned(),
            ]
        }
    }

    fn app_bundle_for_executable(executable: &Path) -> Option<&Path> {
        let macos = executable.parent()?;
        if macos.file_name()? != "MacOS" {
            return None;
        }
        let contents = macos.parent()?;
        if contents.file_name()? != "Contents" {
            return None;
        }
        let app = contents.parent()?;
        (app.extension()? == "app").then_some(app)
    }

    fn escape_xml(value: &str) -> String {
        value
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&apos;")
    }

    #[cfg(test)]
    mod tests {
        use std::{fs, path::Path};

        use tempfile::tempdir;

        use super::{
            MacAutostart, browser_command, launch_agent_plist, launch_agent_program_arguments,
            notification_command,
        };
        use crate::ports::Autostart;
        use crate::{dsh::readiness::LoopbackUrl, surface::ExternalUrl};

        #[test]
        fn bundled_login_launch_uses_launch_services_and_the_bundle_identifier() {
            let executable =
                Path::new("/Applications/DSH & Atelier.app/Contents/MacOS/DSH Atelier");

            assert_eq!(
                launch_agent_program_arguments(executable),
                [
                    "/usr/bin/open",
                    "-n",
                    "/Applications/DSH & Atelier.app",
                    "--args",
                    "--autostart",
                ]
            );
            let plist = launch_agent_plist(executable);
            assert!(plist.contains("com.deepseek-harness.atelier"));
            assert!(plist.contains("/Applications/DSH &amp; Atelier.app"));
            assert!(plist.contains("<string>Aqua</string>"));
        }

        #[test]
        fn enabling_and_disabling_only_changes_the_injected_launch_agent() {
            let directory = tempdir().unwrap();
            let executable = directory.path().join("dsh-atelier");
            fs::write(&executable, []).unwrap();
            let adapter = MacAutostart::from_paths(&executable, directory.path());

            adapter.set_enabled(true).unwrap();
            assert!(adapter.is_enabled().unwrap());
            assert!(adapter.launch_agent_path().is_file());
            adapter.set_enabled(true).unwrap();

            adapter.set_enabled(false).unwrap();
            assert!(!adapter.is_enabled().unwrap());
        }

        #[test]
        fn browser_command_opens_only_the_validated_loopback_url() {
            let url = LoopbackUrl::parse("http://127.0.0.1:24888").unwrap();
            let command = browser_command(url.as_str());

            assert_eq!(command.get_program(), "/usr/bin/open");
            assert_eq!(
                command.get_args().collect::<Vec<_>>(),
                [std::ffi::OsStr::new("http://127.0.0.1:24888/")]
            );
        }

        #[test]
        fn browser_command_opens_validated_external_urls_without_a_shell() {
            let url = ExternalUrl::parse("https://example.com/docs?q=surface").unwrap();
            let command = browser_command(url.as_str());

            assert_eq!(command.get_program(), "/usr/bin/open");
            assert_eq!(
                command.get_args().collect::<Vec<_>>(),
                [std::ffi::OsStr::new("https://example.com/docs?q=surface")]
            );
        }

        #[test]
        fn notification_text_is_passed_as_argv_not_applescript_source() {
            let command = notification_command("title \"quoted\"", "body; do shell script");
            let arguments = command.get_args().collect::<Vec<_>>();

            assert_eq!(command.get_program(), "/usr/bin/osascript");
            assert_eq!(arguments[arguments.len() - 2], "title \"quoted\"");
            assert_eq!(arguments[arguments.len() - 1], "body; do shell script");
        }
    }
}

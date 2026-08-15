#[cfg(windows)]
#[test]
fn runtime_binary_embeds_an_application_icon() {
    use windows::{
        Win32::UI::Shell::ExtractIconExW,
        core::{HSTRING, PCWSTR},
    };

    let executable = HSTRING::from(env!("CARGO_BIN_EXE_dsh-atelier-runtime"));
    let count = unsafe { ExtractIconExW(PCWSTR(executable.as_ptr()), -1, None, None, 0) };

    assert!(
        count > 0,
        "Runtime executable does not contain an icon resource"
    );
}

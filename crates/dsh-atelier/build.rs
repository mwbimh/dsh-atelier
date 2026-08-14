fn main() {
    #[cfg(windows)]
    {
        const ICON: &str = "../../assets/icons/deepseek-blue.ico";
        println!("cargo:rerun-if-changed={ICON}");
        winresource::WindowsResource::new()
            .set_icon(ICON)
            .compile()
            .expect("embed the DeepSeek blue Windows icon");
    }
}

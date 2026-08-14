use std::path::Path;

use image::ImageFormat;

fn main() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let source = repository.join("assets/icons/deepseek-blue.ico");
    let destination = repository.join("assets/icons/deepseek-black.ico");
    let mut icon = image::open(source)
        .expect("decode official DeepSeek favicon")
        .to_rgba8();
    for pixel in icon.pixels_mut() {
        if pixel[3] != 0 {
            pixel[0] = 0;
            pixel[1] = 0;
            pixel[2] = 0;
        }
    }
    icon.save_with_format(destination, ImageFormat::Ico)
        .expect("write black DeepSeek ICO");
}

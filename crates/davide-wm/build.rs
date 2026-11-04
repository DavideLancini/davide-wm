//! Embeds the wallpaper named in `config::WALLPAPER` (empty when missing).

fn main() {
    let out = std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("wallpaper.jpg");
    println!("cargo:rerun-if-changed={}", config::WALLPAPER);
    let data = std::fs::read(config::WALLPAPER).unwrap_or_else(|_| {
        println!("cargo:warning=wallpaper {} not found, using a plain background", config::WALLPAPER);
        Vec::new()
    });
    std::fs::write(out, data).unwrap();
}

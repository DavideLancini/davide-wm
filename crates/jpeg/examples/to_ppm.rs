//! Decodes a JPEG into a binary PPM: `to_ppm in.jpg out.ppm`.
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data = std::fs::read(&args[1]).unwrap();
    let t = std::time::Instant::now();
    let img = jpeg::decode(&data).unwrap();
    eprintln!("{}x{} in {:?}", img.width, img.height, t.elapsed());
    let mut out = format!("P6\n{} {}\n255\n", img.width, img.height).into_bytes();
    for p in img.pixels.chunks(4) {
        out.extend_from_slice(&p[..3]);
    }
    std::fs::write(&args[2], out).unwrap();
}

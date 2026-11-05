//! Publishes a moving test pattern; prints the node id.
fn main() {
    let pw = pipewire::PipeWire::connect().unwrap();
    let (w, h) = (320u32, 240u32);
    let mut s = pipewire::Stream::new(&pw, "davide-wm-test", w, h, 30).unwrap();
    let mut printed = false;
    let mut n = 0u32;
    loop {
        let mut p = libc::pollfd { fd: pw.raw_fd(), events: libc::POLLIN, revents: 0 };
        unsafe { libc::poll(&mut p, 1, 10) };
        pw.dispatch();
        if !printed {
            if let Some(id) = s.node_id() {
                println!("node {id}");
                printed = true;
            }
        }
        if s.wants_frame() {
            n += 1;
            let frame: Vec<u8> = (0..w * h).flat_map(|i| [((i % w + n) % 256) as u8, 0, ((i / w) % 256) as u8, 255]).collect();
            s.push(frame);
        }
        if let Some(e) = s.error() {
            eprintln!("error {e}");
            break;
        }
    }
}

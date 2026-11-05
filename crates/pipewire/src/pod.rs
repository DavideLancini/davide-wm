//! The few SPA pods a video source needs, encoded by hand.
//!
//! A pod is a (size, type) header and a body padded to 8 bytes; `size`
//! counts the body without that padding. Objects hold (key, flags, pod)
//! properties, choices hold a child header and the bare values.

const ID: u32 = 3;
const INT: u32 = 4;
const RECTANGLE: u32 = 10;
const FRACTION: u32 = 11;
const OBJECT: u32 = 15;
const CHOICE: u32 = 19;

const CHOICE_RANGE: u32 = 1;
const CHOICE_FLAGS: u32 = 4;

const OBJECT_FORMAT: u32 = 0x40003;
const OBJECT_PARAM_BUFFERS: u32 = 0x40004;
const PARAM_ENUM_FORMAT: u32 = 3;
const PARAM_BUFFERS: u32 = 5;

const FORMAT_MEDIA_TYPE: u32 = 1;
const FORMAT_MEDIA_SUBTYPE: u32 = 2;
const FORMAT_VIDEO_FORMAT: u32 = 0x20001;
const FORMAT_VIDEO_SIZE: u32 = 0x20003;
const FORMAT_VIDEO_FRAMERATE: u32 = 0x20004;
const FORMAT_VIDEO_MAX_FRAMERATE: u32 = 0x20005;
const MEDIA_TYPE_VIDEO: u32 = 2;
const MEDIA_SUBTYPE_RAW: u32 = 1;
/// Bytes R, G, B, x: what glReadPixels gives with GL_RGBA.
const VIDEO_FORMAT_RGBX: u32 = 7;

const BUFFERS_BUFFERS: u32 = 1;
const BUFFERS_BLOCKS: u32 = 2;
const BUFFERS_SIZE: u32 = 3;
const BUFFERS_STRIDE: u32 = 4;
const BUFFERS_DATA_TYPE: u32 = 6;
const DATA_MEMFD: u32 = 2;

fn words(v: &[u32]) -> Vec<u8> {
    v.iter().flat_map(|w| w.to_ne_bytes()).collect()
}

fn pod(ty: u32, body: &[u8]) -> Vec<u8> {
    let mut out = words(&[body.len() as u32, ty]);
    out.extend_from_slice(body);
    while !out.len().is_multiple_of(8) {
        out.push(0);
    }
    out
}

fn id(v: u32) -> Vec<u8> {
    pod(ID, &words(&[v]))
}

fn int(v: u32) -> Vec<u8> {
    pod(INT, &words(&[v]))
}

fn rectangle(w: u32, h: u32) -> Vec<u8> {
    pod(RECTANGLE, &words(&[w, h]))
}

fn fraction(n: u32, d: u32) -> Vec<u8> {
    pod(FRACTION, &words(&[n, d]))
}

/// A choice of `values`, each `child_size` bytes of type `child_type`.
fn choice(kind: u32, child_type: u32, child_size: u32, values: &[u32]) -> Vec<u8> {
    let mut body = words(&[kind, 0, child_size, child_type]);
    body.extend(words(values));
    pod(CHOICE, &body)
}

fn object(ty: u32, id: u32, props: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let mut body = words(&[ty, id]);
    for (key, value) in props {
        body.extend(words(&[*key, 0]));
        body.extend_from_slice(value);
    }
    pod(OBJECT, &body)
}

/// The one format offered: RGBx at a fixed size, variable frame rate.
pub fn video_format(width: u32, height: u32, fps: u32) -> Vec<u8> {
    object(
        OBJECT_FORMAT,
        PARAM_ENUM_FORMAT,
        &[
            (FORMAT_MEDIA_TYPE, id(MEDIA_TYPE_VIDEO)),
            (FORMAT_MEDIA_SUBTYPE, id(MEDIA_SUBTYPE_RAW)),
            (FORMAT_VIDEO_FORMAT, id(VIDEO_FORMAT_RGBX)),
            (FORMAT_VIDEO_SIZE, rectangle(width, height)),
            (FORMAT_VIDEO_FRAMERATE, fraction(0, 1)),
            (FORMAT_VIDEO_MAX_FRAMERATE, choice(CHOICE_RANGE, FRACTION, 8, &[fps, 1, 1, 1, fps, 1])),
        ],
    )
}

/// Buffers of one memfd block each.
pub fn buffers(size: u32, stride: u32) -> Vec<u8> {
    object(
        OBJECT_PARAM_BUFFERS,
        PARAM_BUFFERS,
        &[
            (BUFFERS_BUFFERS, choice(CHOICE_RANGE, INT, 4, &[4, 2, 8])),
            (BUFFERS_BLOCKS, int(1)),
            (BUFFERS_SIZE, int(size)),
            (BUFFERS_STRIDE, int(stride)),
            (BUFFERS_DATA_TYPE, choice(CHOICE_FLAGS, INT, 4, &[1 << DATA_MEMFD])),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn padding_and_sizes() {
        // An Id: 4 bytes of body, padded to 8.
        assert_eq!(id(2), words(&[4, ID, 2, 0]));
        let c = choice(CHOICE_RANGE, INT, 4, &[4, 2, 8]);
        assert_eq!(&c[..8], &words(&[16 + 12, CHOICE])[..]);
        assert_eq!(c.len(), 8 + 28 + 4);
        let f = video_format(1920, 1080, 30);
        let size = u32::from_ne_bytes(f[..4].try_into().unwrap()) as usize;
        assert_eq!(f.len(), (8 + size).div_ceil(8) * 8);
        assert_eq!(f.len() % 8, 0);
    }
}

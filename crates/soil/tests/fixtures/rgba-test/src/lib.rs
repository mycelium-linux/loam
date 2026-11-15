//! A Spore that presents encoded RGBA instead of a `wasi:webgpu` texture.
//!
//! This is the fixture for the `rgba` delivery: no GPU device, no texture, no
//! readback. It fills the granted extent with four vertical bars and a black
//! corner block, so one captured frame proves the extent, the channel order and
//! the orientation at once.

wit_bindgen::generate!({
    path: "wit",
    world: "runner",
    generate_all,
});

use soil::present::surface;
use soil::present::surface::RgbaFrame;

fn render() -> Result<(), String> {
    let (width, height) = surface::size();
    if width == 0 || height == 0 {
        return Err("host granted a zero extent".into());
    }
    let mut pixels = Vec::with_capacity((width * height * 4) as usize);
    let quarter = (width / 4).max(1);
    let corner = ((width / 8).max(1), (height / 8).max(1));
    for y in 0..height {
        for x in 0..width {
            let (r, g, b) = if x < corner.0 && y < corner.1 {
                (0, 0, 0)
            } else {
                match x / quarter {
                    0 => (255, 0, 0),
                    1 => (0, 255, 0),
                    2 => (0, 0, 255),
                    _ => (255, 255, 255),
                }
            };
            pixels.extend_from_slice(&[r, g, b, 255]);
        }
    }
    surface::present_rgba(&RgbaFrame {
        width,
        height,
        pixels,
    })
    .map_err(|error| format!("present-rgba: {error}"))
}

struct Component;

impl Guest for Component {
    async fn run() -> u32 {
        match render() {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("rgba-test: {error}");
                1
            }
        }
    }
}

export!(Component);

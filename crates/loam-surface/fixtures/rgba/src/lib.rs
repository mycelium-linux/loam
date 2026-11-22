// SPDX-License-Identifier: MIT OR Apache-2.0
//! A one-frame surface fixture: four vertical bars and a black corner.
//!
//! It imports only `soil:surface`, so it proves the native window and the
//! headless capture path without a GPU or any other capability.

wit_bindgen::generate!({
    path: "wit",
    world: "guest",
    generate_all,
});

use soil::surface::surface;
use soil::surface::surface::RgbaFrame;

struct Component;

impl Guest for Component {
    fn run() -> u32 {
        let (width, height) = surface::size();
        if width == 0 || height == 0 {
            return 1;
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
        match surface::present_rgba(&RgbaFrame {
            width,
            height,
            pixels,
        })
        {
            Ok(()) => 0,
            Err(_) => 1,
        }
    }
}

export!(Component);

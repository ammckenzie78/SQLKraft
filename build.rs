//! Renders packaging/sqlkraft.svg (the single source for the app icon) into
//! - `$OUT_DIR/icon-256.rgba`: raw RGBA pixels for the window icon, and
//! - on Windows, `$OUT_DIR/sqlkraft.ico`, embedded in the .exe with version info.

use std::{env, fs, path::PathBuf};

use resvg::{tiny_skia, usvg};

const SVG: &str = "packaging/sqlkraft.svg";
const ICO_SIZES: [u32; 7] = [16, 20, 24, 32, 48, 64, 256];

fn main() {
    println!("cargo:rerun-if-changed={SVG}");
    println!("cargo:rerun-if-changed=build.rs");
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));

    let data = fs::read(SVG).expect("read icon SVG");
    let tree = usvg::Tree::from_data(&data, &usvg::Options::default()).expect("parse icon SVG");
    let render = |size: u32| {
        let mut pixmap = tiny_skia::Pixmap::new(size, size).expect("pixmap");
        let scale = size as f32 / tree.size().width();
        resvg::render(&tree, tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
        pixmap
    };

    // tiny-skia stores premultiplied alpha; the window icon wants straight RGBA.
    let rgba: Vec<u8> = render(256)
        .pixels()
        .iter()
        .flat_map(|p| {
            let c = p.demultiply();
            [c.red(), c.green(), c.blue(), c.alpha()]
        })
        .collect();
    fs::write(out.join("icon-256.rgba"), rgba).expect("write window icon");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let pngs: Vec<(u32, Vec<u8>)> = ICO_SIZES
            .iter()
            .map(|&s| (s, render(s).encode_png().expect("encode PNG")))
            .collect();
        let ico = out.join("sqlkraft.ico");
        fs::write(&ico, build_ico(&pngs)).expect("write .ico");

        let mut res = winresource::WindowsResource::new();
        res.set_icon(ico.to_str().expect("UTF-8 OUT_DIR"));
        res.set("FileDescription", "SQLKraft");
        res.set("ProductName", "SQLKraft");
        res.compile().expect("compile Windows resources");
    }
}

/// An .ico whose images are stored as PNG (supported since Windows Vista).
fn build_ico(images: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let mut ico = Vec::new();
    ico.extend_from_slice(&0u16.to_le_bytes()); // reserved
    ico.extend_from_slice(&1u16.to_le_bytes()); // type: icon
    ico.extend_from_slice(&(images.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * images.len() as u32;
    for (size, png) in images {
        let dim = if *size >= 256 { 0 } else { *size as u8 }; // 0 means 256
        ico.extend_from_slice(&[dim, dim, 0, 0]); // width, height, palette, reserved
        ico.extend_from_slice(&1u16.to_le_bytes()); // colour planes
        ico.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
        ico.extend_from_slice(&(png.len() as u32).to_le_bytes());
        ico.extend_from_slice(&offset.to_le_bytes());
        offset += png.len() as u32;
    }
    for (_, png) in images {
        ico.extend_from_slice(png);
    }
    ico
}

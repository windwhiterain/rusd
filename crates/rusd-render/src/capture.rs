//! Deterministic capture helpers (SPEC §7): write beauty/id images to PNG.

/// Write RGBA8 pixels to a PNG file.
pub fn write_png(path: &str, width: u32, height: u32, rgba: &[u8]) -> Result<(), String> {
    let file = std::fs::File::create(path).map_err(|e| format!("{path}: {e}"))?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(rgba).map_err(|e| e.to_string())?;
    Ok(())
}

/// Visualize a prim-id buffer as an RGBA image: background black, each id a
/// deterministic well-separated hue (golden-ratio spacing). For eyeballing the
/// segmentation pass; picking reads the raw ids, not this image.
pub fn ids_to_rgba(ids: &[u32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ids.len() * 4);
    for &id in ids {
        let [r, g, b] = if id == 0 { [0u8, 0, 0] } else { id_color(id) };
        out.extend_from_slice(&[r, g, b, 255]);
    }
    out
}

fn id_color(id: u32) -> [u8; 3] {
    let h = (id as f32 * 0.618_034) % 1.0;
    hsv_to_rgb(h, 0.65, 0.95)
}

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> [u8; 3] {
    let i = (h * 6.0).floor();
    let f = h * 6.0 - i;
    let p = v * (1.0 - s);
    let q = v * (1.0 - f * s);
    let t = v * (1.0 - (1.0 - f) * s);
    let (r, g, b) = match (i as i32) % 6 {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    };
    [(r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8]
}

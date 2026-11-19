//! Outils partagés par les tests d'intégration : relief analytique et tuiles synthétiques.
#![allow(dead_code)]

use std::path::Path;

use itm_server::dem::TileKey;
use itm_server::raster_out::{geotiff_f32, GeoRef};

/// Même relief analytique que scripts/make_tiles.py
pub fn terrain(lat: f64, lon: f64) -> f64 {
    400.0
        + 250.0 * (lat * 12.0).sin() * (lon * 9.0).cos()
        + 1800.0 * (-(((lat - 45.2) / 0.08).powi(2) + ((lon - 5.75) / 0.1).powi(2))).exp()
}

/// Écrit une tuile au format Copernicus (PixelIsArea, origine décalée d'un demi-pixel).
pub fn write_tile(root: &Path, lat: i32, lon: i32, width: usize) {
    let key = TileKey { lat, lon };
    let name = key.name();
    let (dx, dy) = (1.0 / width as f64, 1.0 / 1200.0);
    let mut data = Vec::with_capacity(width * 1200);
    for r in 0..1200 {
        for c in 0..width {
            data.push(terrain(lat as f64 + 1.0 - r as f64 * dy, lon as f64 + c as f64 * dx) as f32);
        }
    }
    let geo = GeoRef {
        west: lon as f64 - dx / 2.0,
        north: lat as f64 + 1.0 + dy / 2.0,
        dlon: dx,
        dlat: dy,
        pixel_is_point: false,
    };
    let bytes = geotiff_f32(width as u32, 1200, &data, &geo, None).unwrap();
    let dir = root.join(&name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{name}.tif")), bytes).unwrap();
}

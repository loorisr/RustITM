//! Écriture des résultats raster : GeoTIFF float32 (EPSG:4326) et PNG coloré.

use std::io::Cursor;

use anyhow::Result;
use tiff::encoder::compression::DeflateLevel;
use tiff::encoder::{colortype, Compression, TiffEncoder};
use tiff::tags::Tag;

use crate::coverage::Grid;

pub const NODATA: f32 = -9999.0;

/// Géoréférencement d'un raster lat/lon.
pub struct GeoRef {
    /// Coordonnées du coin nord-ouest (PixelIsArea) ou du centre du pixel (0,0) (PixelIsPoint)
    pub west: f64,
    pub north: f64,
    pub dlon: f64,
    pub dlat: f64,
    pub pixel_is_point: bool,
}

/// Encode un raster float32 en GeoTIFF (compression Deflate) avec les GeoKeys EPSG:4326.
pub fn geotiff_f32(
    width: u32,
    height: u32,
    data: &[f32],
    geo: &GeoRef,
    nodata: Option<f32>,
) -> Result<Vec<u8>> {
    let mut buf = Cursor::new(Vec::new());
    {
        let mut enc = TiffEncoder::new(&mut buf)?
            .with_compression(Compression::Deflate(DeflateLevel::Balanced));
        let mut img = enc.new_image::<colortype::Gray32Float>(width, height)?;
        let e = img.encoder();
        e.write_tag(Tag::ModelPixelScaleTag, &[geo.dlon, geo.dlat, 0.0][..])?;
        e.write_tag(
            Tag::ModelTiepointTag,
            &[0.0, 0.0, 0.0, geo.west, geo.north, 0.0][..],
        )?;
        let raster_type: u16 = if geo.pixel_is_point { 2 } else { 1 };
        let keys: [u16; 16] = [
            1,
            1,
            0,
            3, // en-tête : version 1.1.0, 3 clés
            1024,
            0,
            1,
            2, // GTModelTypeGeoKey = Geographic
            1025,
            0,
            1,
            raster_type, // GTRasterTypeGeoKey
            2048,
            0,
            1,
            4326, // GeographicTypeGeoKey = WGS 84
        ];
        e.write_tag(Tag::GeoKeyDirectoryTag, &keys[..])?;
        if let Some(nd) = nodata {
            e.write_tag(Tag::GdalNodata, format!("{nd}").as_str())?;
        }
        img.write_data(data)?;
    }
    Ok(buf.into_inner())
}

pub fn grid_geotiff(g: &Grid) -> Result<Vec<u8>> {
    let data: Vec<f32> = g
        .data
        .iter()
        .map(|v| if v.is_finite() { *v } else { NODATA })
        .collect();
    let geo = GeoRef {
        west: g.west,
        north: g.north,
        dlon: g.dlon,
        dlat: g.dlat,
        pixel_is_point: false,
    };
    geotiff_f32(g.width as u32, g.height as u32, &data, &geo, Some(NODATA))
}

/// Palette (de faible à fort signal).
const RAMP: [(f32, [u8; 3]); 6] = [
    (0.0, [48, 18, 59]),
    (0.2, [70, 107, 227]),
    (0.4, [40, 187, 236]),
    (0.6, [100, 253, 106]),
    (0.8, [249, 189, 37]),
    (1.0, [205, 32, 8]),
];

fn ramp(t: f32) -> [u8; 3] {
    let t = t.clamp(0.0, 1.0);
    for w in RAMP.windows(2) {
        let (t0, c0) = w[0];
        let (t1, c1) = w[1];
        if t <= t1 {
            let f = (t - t0) / (t1 - t0);
            let mut out = [0u8; 3];
            for i in 0..3 {
                out[i] = (c0[i] as f32 + (c1[i] as f32 - c0[i] as f32) * f).round() as u8;
            }
            return out;
        }
    }
    RAMP[RAMP.len() - 1].1
}

/// PNG RGBA : `strong`/`weak` = valeurs de la grandeur pour le haut et le bas de la palette.
/// Les pixels plus faibles que `weak` (ou sans donnée) sont transparents.
pub fn grid_png(g: &Grid, weak: f32, strong: f32, alpha: u8) -> Result<Vec<u8>> {
    let mut rgba = Vec::with_capacity(g.width * g.height * 4);
    let span = strong - weak;
    for v in &g.data {
        let t = if v.is_finite() && span != 0.0 {
            (v - weak) / span
        } else {
            -1.0
        };
        if t < 0.0 {
            rgba.extend_from_slice(&[0, 0, 0, 0]);
        } else {
            let c = ramp(t);
            rgba.extend_from_slice(&[c[0], c[1], c[2], alpha]);
        }
    }
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, g.width as u32, g.height as u32);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut w = enc.write_header()?;
        w.write_image_data(&rgba)?;
    }
    Ok(out)
}

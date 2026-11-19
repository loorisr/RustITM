//! Carte de couverture calculée par ITM point à point le long de radiales.
//!
//! Pour chaque radiale, le profil complet (émetteur → rayon max) est extrait une seule fois,
//! puis ITM P2P est appelé pour chaque distance avec le sous-profil [0..k]. C'est la méthode
//! de SPLAT!/Signal-Server : chaque pixel reçoit un vrai calcul point à point avec le terrain
//! réel, et non le mode « zone » statistique d'ITM.
//! Les résultats polaires sont ensuite reportés sur une grille lat/lon (EPSG:4326) :
//! - `Interp::Nearest` (défaut) : chaque pixel prend le point (radiale, pas) le plus proche,
//!   et ITM n'est appelé qu'aux points utilisés par au moins un pixel (un appel par pixel
//!   au plus) ;
//! - `Interp::Bilinear` : ITM à tous les pas, puis interpolation bilinéaire en
//!   (azimut, distance).

use std::f64::consts::TAU;
use std::time::Instant;

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::dem::TileSet;
use crate::geo::{bearing, distance, radial_profile, LatLon, EARTH_R};

pub struct CoverageSpec {
    pub tx: LatLon,
    pub radius_m: f64,
    /// 1 = grille native GLO-90 ; 2, 4… = un pixel GLO-90 sur N (aperçu rapide)
    pub downsample: u32,
    pub params: itm::P2pParams,
    pub max_radials: usize,
    pub interp: Interp,
}

/// Report des résultats polaires sur la grille.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Interp {
    /// Point polaire le plus proche ; ITM seulement aux points utilisés (le plus rapide)
    #[default]
    Nearest,
    /// Interpolation bilinéaire en (azimut, distance) ; ITM à tous les pas
    Bilinear,
}

/// 3 secondes d'arc en mètres (pas en latitude de GLO-90).
pub const ARCSEC3_M: f64 = EARTH_R * std::f64::consts::PI / 180.0 / 1200.0;

/// Taille de pixel GLO-90 (degrés lat, degrés lon) : 3" en latitude ; en longitude 3" sous 50°
/// puis 4,5", 6", 9", 15", 30" vers les pôles (tuiles de 1200, 800, 600, 400, 240, 120 colonnes).
pub fn glo90_pixel_deg(lat: f64) -> (f64, f64) {
    let a = lat.abs();
    let cols = if a < 50.0 {
        1200.0
    } else if a < 60.0 {
        800.0
    } else if a < 70.0 {
        600.0
    } else if a < 80.0 {
        400.0
    } else if a < 85.0 {
        240.0
    } else {
        120.0
    };
    (1.0 / 1200.0, 1.0 / cols)
}

/// Géométrie de la grille de sortie, calée sur les centres de pixels GLO-90.
#[derive(Debug, Clone, Copy)]
pub struct GridGeom {
    pub width: usize,
    pub height: usize,
    pub west: f64,
    pub north: f64,
    pub dlon: f64,
    pub dlat: f64,
    /// Pas radial (m) = taille du pixel en latitude
    pub step_m: f64,
    pub n_radials: usize,
}

pub fn grid_geometry(tx: LatLon, radius_m: f64, downsample: u32, max_radials: usize) -> GridGeom {
    let ds = downsample.max(1) as f64;
    let (dlat0, dlon0) = glo90_pixel_deg(tx.lat);
    let (dlat, dlon) = (dlat0 * ds, dlon0 * ds);
    let m_per_deg = EARTH_R * std::f64::consts::PI / 180.0;
    let coslat = tx.lat.to_radians().cos().max(0.01);
    let step_m = dlat * m_per_deg;
    let dx_m = dlon * m_per_deg * coslat;
    // Centres de pixels GLO-90 : multiples entiers du pas, en lat comme en lon.
    let lat_c = (tx.lat / dlat).round() * dlat;
    let lon_c = (tx.lon / dlon).round() * dlon;
    let n_lat = (radius_m / step_m).ceil() as usize;
    let n_lon = (radius_m / dx_m).ceil() as usize;
    let n_radials =
        ((TAU * radius_m / step_m.min(dx_m)).ceil() as usize).clamp(8, max_radials.max(8));
    GridGeom {
        width: 2 * n_lon + 1,
        height: 2 * n_lat + 1,
        west: lon_c - (n_lon as f64 + 0.5) * dlon,
        north: lat_c + (n_lat as f64 + 0.5) * dlat,
        dlon,
        dlat,
        step_m,
        n_radials,
    }
}

/// Grille géoréférencée (EPSG:4326). `west`/`north` = bords extérieurs du pixel (0,0).
pub struct Grid {
    pub width: usize,
    pub height: usize,
    pub west: f64,
    pub north: f64,
    pub dlon: f64,
    pub dlat: f64,
    /// Valeurs ligne par ligne du nord au sud ; NaN = pas de donnée
    pub data: Vec<f32>,
}

impl Grid {
    pub fn bounds(&self) -> Bounds {
        Bounds {
            west: self.west,
            south: self.north - self.height as f64 * self.dlat,
            east: self.west + self.width as f64 * self.dlon,
            north: self.north,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Bounds {
    pub west: f64,
    pub south: f64,
    pub east: f64,
    pub north: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CoverageStats {
    pub radials: usize,
    pub steps_per_radial: usize,
    pub step_m: f64,
    pub pixel_arcsec: [f64; 2],
    pub interp: Interp,
    pub itm_calls: u64,
    pub itm_errors: u64,
    pub width: usize,
    pub height: usize,
    pub elapsed_ms: u128,
}

/// Position polaire fractionnaire (indice de radiale, indice de pas) d'un point, ou `None`
/// au-delà du rayon.
fn polar_pos(
    tx: LatLon,
    p: LatLon,
    radius_m: f64,
    step: f64,
    n_steps: usize,
    n_rad: usize,
) -> Option<(f64, f64)> {
    let d = distance(tx, p);
    if d > radius_m {
        return None;
    }
    let fk = (d / step).min(n_steps as f64);
    let fj = if d < 1e-6 {
        0.0
    } else {
        bearing(tx, p) / TAU * n_rad as f64
    };
    Some((fj, fk))
}

/// Calcule l'affaiblissement de base (dB) sur la grille.
pub fn compute(tiles: &TileSet, spec: &CoverageSpec) -> (Grid, CoverageStats) {
    let t0 = Instant::now();
    let g = grid_geometry(spec.tx, spec.radius_m, spec.downsample, spec.max_radials);
    // Au moins 2 pas : ITM n'est appelé qu'à partir du pas 2.
    let n_steps = ((spec.radius_m / g.step_m).ceil() as usize).max(2);
    let (data, itm_calls, itm_errors) = match spec.interp {
        Interp::Nearest => compute_nearest(tiles, spec, &g, n_steps),
        Interp::Bilinear => compute_bilinear(tiles, spec, &g, n_steps),
    };
    let stats = CoverageStats {
        radials: g.n_radials,
        steps_per_radial: n_steps,
        step_m: g.step_m,
        pixel_arcsec: [g.dlat * 3600.0, g.dlon * 3600.0],
        interp: spec.interp,
        itm_calls,
        itm_errors,
        width: g.width,
        height: g.height,
        elapsed_ms: t0.elapsed().as_millis(),
    };
    (
        Grid {
            width: g.width,
            height: g.height,
            west: g.west,
            north: g.north,
            dlon: g.dlon,
            dlat: g.dlat,
            data,
        },
        stats,
    )
}

/// Un appel ITM par pixel : chaque pixel prend la valeur du point (radiale, pas) le plus
/// proche, et ITM n'est calculé qu'aux points ainsi utilisés (comme Signal-Server, où chaque
/// pixel appartient à un seul rayon). Près de l'émetteur, où de nombreuses radiales passent
/// dans le même pixel, cela évite la plupart des appels.
fn compute_nearest(
    tiles: &TileSet,
    spec: &CoverageSpec,
    g: &GridGeom,
    n_steps: usize,
) -> (Vec<f32>, u64, u64) {
    let (w, h) = (g.width, g.height);
    let (step, n_rad) = (g.step_m, g.n_radials);
    let nk = n_steps + 1;
    const OUTSIDE: u32 = u32::MAX;
    assert!(n_rad * nk < OUTSIDE as usize);

    // 1) Point polaire (j·nk + k) associé à chaque pixel.
    let mut keys = vec![OUTSIDE; w * h];
    keys.par_chunks_mut(w).enumerate().for_each(|(r, row)| {
        let lat = g.north - (r as f64 + 0.5) * g.dlat;
        for (c, key) in row.iter_mut().enumerate() {
            let p = LatLon {
                lat,
                lon: g.west + (c as f64 + 0.5) * g.dlon,
            };
            if let Some((fj, fk)) = polar_pos(spec.tx, p, spec.radius_m, step, n_steps, n_rad) {
                let j = (fj.round() as usize) % n_rad;
                // Les deux premiers pas (< 2 pas de l'émetteur) reprennent la valeur du pas 2.
                let k = (fk.round() as usize).clamp(2, n_steps);
                *key = (j * nk + k) as u32;
            }
        }
    });

    // 2) Points polaires réellement utilisés.
    let mut needed = vec![false; n_rad * nk];
    for &key in &keys {
        if key != OUTSIDE {
            needed[key as usize] = true;
        }
    }

    // 3) ITM uniquement aux points utilisés ; profil extrait jusqu'au dernier d'entre eux.
    let radials: Vec<(Vec<f32>, u64, u64)> = (0..n_rad)
        .into_par_iter()
        .map(|j| {
            let need = &needed[j * nk..(j + 1) * nk];
            let Some(k_max) = need.iter().rposition(|&b| b) else {
                return (Vec::new(), 0, 0);
            };
            let brg = TAU * j as f64 / n_rad as f64;
            let elev = radial_profile(tiles, spec.tx, brg, step, k_max);
            let mut out = vec![f32::NAN; k_max + 1];
            let (mut calls, mut errors) = (0u64, 0u64);
            let mut hint = itm::DeltaHHint::default();
            for k in 2..=k_max {
                if need[k] {
                    calls += 1;
                    match itm::p2p_hinted(&spec.params, &elev[..=k], step, &mut hint) {
                        Ok(r) => out[k] = r.loss_db as f32,
                        Err(_) => errors += 1,
                    }
                }
            }
            (out, calls, errors)
        })
        .collect();
    let itm_calls = radials.iter().map(|r| r.1).sum();
    let itm_errors = radials.iter().map(|r| r.2).sum();

    // 4) Lecture directe de la valeur polaire de chaque pixel.
    let mut data = vec![f32::NAN; w * h];
    data.par_iter_mut()
        .zip(keys.par_iter())
        .for_each(|(cell, &key)| {
            if key != OUTSIDE {
                let (j, k) = (key as usize / nk, key as usize % nk);
                *cell = radials[j].0[k];
            }
        });
    (data, itm_calls, itm_errors)
}

/// Ancienne méthode : ITM à chaque pas de chaque radiale, puis interpolation bilinéaire en
/// (azimut, distance). Rendu plus lisse, mais environ 1,5 fois plus d'appels ITM.
fn compute_bilinear(
    tiles: &TileSet,
    spec: &CoverageSpec,
    g: &GridGeom,
    n_steps: usize,
) -> (Vec<f32>, u64, u64) {
    let (step, n_rad) = (g.step_m, g.n_radials);

    // 1) Calcul polaire : une radiale par tâche rayon.
    let radials: Vec<(Vec<f32>, u64)> = (0..n_rad)
        .into_par_iter()
        .map(|j| {
            let brg = TAU * j as f64 / n_rad as f64;
            let elev = radial_profile(tiles, spec.tx, brg, step, n_steps);
            let mut out = vec![f32::NAN; n_steps + 1];
            let mut errors = 0u64;
            let mut hint = itm::DeltaHHint::default();
            for k in 2..=n_steps {
                match itm::p2p_hinted(&spec.params, &elev[..=k], step, &mut hint) {
                    Ok(r) => out[k] = r.loss_db as f32,
                    Err(_) => errors += 1,
                }
            }
            // Les deux premiers pas (< 2 pas de l'émetteur) reprennent la valeur du pas 2.
            out[0] = out[2];
            out[1] = out[2];
            (out, errors)
        })
        .collect();
    let itm_errors: u64 = radials.iter().map(|r| r.1).sum();
    let polar: Vec<Vec<f32>> = radials.into_iter().map(|r| r.0).collect();

    // 2) Rééchantillonnage sur grille lat/lon.
    let w = g.width;
    let mut data = vec![f32::NAN; w * g.height];
    data.par_chunks_mut(w).enumerate().for_each(|(r, row)| {
        let lat = g.north - (r as f64 + 0.5) * g.dlat;
        for (c, cell) in row.iter_mut().enumerate() {
            let p = LatLon {
                lat,
                lon: g.west + (c as f64 + 0.5) * g.dlon,
            };
            let Some((fj, fk)) = polar_pos(spec.tx, p, spec.radius_m, step, n_steps, n_rad) else {
                continue;
            };
            let k0 = (fk.floor() as usize).min(n_steps);
            let k1 = (k0 + 1).min(n_steps);
            let wk = fk - k0 as f64;
            let j0 = (fj.floor() as usize) % n_rad;
            let j1 = (j0 + 1) % n_rad;
            let wj = fj - fj.floor();
            let samples = [
                (polar[j0][k0], (1.0 - wj) * (1.0 - wk)),
                (polar[j0][k1], (1.0 - wj) * wk),
                (polar[j1][k0], wj * (1.0 - wk)),
                (polar[j1][k1], wj * wk),
            ];
            let (mut acc, mut wsum) = (0.0f64, 0.0f64);
            for (v, w) in samples {
                if v.is_finite() && w > 0.0 {
                    acc += v as f64 * w;
                    wsum += w;
                }
            }
            if wsum > 0.0 {
                *cell = (acc / wsum) as f32;
            } else if let Some(v) = samples.iter().map(|s| s.0).find(|v| v.is_finite()) {
                *cell = v;
            }
        }
    });
    let itm_calls = (n_rad * (n_steps - 1)) as u64;
    (data, itm_calls, itm_errors)
}

/// Point utile aux tests : position d'un pixel.
#[allow(dead_code)]
pub fn pixel_center(g: &Grid, r: usize, c: usize) -> LatLon {
    LatLon {
        lat: g.north - (r as f64 + 0.5) * g.dlat,
        lon: g.west + (c as f64 + 0.5) * g.dlon,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_aligned_on_glo90_pixels() {
        let tx = LatLon {
            lat: 45.123456,
            lon: 5.654321,
        };
        let g = grid_geometry(tx, 10_000.0, 1, 7200);
        // Centre du pixel (0,0) sur la grille des centres Copernicus (multiples de 1/1200°)
        let lat0 = g.north - g.dlat / 2.0;
        let lon0 = g.west + g.dlon / 2.0;
        assert!(((lat0 * 1200.0) - (lat0 * 1200.0).round()).abs() < 1e-6);
        assert!(((lon0 * 1200.0) - (lon0 * 1200.0).round()).abs() < 1e-6);
        assert!((g.step_m - 92.66).abs() < 0.05, "{}", g.step_m);
        // 10 km / 92,7 m -> 108 pixels de part et d'autre en latitude
        assert_eq!(g.height, 2 * 108 + 1);
        // Au nord de 50° : 800 colonnes par degré
        let g2 = grid_geometry(
            LatLon {
                lat: 55.3,
                lon: 10.1,
            },
            5_000.0,
            1,
            7200,
        );
        assert!((g2.dlon - 1.0 / 800.0).abs() < 1e-12);
        // Aperçu x4
        let g4 = grid_geometry(tx, 10_000.0, 4, 7200);
        assert!((g4.dlat - 4.0 / 1200.0).abs() < 1e-12);
    }
}

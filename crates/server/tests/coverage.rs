//! Tests de la carte de couverture sur des tuiles synthétiques.

use std::f64::consts::TAU;

use itm_server::coverage::{compute, CoverageSpec, Interp, ARCSEC3_M};
use itm_server::dem::{Dem, DemConfig};
use itm_server::geo::{bearing, circle_bbox, distance, radial_profile, LatLon};

mod common;
use common::write_tile;

const TX: LatLon = LatLon {
    lat: 45.23,
    lon: 5.68,
};
const RADIUS_M: f64 = 8_000.0;

fn spec(interp: Interp) -> CoverageSpec {
    spec_fast(interp, 0)
}

fn spec_fast(interp: Interp, fast: u32) -> CoverageSpec {
    CoverageSpec {
        tx: TX,
        radius_m: RADIUS_M,
        downsample: 1,
        params: itm::P2pParams {
            h_tx_m: 20.0,
            h_rx_m: 2.0,
            f_mhz: 446.0,
            ..Default::default()
        },
        max_radials: 7200,
        interp,
        fast,
    }
}

fn with_tiles<F: FnOnce(&itm_server::dem::TileSet)>(f: F) {
    let tiles_dir = tempfile::tempdir().unwrap();
    write_tile(tiles_dir.path(), 45, 5, 1200);
    let cache = tempfile::tempdir().unwrap();
    let dem = Dem::new(DemConfig {
        local_dir: Some(tiles_dir.path().into()),
        cache_dir: cache.path().into(),
        download: false,
        ..Default::default()
    })
    .unwrap();
    let (s, w, n, e) = circle_bbox(TX, RADIUS_M);
    let ts = dem.tile_set(s, w, n, e, 4).unwrap();
    f(&ts);
}

#[test]
fn fast_mode_close_to_exact() {
    with_tiles(|ts| {
        for interp in [Interp::Nearest, Interp::Bilinear] {
            let (exact, _) = compute(ts, &spec(interp));
            for (n, min_1db, min_3db) in [(1, 0.9999, 1.0), (50, 0.95, 0.999)] {
                let (fast, stats) = compute(ts, &spec_fast(interp, n));
                assert_eq!(stats.fast, n);
                assert_eq!(stats.itm_errors, 0);
                let (mut tot, mut in1, mut in3) = (0usize, 0usize, 0usize);
                for (a, b) in exact.data.iter().zip(&fast.data) {
                    assert_eq!(a.is_finite(), b.is_finite());
                    if a.is_finite() {
                        let e = (a - b).abs();
                        tot += 1;
                        in1 += (e <= 1.0) as usize;
                        in3 += (e <= 3.0) as usize;
                    }
                }
                let (r1, r3) = (in1 as f64 / tot as f64, in3 as f64 / tot as f64);
                assert!(
                    r1 >= min_1db && r3 >= min_3db,
                    "{interp:?} fast={n}: {r1} {r3}"
                );
            }
        }
    });
}

#[test]
fn nearest_pixels_are_exact_p2p_and_cheaper_than_bilinear() {
    with_tiles(nearest_checks);
}

fn nearest_checks(ts: &itm_server::dem::TileSet) {
    let sp = spec(Interp::Nearest);
    let (grid, stats) = compute(ts, &sp);
    let (grid_b, stats_b) = compute(ts, &spec(Interp::Bilinear));

    // Même emprise, mêmes pixels renseignés.
    assert_eq!((grid.width, grid.height), (grid_b.width, grid_b.height));
    let valid = grid.data.iter().filter(|v| v.is_finite()).count();
    let valid_b = grid_b.data.iter().filter(|v| v.is_finite()).count();
    assert_eq!(valid, valid_b);
    assert!(valid > 20_000, "{valid}");
    // Au plus un appel ITM par pixel, et nettement moins que la méthode bilinéaire.
    assert_eq!(stats.itm_errors, 0);
    assert!(
        stats.itm_calls as usize <= valid,
        "{} > {valid}",
        stats.itm_calls
    );
    assert!(
        (stats.itm_calls as f64) < 0.8 * stats_b.itm_calls as f64,
        "{} vs {}",
        stats.itm_calls,
        stats_b.itm_calls
    );

    // Chaque pixel vaut exactement ITM P2P vers le point (radiale, pas) le plus proche.
    let (n_rad, n_steps, step) = (stats.radials, stats.steps_per_radial, ARCSEC3_M);
    let mut checked = 0;
    for r in (0..grid.height).step_by(7) {
        for c in (0..grid.width).step_by(5) {
            let v = grid.data[r * grid.width + c];
            if !v.is_finite() {
                continue;
            }
            let p = LatLon {
                lat: grid.north - (r as f64 + 0.5) * grid.dlat,
                lon: grid.west + (c as f64 + 0.5) * grid.dlon,
            };
            let d = distance(TX, p);
            let fj = if d < 1e-6 {
                0.0
            } else {
                bearing(TX, p) / TAU * n_rad as f64
            };
            let j = (fj.round() as usize) % n_rad;
            let k = ((d / step).min(n_steps as f64).round() as usize).clamp(2, n_steps);
            let elev = radial_profile(ts, TX, TAU * j as f64 / n_rad as f64, step, k);
            let want = itm::p2p(&sp.params, &elev, step).unwrap().loss_db as f32;
            assert_eq!(
                v.to_bits(),
                want.to_bits(),
                "pixel ({r},{c}) : {v} vs {want}"
            );
            checked += 1;
        }
    }
    assert!(checked > 500, "{checked}");
}

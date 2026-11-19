//! Micro-benchmark par étape d'un appel ITM sur de vrais profils GLO-90, découpés comme dans
//! une couverture (un appel par distance le long de chaque radiale).
//!
//! `cargo run --release -p itm-server --example real_split -- <dossier tuiles> [lat lon rayon_km]`
//! (défaut : 45.5 6.0 100). Les tuiles doivent couvrir le cercle ; pas de téléchargement.
use std::hint::black_box;
use std::time::Instant;

use itm_server::coverage::{grid_geometry, ARCSEC3_M};
use itm_server::dem::{Dem, DemConfig};
use itm_server::geo::{circle_bbox, radial_profile, LatLon};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let dir = args
        .get(1)
        .expect("usage: real_split <dossier tuiles> [lat lon rayon_km]");
    let num = |i: usize, d: f64| args.get(i).map(|s| s.parse().unwrap()).unwrap_or(d);
    let tx = LatLon {
        lat: num(2, 45.5),
        lon: num(3, 6.0),
    };
    let radius_m = num(4, 100.0) * 1000.0;

    let cache = std::env::temp_dir().join("itm-real-split-cache");
    let dem = Dem::new(DemConfig {
        local_dir: Some(dir.into()),
        cache_dir: cache,
        download: false,
        ..Default::default()
    })?;
    let (s, w, n, e) = circle_bbox(tx, radius_m);
    let tiles = dem.tile_set(s, w, n, e, 64)?;

    let g = grid_geometry(tx, radius_m, 1, 7200);
    let step = ARCSEC3_M;
    let n_steps = (radius_m / step).ceil() as usize;
    // Une radiale sur 20 de la vraie couverture : échantillon représentatif.
    let n_bench = g.n_radials / 20;

    let t = Instant::now();
    let profiles: Vec<Vec<f64>> = (0..n_bench)
        .map(|j| {
            let brg = std::f64::consts::TAU * j as f64 / n_bench as f64;
            radial_profile(&tiles, tx, brg, step, n_steps)
        })
        .collect();
    let t_prof = t.elapsed().as_secs_f64();
    let calls = (n_bench * (n_steps - 1)) as f64;
    println!(
        "{n_bench} radiales x {n_steps} pas, {calls} appels ({} radiales dans la vraie couverture)",
        g.n_radials
    );
    println!(
        "extraction profils {:.3} µs/point",
        t_prof * 1e6 / (n_bench * (n_steps + 1)) as f64
    );

    let p = itm::P2pParams {
        h_tx_m: 30.0,
        h_rx_m: 2.0,
        f_mhz: 145.0,
        ..Default::default()
    };
    let gamma_e = 157e-9 * (1.0 - 0.04665 * (301.0f64 / 179.3).exp());
    let h = [30.0, 2.0];

    let run = |label: &str, f: &mut dyn FnMut(&[f64], usize)| {
        let t = Instant::now();
        for z in &profiles {
            for k in 2..=n_steps {
                f(z, k);
            }
        }
        println!(
            "{label:<18}{:.3} µs/appel",
            t.elapsed().as_secs_f64() * 1e6 / calls
        );
    };
    let mut acc = 0.0;
    run("p2p complet", &mut |z, k| {
        acc += itm::p2p(&p, black_box(&z[..=k]), step).map_or(0.0, |r| r.loss_db)
    });
    println!("  (checksum {acc:.6})");
    let mut acc = 0.0;
    let mut hint = itm::DeltaHHint::default();
    run("p2p_hinted", &mut |z, k| {
        if k == 2 {
            hint = itm::DeltaHHint::default();
        }
        acc += itm::p2p_hinted(&p, black_box(&z[..=k]), step, &mut hint).map_or(0.0, |r| r.loss_db)
    });
    println!("  (checksum {acc:.6})");
    run("find_horizons", &mut |z, k| {
        black_box(itm::core::find_horizons(
            black_box(&z[..=k]),
            step,
            1.0 / gamma_e,
            h,
        ));
    });
    run("compute_delta_h", &mut |z, k| {
        let d = k as f64 * step;
        black_box(itm::core::compute_delta_h(
            black_box(&z[..=k]),
            step,
            (15.0 * h[0]).min(0.1 * d),
            d - (15.0 * h[1]).min(0.1 * d),
        ));
    });
    let mut hint = itm::DeltaHHint::default();
    run("delta_h (indice)", &mut |z, k| {
        if k == 2 {
            hint = itm::DeltaHHint::default();
        }
        let d = k as f64 * step;
        black_box(itm::core::compute_delta_h_hinted(
            black_box(&z[..=k]),
            step,
            (15.0 * h[0]).min(0.1 * d),
            d - (15.0 * h[1]).min(0.1 * d),
            Some(&mut hint),
        ));
    });
    run("quick_pfl (total)", &mut |z, k| {
        black_box(itm::core::quick_pfl(black_box(&z[..=k]), step, gamma_e, h));
    });
    Ok(())
}

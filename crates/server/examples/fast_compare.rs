//! Compare le mode ITM rapide (`fast = N`) au mode exact sur une vraie couverture GLO-90.
//!
//! `cargo run --release -p itm-server --example fast_compare -- <dossier tuiles> [lat lon rayon_km [N,N,...]]`
//! (défaut : 45.5 6.0 100, N = 1,10,50). 145 MHz, émetteur 30 m, récepteur 2 m, `interp`
//! nearest. Temps : meilleur de 3 ; `RAYON_NUM_THREADS=1` pour un seul thread.
use std::time::Instant;

use itm_server::coverage::{compute, CoverageSpec, Interp};
use itm_server::dem::{Dem, DemConfig};
use itm_server::geo::{circle_bbox, LatLon};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let dir = args
        .get(1)
        .expect("usage: fast_compare <dossier tuiles> [lat lon rayon_km [N,...]]");
    let num = |i: usize, d: f64| args.get(i).map(|s| s.parse().unwrap()).unwrap_or(d);
    let tx = LatLon {
        lat: num(2, 45.5),
        lon: num(3, 6.0),
    };
    let radius_m = num(4, 100.0) * 1000.0;
    let strides: Vec<u32> = args
        .get(5)
        .map(|s| s.split(',').map(|v| v.parse().unwrap()).collect())
        .unwrap_or(vec![1, 10, 50]);
    let interp = match std::env::var("INTERP").as_deref() {
        Ok("bilinear") => Interp::Bilinear,
        _ => Interp::Nearest,
    };

    let dem = Dem::new(DemConfig {
        local_dir: Some(dir.into()),
        cache_dir: std::env::temp_dir().join("itm-fast-compare-cache"),
        download: false,
        ..Default::default()
    })?;
    let (s, w, n, e) = circle_bbox(tx, radius_m);
    let tiles = dem.tile_set(s, w, n, e, 64)?;
    let spec = |fast| CoverageSpec {
        tx,
        radius_m,
        downsample: 1,
        params: itm::P2pParams {
            h_tx_m: 30.0,
            h_rx_m: 2.0,
            f_mhz: 145.0,
            ..Default::default()
        },
        max_radials: 7200,
        interp,
        fast,
    };
    let run = |fast| {
        let mut best = f64::INFINITY;
        let mut out = None;
        for _ in 0..3 {
            let t = Instant::now();
            let r = compute(&tiles, &spec(fast));
            best = best.min(t.elapsed().as_secs_f64());
            out = Some(r);
        }
        (out.unwrap(), best)
    };

    let ((exact, st), t_exact) = run(0);
    println!(
        "{}x{} px, {} radiales, {} appels ITM, interp {:?}",
        st.width, st.height, st.radials, st.itm_calls, interp
    );
    println!(
        "| Mode | Temps | Gain | Écart moyen | ≤1 dB | ≤3 dB | >6 dB | max | Contour −120 dBm |"
    );
    println!("|---|---:|---:|---:|---:|---:|---:|---:|---:|");
    println!("| Exact | {t_exact:.2} s | 1,00× | 0 | 100 % | 100 % | 0 % | 0 | 0 % |");
    // Seuil −120 dBm pour 52 dBm PIRE : affaiblissement 172 dB
    let thr = 172.0f32;
    for n in strides {
        let ((fast, _), t) = run(n);
        let (mut tot, mut sum, mut i1, mut i3, mut o6, mut mx, mut cont) =
            (0usize, 0.0f64, 0usize, 0usize, 0usize, 0.0f32, 0usize);
        for (a, b) in exact.data.iter().zip(&fast.data) {
            if a.is_finite() && b.is_finite() {
                let e = (a - b).abs();
                tot += 1;
                sum += e as f64;
                i1 += (e <= 1.0) as usize;
                i3 += (e <= 3.0) as usize;
                o6 += (e > 6.0) as usize;
                mx = mx.max(e);
                cont += ((*a <= thr) != (*b <= thr)) as usize;
            }
        }
        let pc = |v: usize| 100.0 * v as f64 / tot as f64;
        println!(
            "| `fast {n}` | {t:.2} s | {:.2}× | {:.3} dB | {:.3} % | {:.3} % | {:.4} % | {mx:.1} dB | {:.3} % |",
            t_exact / t,
            sum / tot as f64,
            pc(i1),
            pc(i3),
            pc(o6),
            pc(cont)
        );
    }
    Ok(())
}

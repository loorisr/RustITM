//! Tests du lecteur MNE.
//! - toujours : aller-retour avec notre écrivain GeoTIFF, interpolation, téléchargement HTTP (serveur local)
//! - si `ITM_TEST_TILES` pointe vers des COG générés par GDAL (scripts/make_tiles.py) : lecture de COG réels

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::thread;

use itm_server::dem::{Dem, DemConfig, TileKey};
use itm_server::raster_out::{geotiff_f32, GeoRef};

/// Même relief analytique que scripts/make_tiles.py
fn terrain(lat: f64, lon: f64) -> f64 {
    400.0
        + 250.0 * (lat * 12.0).sin() * (lon * 9.0).cos()
        + 1800.0 * (-(((lat - 45.2) / 0.08).powi(2) + ((lon - 5.75) / 0.1).powi(2))).exp()
}

/// Écrit une tuile au format Copernicus (PixelIsArea, origine décalée d'un demi-pixel).
fn write_tile(root: &Path, lat: i32, lon: i32, width: usize) {
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

fn cfg(local: Option<PathBuf>, cache: &Path, download: bool, base_url: &str) -> DemConfig {
    DemConfig {
        local_dir: local,
        cache_dir: cache.to_path_buf(),
        download,
        base_url: base_url.into(),
        mem_tiles: 8,
        http_timeout: std::time::Duration::from_secs(10),
    }
}

fn check_points(dem: &Dem, pts: &[(f64, f64)], tol: f64) {
    let ts = dem.tile_set(44.0, 5.0, 45.99, 6.99, 16).unwrap();
    for &(la, lo) in pts {
        let z = ts.elevation(la, lo);
        let t = terrain(la, lo);
        assert!((z - t).abs() < tol, "({la},{lo}) : {z} vs {t}");
    }
}

const PTS: [(f64, f64); 8] = [
    (45.2, 5.75), // sommet du massif
    (45.21, 5.7612),
    (45.0, 5.5),     // limite de tuile en latitude
    (45.00004, 6.0), // coin entre 4 tuiles
    (44.99999, 5.99999),
    (44.5, 6.5),
    (45.9996, 5.0002), // bord nord
    (45.15, 5.85),
];

#[test]
fn roundtrip_own_writer() {
    let tmp = tempfile::tempdir().unwrap();
    for (la, lo) in [(44, 5), (44, 6), (45, 5), (45, 6)] {
        write_tile(tmp.path(), la, lo, 1200);
    }
    let cache = tempfile::tempdir().unwrap();
    let dem = Dem::new(cfg(Some(tmp.path().into()), cache.path(), false, "")).unwrap();
    // Erreur d'interpolation bilinéaire attendue : quelques mètres sur le massif (pente forte)
    check_points(&dem, &PTS, 3.0);
    // Tuile absente sans téléchargement : mer (0 m)
    let ts = dem.tile_set(43.5, 5.5, 43.6, 5.6, 4).unwrap();
    assert_eq!(ts.elevation(43.55, 5.55), 0.0);
}

#[test]
fn gdal_cog_tiles() {
    let Ok(dir) = std::env::var("ITM_TEST_TILES") else {
        eprintln!("ITM_TEST_TILES non défini : test COG GDAL ignoré");
        return;
    };
    let cache = tempfile::tempdir().unwrap();
    let dem = Dem::new(cfg(Some(dir.into()), cache.path(), false, "")).unwrap();
    check_points(&dem, &PTS, 3.0);
    // Bande 50-60° : 800 colonnes
    let ts = dem.tile_set(50.1, 5.1, 50.9, 5.9, 4).unwrap();
    for (la, lo) in [(50.3, 5.3), (50.77, 5.123)] {
        assert!((ts.elevation(la, lo) - terrain(la, lo)).abs() < 3.0);
    }
}

/// Mini serveur HTTP statique (GET uniquement) pour simuler le bucket S3.
fn serve(root: PathBuf) -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    thread::spawn(move || {
        for s in l.incoming() {
            let mut s = s.unwrap();
            let mut line = String::new();
            let mut r = BufReader::new(s.try_clone().unwrap());
            r.read_line(&mut line).unwrap();
            loop {
                let mut h = String::new();
                if r.read_line(&mut h).unwrap() <= 2 {
                    break;
                }
            }
            let path = line
                .split_whitespace()
                .nth(1)
                .unwrap_or("/")
                .trim_start_matches('/');
            match std::fs::read(root.join(path)) {
                Ok(b) => {
                    write!(
                        s,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        b.len()
                    )
                    .unwrap();
                    s.write_all(&b).unwrap();
                }
                Err(_) => {
                    // Comme S3 : 403 pour une clé inexistante
                    write!(
                        s,
                        "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .unwrap();
                }
            }
        }
    });
    format!("http://{addr}")
}

#[test]
fn download_and_cache() {
    let bucket = tempfile::tempdir().unwrap();
    write_tile(bucket.path(), 45, 5, 1200);
    let url = serve(bucket.path().into());
    let cache = tempfile::tempdir().unwrap();
    // Contourne un éventuel proxy d'environnement pour 127.0.0.1
    std::env::set_var("NO_PROXY", "127.0.0.1,localhost");
    std::env::set_var("no_proxy", "127.0.0.1,localhost");
    let dem = Dem::new(cfg(None, cache.path(), true, &url)).unwrap();

    let t = dem
        .tile(TileKey { lat: 45, lon: 5 })
        .unwrap()
        .expect("tuile téléchargée");
    assert_eq!(t.width, 1200);
    let name = TileKey { lat: 45, lon: 5 }.name();
    assert!(
        cache
            .path()
            .join(&name)
            .join(format!("{name}.tif"))
            .is_file(),
        "tuile mise en cache disque"
    );

    // Tuile inexistante côté bucket -> mer + marqueur
    assert!(dem.tile(TileKey { lat: 40, lon: -30 }).unwrap().is_none());
    assert!(cache
        .path()
        .join(format!("{}.missing", TileKey { lat: 40, lon: -30 }.name()))
        .exists());

    // Nouvelle instance sans téléchargement : lit depuis le cache disque
    let dem2 = Dem::new(cfg(None, cache.path(), false, "http://invalid")).unwrap();
    assert!(dem2.tile(TileKey { lat: 45, lon: 5 }).unwrap().is_some());
}

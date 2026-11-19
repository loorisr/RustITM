//! Routes HTTP.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::http::{header, HeaderMap, HeaderValue};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::Semaphore;

use crate::coverage::{self, CoverageSpec};
use crate::dem::Dem;
use crate::error::AppError;
use crate::geo::{self, LatLon};
use crate::radio::{field_strength_dbuvm, rx_power_dbm, ItmInput, LinkBudget};
use crate::raster_out;

#[derive(Debug, Clone)]
pub struct Limits {
    pub max_p2p_km: f64,
    pub max_radius_km: f64,
    pub max_grid_side: usize,
    pub max_tiles: usize,
    pub max_radials: usize,
    pub profile_step_m: f64,
    pub max_elevation_points: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_p2p_km: 1000.0,
            max_radius_km: 150.0,
            max_grid_side: 6001,
            max_tiles: 64,
            max_radials: 7200,
            profile_step_m: coverage::ARCSEC3_M,
            max_elevation_points: 10_000,
        }
    }
}

#[derive(Clone)]
pub struct AppState {
    pub dem: Arc<Dem>,
    pub limits: Arc<Limits>,
    pub coverage_slots: Arc<Semaphore>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(ui_index))
        .route("/ui/{*path}", get(ui_asset))
        .route("/demo", get(|| async { Redirect::permanent("/") }))
        .route("/health", get(health))
        .route("/v1", get(index))
        .route("/v1/info", get(info))
        .route("/v1/elevation", get(elevation_one).post(elevation_many))
        .route("/v1/p2p", post(p2p))
        .route("/v1/coverage", post(coverage_handler))
        .with_state(state)
}

async fn index() -> Json<serde_json::Value> {
    Json(json!({
        "service": "itm-api",
        "version": env!("CARGO_PKG_VERSION"),
        "model": "ITM (Longley-Rice) v1.3, portage Rust, mode point à point",
        "dem": "Copernicus GLO-90",
        "endpoints": ["GET /health", "GET /v1/info", "GET /v1/elevation?lat=&lon=", "POST /v1/elevation", "POST /v1/p2p", "POST /v1/coverage", "GET / (interface web)"]
    }))
}

/// Version, limites et configuration MNE (utilisé par l'interface web).
async fn info(State(st): State<AppState>) -> Json<serde_json::Value> {
    let l = &st.limits;
    let c = st.dem.config();
    Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "limits": {
            "max_p2p_km": l.max_p2p_km,
            "max_radius_km": l.max_radius_km,
            "max_grid_side": l.max_grid_side,
            "max_tiles": l.max_tiles,
            "profile_step_m": l.profile_step_m,
            "max_downsample": 8,
            "max_radials": l.max_radials,
        },
        "grid": {
            "kind": "GLO-90 native (centres de pixels Copernicus)",
            "dlat_arcsec": 3.0,
            "dlon_arcsec_by_abs_lat": [[0, 3.0], [50, 4.5], [60, 6.0], [70, 9.0], [80, 15.0], [85, 30.0]],
            "arcsec3_m": coverage::ARCSEC3_M,
        },
        "dem": {
            "source": "Copernicus GLO-90",
            "download": c.download,
            "local_dir": c.local_dir.is_some(),
            "mem_tiles": c.mem_tiles,
        }
    }))
}

// ---------- Interface web (fichiers embarqués dans le binaire) ----------

struct Asset {
    path: &'static str,
    mime: &'static str,
    body: &'static [u8],
}

macro_rules! asset {
    ($path:literal, $mime:literal) => {
        Asset {
            path: $path,
            mime: $mime,
            body: include_bytes!(concat!("../web/", $path)),
        }
    };
}

static ASSETS: &[Asset] = &[
    asset!("index.html", "text/html; charset=utf-8"),
    asset!("app.js", "text/javascript; charset=utf-8"),
    asset!("style.css", "text/css; charset=utf-8"),
    asset!(
        "vendor/leaflet/leaflet.js",
        "text/javascript; charset=utf-8"
    ),
    asset!("vendor/leaflet/leaflet.css", "text/css; charset=utf-8"),
    asset!("vendor/leaflet/LICENSE", "text/plain; charset=utf-8"),
    asset!("vendor/leaflet/images/layers.png", "image/png"),
    asset!("vendor/leaflet/images/layers-2x.png", "image/png"),
    asset!("vendor/leaflet/images/marker-icon.png", "image/png"),
    asset!("vendor/leaflet/images/marker-icon-2x.png", "image/png"),
    asset!("vendor/leaflet/images/marker-shadow.png", "image/png"),
];

fn serve_asset(path: &str) -> Response {
    match ASSETS.iter().find(|a| a.path == path) {
        Some(a) => {
            let cache = if a.path == "index.html" {
                "no-cache"
            } else {
                "public, max-age=3600"
            };
            (
                [
                    (header::CONTENT_TYPE, a.mime),
                    (header::CACHE_CONTROL, cache),
                ],
                a.body,
            )
                .into_response()
        }
        None => (StatusCode::NOT_FOUND, "introuvable").into_response(),
    }
}

async fn ui_index() -> Response {
    serve_asset("index.html")
}

async fn ui_asset(Path(path): Path<String>) -> Response {
    serve_asset(&path)
}

async fn health() -> &'static str {
    "ok"
}

/// Exécute un travail bloquant (MNE + calcul) hors du runtime async.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, AppError> + Send + 'static,
) -> Result<T, AppError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| AppError::internal(format!("tâche interrompue : {e}")))?
}

fn check_point(p: LatLon, what: &str) -> Result<(), AppError> {
    if !(-90.0..=90.0).contains(&p.lat)
        || !(-180.0..=180.0).contains(&p.lon)
        || !p.lat.is_finite()
        || !p.lon.is_finite()
    {
        return Err(AppError::bad_request(format!(
            "{what} : coordonnées invalides"
        )));
    }
    if p.lat.abs() > 85.0 {
        return Err(AppError::bad_request(format!(
            "{what} : latitude au-delà de ±85° non supportée"
        )));
    }
    Ok(())
}

// ---------- Altitude ----------

#[derive(Deserialize)]
struct PointQuery {
    lat: f64,
    lon: f64,
}

async fn elevation_one(
    State(st): State<AppState>,
    Query(q): Query<PointQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let p = LatLon {
        lat: q.lat,
        lon: q.lon,
    };
    check_point(p, "point")?;
    let dem = st.dem.clone();
    let z = blocking(move || {
        let ts = dem.tile_set(p.lat - 0.01, p.lon - 0.01, p.lat + 0.01, p.lon + 0.01, 4)?;
        Ok(ts.elevation(p.lat, p.lon))
    })
    .await?;
    Ok(Json(
        json!({ "lat": p.lat, "lon": p.lon, "elevation_m": z }),
    ))
}

#[derive(Deserialize)]
struct ElevationBody {
    points: Vec<Point>,
}

#[derive(Deserialize, Serialize, Clone, Copy)]
struct Point {
    lat: f64,
    lon: f64,
}

async fn elevation_many(
    State(st): State<AppState>,
    Json(b): Json<ElevationBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    if b.points.is_empty() || b.points.len() > st.limits.max_elevation_points {
        return Err(AppError::bad_request(format!(
            "1 à {} points",
            st.limits.max_elevation_points
        )));
    }
    for p in &b.points {
        check_point(
            LatLon {
                lat: p.lat,
                lon: p.lon,
            },
            "point",
        )?;
    }
    let dem = st.dem.clone();
    let max_tiles = st.limits.max_tiles;
    let out = blocking(move || {
        let s = b.points.iter().map(|p| p.lat).fold(f64::MAX, f64::min) - 0.01;
        let n = b.points.iter().map(|p| p.lat).fold(f64::MIN, f64::max) + 0.01;
        let w = b.points.iter().map(|p| p.lon).fold(f64::MAX, f64::min) - 0.01;
        let e = b.points.iter().map(|p| p.lon).fold(f64::MIN, f64::max) + 0.01;
        let ts = dem.tile_set(s, w, n, e, max_tiles)?;
        Ok(b.points
            .iter()
            .map(|p| json!({"lat": p.lat, "lon": p.lon, "elevation_m": ts.elevation(p.lat, p.lon)}))
            .collect::<Vec<_>>())
    })
    .await?;
    Ok(Json(json!({ "results": out })))
}

// ---------- Point à point ----------

#[derive(Deserialize, Clone, Copy)]
struct Terminal {
    lat: f64,
    lon: f64,
    /// Hauteur d'antenne au-dessus du sol (m)
    height_m: f64,
}

#[derive(Deserialize)]
struct P2pRequest {
    tx: Terminal,
    rx: Terminal,
    #[serde(flatten)]
    itm: ItmInput,
    #[serde(flatten)]
    budget: LinkBudget,
    #[serde(default)]
    include_profile: bool,
    profile_step_m: Option<f64>,
}

#[derive(Serialize)]
struct P2pResponse {
    path_loss_db: f64,
    free_space_loss_db: f64,
    excess_loss_db: f64,
    /// Mode de la courbe de référence ITM (los / diffraction / troposcatter)
    mode: &'static str,
    /// Géométrie réelle du trajet sur le terrain
    path_geometry: &'static str,
    distance_km: f64,
    bearing_deg: f64,
    tx_ground_m: f64,
    rx_ground_m: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    rx_power_dbm: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    field_strength_dbuvm: Option<f64>,
    warnings: WarningsOut,
    intermediate: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    profile: Option<ProfileOut>,
}

#[derive(Serialize)]
struct WarningsOut {
    flags: u32,
    messages: Vec<&'static str>,
}

#[derive(Serialize)]
struct ProfileOut {
    step_m: f64,
    elevations_m: Vec<f64>,
}

async fn p2p(
    State(st): State<AppState>,
    Json(req): Json<P2pRequest>,
) -> Result<Json<P2pResponse>, AppError> {
    let a = LatLon {
        lat: req.tx.lat,
        lon: req.tx.lon,
    };
    let b = LatLon {
        lat: req.rx.lat,
        lon: req.rx.lon,
    };
    check_point(a, "tx")?;
    check_point(b, "rx")?;
    let params = req.itm.to_params(req.tx.height_m, req.rx.height_m)?;
    let d = geo::distance(a, b);
    if d < 100.0 {
        return Err(AppError::bad_request(
            "distance tx-rx trop courte (< 100 m)",
        ));
    }
    if d > st.limits.max_p2p_km * 1000.0 {
        return Err(AppError::bad_request(format!(
            "distance > {} km",
            st.limits.max_p2p_km
        )));
    }
    let step = req
        .profile_step_m
        .unwrap_or(st.limits.profile_step_m)
        .clamp(10.0, 1000.0);
    let dem = st.dem.clone();
    let max_tiles = st.limits.max_tiles;

    let resp = blocking(move || {
        let (s, w, n, e) = geo::path_bbox(a, b);
        let ts = dem.tile_set(s, w, n, e, max_tiles)?;
        let (elev, real_step, dist) = geo::path_profile(&ts, a, b, step);
        let r = itm::p2p(&params, &elev, real_step)?;
        let iv = r.intermediate;
        let rx_power = req
            .budget
            .eirp_dbm
            .map(|p| rx_power_dbm(p, req.budget.rx_gain_dbi, r.loss_db));
        let field = req
            .budget
            .eirp_dbm
            .map(|p| field_strength_dbuvm(p, r.loss_db, params.f_mhz));
        Ok(P2pResponse {
            path_loss_db: r.loss_db,
            free_space_loss_db: iv.a_fs_db,
            excess_loss_db: r.loss_db - iv.a_fs_db,
            mode: iv.mode.as_str(),
            path_geometry: iv.path_geometry.as_str(),
            distance_km: dist / 1000.0,
            bearing_deg: geo::bearing(a, b).to_degrees(),
            tx_ground_m: elev[0],
            rx_ground_m: *elev.last().unwrap(),
            rx_power_dbm: rx_power,
            field_strength_dbuvm: field,
            warnings: WarningsOut {
                flags: r.warnings.0,
                messages: r.warnings.descriptions(),
            },
            intermediate: json!({
                "reference_attenuation_db": iv.a_ref_db,
                "delta_h_m": iv.delta_h_m,
                "horizon_distance_m": iv.d_hzn_m,
                "terrain_horizon_distance_m": iv.terrain_d_hzn_m,
                "effective_height_m": iv.h_e_m,
                "horizon_angle_rad": iv.theta_hzn,
                "surface_refractivity": iv.n_s,
            }),
            profile: req.include_profile.then_some(ProfileOut {
                step_m: real_step,
                elevations_m: elev,
            }),
        })
    })
    .await?;
    Ok(Json(resp))
}

// ---------- Couverture ----------

#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "snake_case")]
enum Quantity {
    #[default]
    PathLoss,
    RxPower,
    FieldStrength,
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
enum Format {
    #[default]
    Png,
    Geotiff,
    Json,
    /// float32 little-endian brut, ligne par ligne nord → sud (NaN = pas de donnée)
    F32,
}

fn d_rx_h() -> f64 {
    2.0
}
fn d_ds() -> u32 {
    1
}
fn d_alpha() -> u8 {
    200
}

#[derive(Deserialize)]
struct CoverageRequest {
    tx: Terminal,
    #[serde(default = "d_rx_h")]
    rx_height_m: f64,
    radius_km: f64,
    /// 1 = grille native GLO-90 (défaut) ; N = un pixel GLO-90 sur N (aperçu)
    #[serde(default = "d_ds")]
    downsample: u32,
    /// `nearest` (défaut, un appel ITM par pixel) ou `bilinear` (ancien rendu lissé)
    #[serde(default)]
    interp: coverage::Interp,
    #[serde(flatten)]
    itm: ItmInput,
    #[serde(flatten)]
    budget: LinkBudget,
    #[serde(default)]
    output: Quantity,
    #[serde(default)]
    format: Format,
    /// Valeur affichée en bas de palette (signal faible) ; en dessous : transparent
    color_weak: Option<f32>,
    /// Valeur affichée en haut de palette (signal fort)
    color_strong: Option<f32>,
    #[serde(default = "d_alpha")]
    png_alpha: u8,
}

async fn coverage_handler(
    State(st): State<AppState>,
    Json(req): Json<CoverageRequest>,
) -> Result<Response, AppError> {
    let tx = LatLon {
        lat: req.tx.lat,
        lon: req.tx.lon,
    };
    check_point(tx, "tx")?;
    let lim = st.limits.clone();
    if !(req.radius_km > 0.0 && req.radius_km <= lim.max_radius_km) {
        return Err(AppError::bad_request(format!(
            "radius_km doit être dans ]0, {}]",
            lim.max_radius_km
        )));
    }
    if !(1..=8).contains(&req.downsample) {
        return Err(AppError::bad_request(
            "downsample doit être entre 1 (natif GLO-90) et 8",
        ));
    }
    let g = coverage::grid_geometry(tx, req.radius_km * 1000.0, req.downsample, lim.max_radials);
    if g.width.max(g.height) > lim.max_grid_side {
        return Err(AppError::bad_request(format!(
            "grille trop grande ({}x{}, max {2}x{2}) : réduire radius_km ou utiliser downsample",
            g.width, g.height, lim.max_grid_side
        )));
    }
    if req.output != Quantity::PathLoss && req.budget.eirp_dbm.is_none() {
        return Err(AppError::bad_request(
            "eirp_dbm est requis pour rx_power et field_strength",
        ));
    }
    let params = req.itm.to_params(req.tx.height_m, req.rx_height_m)?;

    let _permit = st.coverage_slots.clone().try_acquire_owned().map_err(|_| {
        AppError::unavailable("trop de calculs de couverture en cours, réessayer plus tard")
    })?;

    let dem = st.dem.clone();
    let radius_m = req.radius_km * 1000.0;
    let (mut grid, stats) = blocking(move || {
        let (s, w, n, e) = geo::circle_bbox(tx, radius_m);
        let ts = dem.tile_set(s, w, n, e, lim.max_tiles)?;
        let spec = CoverageSpec {
            tx,
            radius_m,
            downsample: req.downsample,
            params,
            max_radials: lim.max_radials,
            interp: req.interp,
        };
        Ok(coverage::compute(&ts, &spec))
    })
    .await?;
    tracing::info!(?stats.radials, stats.elapsed_ms, "couverture calculée");

    // Conversion de grandeur.
    let f = params.f_mhz;
    let (unit, weak, strong) = match req.output {
        Quantity::PathLoss => ("dB", 180.0, 80.0),
        Quantity::RxPower => {
            let (p, g) = (req.budget.eirp_dbm.unwrap(), req.budget.rx_gain_dbi);
            grid.data
                .iter_mut()
                .for_each(|v| *v = rx_power_dbm(p, g, *v as f64) as f32);
            ("dBm", -120.0, -40.0)
        }
        Quantity::FieldStrength => {
            let p = req.budget.eirp_dbm.unwrap();
            grid.data
                .iter_mut()
                .for_each(|v| *v = field_strength_dbuvm(p, *v as f64, f) as f32);
            ("dBuV/m", 20.0, 100.0)
        }
    };
    let weak = req.color_weak.unwrap_or(weak);
    let strong = req.color_strong.unwrap_or(strong);
    let bounds = grid.bounds();

    let mut headers = HeaderMap::new();
    let b = format!(
        "{},{},{},{}",
        bounds.west, bounds.south, bounds.east, bounds.north
    );
    headers.insert("x-bounds", HeaderValue::from_str(&b).unwrap());
    headers.insert("x-unit", HeaderValue::from_static(unit));
    if let Ok(v) = HeaderValue::from_str(&serde_json::to_string(&stats).unwrap_or_default()) {
        headers.insert("x-coverage-stats", v);
    }
    headers.insert(
        header::ACCESS_CONTROL_EXPOSE_HEADERS,
        HeaderValue::from_static("x-bounds, x-unit, x-coverage-stats"),
    );

    match req.format {
        Format::Png => {
            let png =
                blocking(move || Ok(raster_out::grid_png(&grid, weak, strong, req.png_alpha)?))
                    .await?;
            headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/png"));
            Ok((headers, png).into_response())
        }
        Format::Geotiff => {
            let tif = blocking(move || Ok(raster_out::grid_geotiff(&grid)?)).await?;
            headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/tiff"));
            headers.insert(
                header::CONTENT_DISPOSITION,
                HeaderValue::from_static("attachment; filename=\"coverage.tif\""),
            );
            Ok((headers, tif).into_response())
        }
        Format::F32 => {
            let mut bytes = Vec::with_capacity(grid.data.len() * 4);
            for v in &grid.data {
                bytes.extend_from_slice(&v.to_le_bytes());
            }
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/octet-stream"),
            );
            headers.insert("x-width", HeaderValue::from(grid.width));
            headers.insert("x-height", HeaderValue::from(grid.height));
            headers.insert(
                header::ACCESS_CONTROL_EXPOSE_HEADERS,
                HeaderValue::from_static("x-bounds, x-unit, x-coverage-stats, x-width, x-height"),
            );
            Ok((headers, bytes).into_response())
        }
        Format::Json => {
            let values: Vec<Option<f32>> = grid
                .data
                .iter()
                .map(|v| v.is_finite().then(|| (v * 100.0).round() / 100.0))
                .collect();
            let body = json!({
                "quantity": req.output,
                "unit": unit,
                "bounds": bounds,
                "width": grid.width,
                "height": grid.height,
                "crs": "EPSG:4326",
                "order": "row-major, nord -> sud, ouest -> est",
                "stats": stats,
                "values": values,
            });
            Ok((headers, Json(body)).into_response())
        }
    }
}

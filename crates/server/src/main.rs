use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use tokio::sync::Semaphore;
use tower_http::compression::CompressionLayer;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

use itm_server::api::{router, AppState, Limits};
use itm_server::dem::{keys_for_bbox, Dem, DemConfig, DiskStatus};

/// API de propagation radio ITM avec le MNE Copernicus GLO-90.
/// Sans sous-commande, lance le serveur. Chaque option peut aussi être fournie par variable d'environnement.
#[derive(Parser, Debug)]
#[command(version, about, args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,
    #[command(flatten)]
    serve: ServeArgs,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Lance le serveur HTTP (comportement par défaut)
    Serve(ServeArgs),
    /// Pré-télécharge les tuiles MNE d'une zone (pour un usage hors ligne)
    Prefetch(PrefetchArgs),
    /// Vérifie que le serveur local répond (pour le HEALTHCHECK Docker)
    Healthcheck(HealthArgs),
}

#[derive(Args, Debug, Clone)]
struct DemArgs {
    /// Dossier local de tuiles GLO-90 (lecture seule), optionnel
    #[arg(long, env = "DEM_LOCAL_DIR")]
    dem_local_dir: Option<PathBuf>,

    /// Dossier de cache des tuiles téléchargées
    #[arg(long, env = "DEM_CACHE_DIR", default_value = "./dem-cache")]
    dem_cache_dir: PathBuf,

    /// Télécharger automatiquement les tuiles manquantes depuis AWS
    #[arg(long, env = "DEM_DOWNLOAD", default_value_t = true, action = clap::ArgAction::Set)]
    dem_download: bool,

    /// URL de base du bucket Copernicus GLO-90
    #[arg(
        long,
        env = "DEM_BASE_URL",
        default_value = "https://copernicus-dem-90m.s3.amazonaws.com"
    )]
    dem_base_url: String,

    /// Nombre de tuiles gardées en mémoire (~5,8 Mo chacune)
    #[arg(long, env = "DEM_MEM_TILES", default_value_t = 64)]
    dem_mem_tiles: usize,
}

impl DemArgs {
    fn build(&self) -> Result<Dem> {
        Dem::new(DemConfig {
            local_dir: self.dem_local_dir.clone(),
            cache_dir: self.dem_cache_dir.clone(),
            download: self.dem_download,
            base_url: self.dem_base_url.clone(),
            mem_tiles: self.dem_mem_tiles,
            http_timeout: Duration::from_secs(120),
        })
    }
}

#[derive(Args, Debug, Clone)]
struct ServeArgs {
    /// Adresse d'écoute
    #[arg(long, env = "ITM_BIND", default_value = "0.0.0.0:8080")]
    bind: String,

    #[command(flatten)]
    dem: DemArgs,

    /// Distance max d'un calcul point à point (km)
    #[arg(long, env = "MAX_P2P_KM", default_value_t = 1000.0)]
    max_p2p_km: f64,

    /// Rayon max d'une couverture (km)
    #[arg(long, env = "MAX_RADIUS_KM", default_value_t = 150.0)]
    max_radius_km: f64,

    /// Côté max de la grille de couverture (pixels)
    #[arg(long, env = "MAX_GRID_SIDE", default_value_t = 6001)]
    max_grid_side: usize,

    /// Nombre max de tuiles MNE par requête
    #[arg(long, env = "MAX_TILES", default_value_t = 64)]
    max_tiles: usize,

    /// Pas d'échantillonnage du profil en point à point (m) ; défaut : 3" GLO-90 (≈ 92,7 m)
    #[arg(long, env = "PROFILE_STEP_M", default_value_t = itm_server::coverage::ARCSEC3_M)]
    profile_step_m: f64,

    /// Calculs de couverture simultanés
    #[arg(long, env = "COVERAGE_CONCURRENCY", default_value_t = 2)]
    coverage_concurrency: usize,
}

#[derive(Args, Debug)]
struct PrefetchArgs {
    #[command(flatten)]
    dem: DemArgs,

    /// Emprise sud,ouest,nord,est en degrés (ex. 44,5,46.5,7.5)
    #[arg(long, value_delimiter = ',', allow_hyphen_values = true,
          conflicts_with_all = ["center", "radius_km"], required_unless_present = "center")]
    bbox: Option<Vec<f64>>,

    /// Centre lat,lon (avec --radius-km)
    #[arg(
        long,
        value_delimiter = ',',
        allow_hyphen_values = true,
        requires = "radius_km"
    )]
    center: Option<Vec<f64>>,

    /// Rayon autour du centre (km)
    #[arg(long)]
    radius_km: Option<f64>,

    /// Téléchargements simultanés
    #[arg(long, default_value_t = 4)]
    parallel: usize,

    /// Nombre max de tuiles (garde-fou)
    #[arg(long, default_value_t = 2000)]
    max_tiles: usize,
}

#[derive(Args, Debug)]
struct HealthArgs {
    /// Adresse du serveur (par défaut le port de ITM_BIND sur 127.0.0.1)
    #[arg(long, env = "ITM_BIND", default_value = "0.0.0.0:8080")]
    bind: String,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let res = match cli.command {
        Some(Cmd::Healthcheck(h)) => return healthcheck(&h.bind),
        Some(Cmd::Prefetch(p)) => {
            init_logs();
            prefetch(p)
        }
        Some(Cmd::Serve(s)) => {
            init_logs();
            serve(s)
        }
        None => {
            init_logs();
            serve(cli.serve)
        }
    };
    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("erreur : {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn init_logs() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,tower_http=info".into()),
        )
        .init();
}

#[tokio::main]
async fn serve(args: ServeArgs) -> Result<()> {
    let dem = args.dem.build()?;
    let limits = Limits {
        max_p2p_km: args.max_p2p_km,
        max_radius_km: args.max_radius_km,
        max_grid_side: args.max_grid_side,
        max_tiles: args.max_tiles,
        profile_step_m: args.profile_step_m,
        ..Default::default()
    };
    let state = AppState {
        dem: Arc::new(dem),
        limits: Arc::new(limits),
        coverage_slots: Arc::new(Semaphore::new(args.coverage_concurrency.max(1))),
    };
    let app = router(state)
        .layer(CompressionLayer::new())
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http());

    let listener = tokio::net::TcpListener::bind(&args.bind)
        .await
        .with_context(|| format!("écoute sur {}", args.bind))?;
    tracing::info!(addr = %args.bind, version = env!("CARGO_PKG_VERSION"), local = ?args.dem.dem_local_dir,
        cache = %args.dem.dem_cache_dir.display(), download = args.dem.dem_download, "itm-api démarré");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}

async fn shutdown() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
    tracing::info!("arrêt");
}

fn prefetch(args: PrefetchArgs) -> Result<()> {
    let (s, w, n, e) = if let Some(b) = &args.bbox {
        if b.len() != 4 {
            bail!("--bbox attend 4 valeurs : sud,ouest,nord,est");
        }
        (b[0], b[1], b[2], b[3])
    } else {
        let c = args.center.as_ref().context("--center requis")?;
        if c.len() != 2 {
            bail!("--center attend 2 valeurs : lat,lon");
        }
        let r = args.radius_km.context("--radius-km requis")? * 1000.0;
        itm_server::geo::circle_bbox(
            itm_server::geo::LatLon {
                lat: c[0],
                lon: c[1],
            },
            r,
        )
    };
    if !(s < n && w < e && (-90.0..=90.0).contains(&s) && (-90.0..=90.0).contains(&n)) {
        bail!("emprise invalide : sud < nord et ouest < est attendus");
    }
    let keys = keys_for_bbox(s, w, n, e);
    if keys.len() > args.max_tiles {
        bail!(
            "{} tuiles demandées (max {} ; voir --max-tiles)",
            keys.len(),
            args.max_tiles
        );
    }
    let dem = args.dem.build()?;
    println!(
        "{} tuiles à vérifier dans {:.3},{:.3},{:.3},{:.3}",
        keys.len(),
        s,
        w,
        n,
        e
    );

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(args.parallel.max(1))
        .build()?;
    let done = std::sync::atomic::AtomicUsize::new(0);
    let total = keys.len();
    let results: Vec<(String, Result<DiskStatus>)> = pool.install(|| {
        use rayon::prelude::*;
        keys.par_iter()
            .with_max_len(1)
            .map(|k| {
                let r = dem.ensure_on_disk(*k);
                let i = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                let label = match &r {
                    Ok(DiskStatus::Present) => "déjà présente".to_string(),
                    Ok(DiskStatus::Downloaded) => "téléchargée".to_string(),
                    Ok(DiskStatus::Sea) => "mer (pas de tuile)".to_string(),
                    Ok(DiskStatus::Absent) => "absente (téléchargement désactivé)".to_string(),
                    Err(e) => format!("ERREUR {e:#}"),
                };
                println!("[{i}/{total}] {} : {label}", k.name());
                (k.name(), r)
            })
            .collect()
    });
    let count = |f: fn(&DiskStatus) -> bool| {
        results
            .iter()
            .filter(|(_, r)| r.as_ref().is_ok_and(f))
            .count()
    };
    let errors = results.iter().filter(|(_, r)| r.is_err()).count();
    println!(
        "Terminé : {} téléchargées, {} déjà présentes, {} mer, {} absentes, {} erreurs",
        count(|d| *d == DiskStatus::Downloaded),
        count(|d| *d == DiskStatus::Present),
        count(|d| *d == DiskStatus::Sea),
        count(|d| *d == DiskStatus::Absent),
        errors
    );
    if errors > 0 {
        bail!("{errors} tuile(s) en erreur");
    }
    Ok(())
}

/// GET /health minimal en TCP (l'image Docker n'a ni shell ni curl).
fn healthcheck(bind: &str) -> ExitCode {
    let port = bind.rsplit(':').next().unwrap_or("8080");
    let host = bind.rsplit_once(':').map(|(h, _)| h).unwrap_or("127.0.0.1");
    let host = if host.is_empty() || host == "0.0.0.0" || host == "[::]" {
        "127.0.0.1"
    } else {
        host
    };
    let check = || -> Result<()> {
        let addr: SocketAddr = format!("{host}:{port}")
            .to_socket_addrs()?
            .next()
            .context("adresse")?;
        let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(3))?;
        s.set_read_timeout(Some(Duration::from_secs(3)))?;
        s.write_all(b"GET /health HTTP/1.0\r\nHost: localhost\r\n\r\n")?;
        let mut buf = String::new();
        s.read_to_string(&mut buf)?;
        if buf.starts_with("HTTP/1.1 200") || buf.starts_with("HTTP/1.0 200") {
            Ok(())
        } else {
            bail!("réponse inattendue : {}", buf.lines().next().unwrap_or(""))
        }
    };
    match check() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("healthcheck : {e:#}");
            ExitCode::FAILURE
        }
    }
}

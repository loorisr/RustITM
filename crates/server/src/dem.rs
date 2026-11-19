//! Accès au MNE Copernicus GLO-90 (tuiles COG de 1°x1°).
//!
//! Recherche d'une tuile, dans l'ordre :
//!   1. dossier local (lecture seule) `local_dir`
//!   2. dossier de cache `cache_dir` (tuiles téléchargées précédemment)
//!   3. téléchargement depuis le bucket AWS public (si `download` est actif), puis stockage dans le cache
//!
//! Les tuiles absentes du jeu Copernicus (océan ouvert) sont traitées comme altitude 0 m.
//! Les altitudes sont des hauteurs orthométriques (géoïde EGM2008) du modèle de *surface*
//! (bâtiments et végétation inclus).

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use lru::LruCache;
use tiff::decoder::{Decoder, DecodingResult};
use tiff::tags::Tag;

/// Identifiant d'une tuile : coin sud-ouest en degrés entiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TileKey {
    pub lat: i32,
    pub lon: i32,
}

impl TileKey {
    pub fn containing(lat: f64, lon: f64) -> Self {
        let mut lon_i = lon.floor() as i32;
        if lon_i >= 180 {
            lon_i -= 360;
        }
        if lon_i < -180 {
            lon_i += 360;
        }
        Self {
            lat: (lat.floor() as i32).clamp(-90, 89),
            lon: lon_i,
        }
    }

    /// Nom officiel Copernicus, ex. `Copernicus_DSM_COG_30_N45_00_E005_00_DEM`.
    pub fn name(&self) -> String {
        let ns = if self.lat >= 0 { 'N' } else { 'S' };
        let ew = if self.lon >= 0 { 'E' } else { 'W' };
        format!(
            "Copernicus_DSM_COG_30_{}{:02}_00_{}{:03}_00_DEM",
            ns,
            self.lat.abs(),
            ew,
            self.lon.abs()
        )
    }
}

/// Tuile décodée en mémoire.
#[derive(Debug)]
pub struct Tile {
    pub width: usize,
    pub height: usize,
    /// Longitude du centre du pixel (0,0)
    pub lon0: f64,
    /// Latitude du centre du pixel (0,0)
    pub lat0: f64,
    pub dlon: f64,
    pub dlat: f64,
    pub data: Vec<f32>,
}

impl Tile {
    #[inline]
    fn px(&self, row: usize, col: usize) -> f32 {
        self.data[row * self.width + col]
    }

    /// Plus proche voisin (utilisé aux bords pour aller chercher dans la tuile voisine).
    #[inline]
    fn nearest(&self, lat: f64, lon: f64) -> f32 {
        let c = ((lon - self.lon0) / self.dlon)
            .round()
            .clamp(0.0, (self.width - 1) as f64) as usize;
        let r = ((self.lat0 - lat) / self.dlat)
            .round()
            .clamp(0.0, (self.height - 1) as f64) as usize;
        self.px(r, c)
    }
}

fn clean(v: f32) -> f64 {
    // Copernicus n'a pas de nodata officiel ; on protège quand même contre les valeurs aberrantes.
    if v.is_finite() && v > -1000.0 && v < 9000.0 {
        v as f64
    } else {
        0.0
    }
}

/// Décode une tuile GeoTIFF (COG ou non) en mémoire.
pub fn read_geotiff(path: &Path) -> Result<Tile> {
    let f = File::open(path).with_context(|| format!("ouverture {}", path.display()))?;
    let mut dec = Decoder::new(BufReader::new(f)).context("décodage TIFF")?;
    let (w, h) = dec.dimensions()?;
    let scale = dec
        .get_tag_f64_vec(Tag::ModelPixelScaleTag)
        .context("ModelPixelScale manquant")?;
    let tie = dec
        .get_tag_f64_vec(Tag::ModelTiepointTag)
        .context("ModelTiepoint manquant")?;
    if scale.len() < 2 || tie.len() < 6 {
        bail!("géoréférencement GeoTIFF invalide");
    }
    // GTRasterTypeGeoKey (1025) : 1 = PixelIsArea (défaut), 2 = PixelIsPoint
    let mut pixel_is_point = false;
    if let Ok(keys) = dec.get_tag_u16_vec(Tag::GeoKeyDirectoryTag) {
        for k in keys.chunks(4).skip(1) {
            if k.len() == 4 && k[0] == 1025 && k[1] == 0 {
                pixel_is_point = k[3] == 2;
            }
        }
    }
    let nodata: Option<f32> = dec
        .get_tag_ascii_string(Tag::GdalNodata)
        .ok()
        .and_then(|s| s.trim_matches(char::from(0)).trim().parse().ok());

    let (dlon, dlat) = (scale[0], scale[1]);
    let (i, j, x, y) = (tie[0], tie[1], tie[3], tie[4]);
    let mut lon0 = x - i * dlon;
    let mut lat0 = y + j * dlat;
    if !pixel_is_point {
        lon0 += dlon / 2.0;
        lat0 -= dlat / 2.0;
    }

    let mut data: Vec<f32> = match dec.read_image().context("lecture des pixels")? {
        DecodingResult::F32(v) => v,
        DecodingResult::F64(v) => v.into_iter().map(|x| x as f32).collect(),
        DecodingResult::I16(v) => v.into_iter().map(|x| x as f32).collect(),
        DecodingResult::U16(v) => v.into_iter().map(|x| x as f32).collect(),
        DecodingResult::I32(v) => v.into_iter().map(|x| x as f32).collect(),
        _ => bail!("type de pixel non supporté"),
    };
    if data.len() != (w as usize) * (h as usize) {
        bail!("taille de données inattendue (bandes multiples ?)");
    }
    if let Some(nd) = nodata {
        for v in data.iter_mut() {
            if *v == nd {
                *v = 0.0;
            }
        }
    }
    Ok(Tile {
        width: w as usize,
        height: h as usize,
        lon0,
        lat0,
        dlon,
        dlat,
        data,
    })
}

#[derive(Debug, Clone)]
pub struct DemConfig {
    pub local_dir: Option<PathBuf>,
    pub cache_dir: PathBuf,
    pub download: bool,
    pub base_url: String,
    pub mem_tiles: usize,
    pub http_timeout: Duration,
}

impl Default for DemConfig {
    fn default() -> Self {
        Self {
            local_dir: None,
            cache_dir: PathBuf::from("./dem-cache"),
            download: true,
            base_url: "https://copernicus-dem-90m.s3.amazonaws.com".into(),
            mem_tiles: 64,
            http_timeout: Duration::from_secs(120),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskStatus {
    /// Déjà présente (dossier local ou cache)
    Present,
    /// Téléchargée à l'instant
    Downloaded,
    /// N'existe pas dans le jeu Copernicus (mer)
    Sea,
    /// Absente et téléchargement désactivé
    Absent,
}

/// Source de tuiles avec cache LRU en mémoire.
pub struct Dem {
    cfg: DemConfig,
    cache: Mutex<LruCache<TileKey, Option<Arc<Tile>>>>,
    key_locks: Mutex<HashMap<TileKey, Arc<Mutex<()>>>>,
    agent: ureq::Agent,
}

impl Dem {
    pub fn new(cfg: DemConfig) -> Result<Self> {
        std::fs::create_dir_all(&cfg.cache_dir)
            .with_context(|| format!("création du cache {}", cfg.cache_dir.display()))?;
        let agent = ureq::AgentBuilder::new()
            .timeout(cfg.http_timeout)
            .try_proxy_from_env(!no_proxy_matches(&cfg.base_url))
            .build();
        let n = NonZeroUsize::new(cfg.mem_tiles.max(1)).unwrap();
        Ok(Self {
            cfg,
            cache: Mutex::new(LruCache::new(n)),
            key_locks: Mutex::new(HashMap::new()),
            agent,
        })
    }

    pub fn config(&self) -> &DemConfig {
        &self.cfg
    }

    fn candidates(&self, key: TileKey) -> Vec<PathBuf> {
        let name = key.name();
        let mut v = Vec::new();
        for dir in self
            .cfg
            .local_dir
            .iter()
            .chain(std::iter::once(&self.cfg.cache_dir))
        {
            v.push(dir.join(format!("{name}.tif")));
            v.push(dir.join(&name).join(format!("{name}.tif")));
        }
        v
    }

    fn missing_marker(&self, key: TileKey) -> PathBuf {
        self.cfg.cache_dir.join(format!("{}.missing", key.name()))
    }

    /// Charge une tuile (mémoire → disque → téléchargement). `None` = pas de tuile (océan).
    pub fn tile(&self, key: TileKey) -> Result<Option<Arc<Tile>>> {
        if let Some(t) = self.cache.lock().unwrap().get(&key) {
            return Ok(t.clone());
        }
        // Verrou par tuile pour éviter les téléchargements/décodages concurrents en double.
        let lock = {
            let mut m = self.key_locks.lock().unwrap();
            m.entry(key)
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let _g = lock.lock().unwrap();
        if let Some(t) = self.cache.lock().unwrap().get(&key) {
            return Ok(t.clone());
        }
        let loaded = self.load(key)?;
        self.cache.lock().unwrap().put(key, loaded.clone());
        Ok(loaded)
    }

    fn load(&self, key: TileKey) -> Result<Option<Arc<Tile>>> {
        for p in self.candidates(key) {
            if p.is_file() {
                tracing::debug!(tile = %key.name(), path = %p.display(), "tuile locale");
                return Ok(Some(Arc::new(read_geotiff(&p)?)));
            }
        }
        if self.missing_marker(key).exists() {
            return Ok(None);
        }
        if !self.cfg.download {
            // Sans téléchargement, une tuile absente est considérée comme de la mer.
            tracing::warn!(tile = %key.name(), "tuile absente localement et téléchargement désactivé : altitude 0 m");
            return Ok(None);
        }
        match self.download(key)? {
            Some(path) => Ok(Some(Arc::new(read_geotiff(&path)?))),
            None => {
                let _ = File::create(self.missing_marker(key));
                Ok(None)
            }
        }
    }

    /// Garantit la présence de la tuile sur disque (sans la décoder). Utilisé par `prefetch`.
    pub fn ensure_on_disk(&self, key: TileKey) -> Result<DiskStatus> {
        if self.candidates(key).iter().any(|p| p.is_file()) {
            return Ok(DiskStatus::Present);
        }
        if self.missing_marker(key).exists() {
            return Ok(DiskStatus::Sea);
        }
        if !self.cfg.download {
            return Ok(DiskStatus::Absent);
        }
        match self.download(key)? {
            Some(p) => {
                // Contrôle minimal : en-tête TIFF valide.
                let mut magic = [0u8; 4];
                File::open(&p)?.read_exact(&mut magic)?;
                if &magic[..2] != b"II" && &magic[..2] != b"MM" {
                    let _ = std::fs::remove_file(&p);
                    bail!("{} : fichier téléchargé invalide", key.name());
                }
                Ok(DiskStatus::Downloaded)
            }
            None => {
                let _ = File::create(self.missing_marker(key));
                Ok(DiskStatus::Sea)
            }
        }
    }

    fn download(&self, key: TileKey) -> Result<Option<PathBuf>> {
        let name = key.name();
        let url = format!(
            "{}/{name}/{name}.tif",
            self.cfg.base_url.trim_end_matches('/')
        );
        tracing::info!(%url, "téléchargement de tuile");
        let resp = match self.agent.get(&url).call() {
            Ok(r) => r,
            // Le bucket renvoie 403/404 pour les tuiles inexistantes (océan).
            Err(ureq::Error::Status(404 | 403, _)) => {
                tracing::info!(tile = %name, "tuile inexistante (mer)");
                return Ok(None);
            }
            Err(e) => return Err(anyhow!("téléchargement {url}: {e}")),
        };
        let dir = self.cfg.cache_dir.join(&name);
        std::fs::create_dir_all(&dir)?;
        let dest = dir.join(format!("{name}.tif"));
        let tmp = dir.join(format!("{name}.tif.part"));
        {
            let mut out = File::create(&tmp)?;
            let mut reader = resp.into_reader();
            let mut buf = vec![0u8; 1 << 16];
            loop {
                let n = reader.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                out.write_all(&buf[..n])?;
            }
            out.sync_all()?;
        }
        std::fs::rename(&tmp, &dest)?;
        Ok(Some(dest))
    }

    /// Charge (en parallèle) toutes les tuiles couvrant une emprise et renvoie un instantané
    /// utilisable sans verrou pendant les calculs.
    pub fn tile_set(
        &self,
        south: f64,
        west: f64,
        north: f64,
        east: f64,
        max_tiles: usize,
    ) -> Result<TileSet> {
        let keys = keys_for_bbox(south, west, north, east);
        if keys.len() > max_tiles {
            bail!(
                "zone trop grande : {} tuiles nécessaires (max {max_tiles})",
                keys.len()
            );
        }
        use rayon::prelude::*;
        let loaded: Vec<(TileKey, Result<Option<Arc<Tile>>>)> = keys
            .par_iter()
            .with_max_len(1)
            .map(|k| (*k, self.tile(*k)))
            .collect();
        let mut tiles = HashMap::with_capacity(loaded.len());
        for (k, r) in loaded {
            tiles.insert(k, r?);
        }
        Ok(TileSet { tiles })
    }
}

/// Vrai si l'hôte de `url` est exclu du proxy par NO_PROXY / no_proxy (ureq 2 l'ignore).
fn no_proxy_matches(url: &str) -> bool {
    let host = url
        .split("://")
        .nth(1)
        .unwrap_or(url)
        .split(['/', ':'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let list = std::env::var("NO_PROXY")
        .or_else(|_| std::env::var("no_proxy"))
        .unwrap_or_default();
    list.split(',')
        .map(|s| s.trim().trim_start_matches('.').to_ascii_lowercase())
        .any(|p| !p.is_empty() && (p == "*" || host == p || host.ends_with(&format!(".{p}"))))
}

pub fn keys_for_bbox(south: f64, west: f64, north: f64, east: f64) -> Vec<TileKey> {
    let mut keys = Vec::new();
    let lat_a = south.floor() as i32;
    let lat_b = north.floor() as i32;
    let lon_a = west.floor() as i32;
    let lon_b = east.floor() as i32;
    for lat in lat_a.max(-90)..=lat_b.min(89) {
        for lon in lon_a..=lon_b {
            let k = TileKey::containing(lat as f64 + 0.5, lon as f64 + 0.5);
            if !keys.contains(&k) {
                keys.push(k);
            }
        }
    }
    keys
}

/// Ensemble de tuiles figé pour un calcul.
pub struct TileSet {
    tiles: HashMap<TileKey, Option<Arc<Tile>>>,
}

impl TileSet {
    pub fn len(&self) -> usize {
        self.tiles.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tiles.is_empty()
    }

    fn nearest_global(&self, lat: f64, lon: f64) -> f64 {
        match self.tiles.get(&TileKey::containing(lat, lon)) {
            Some(Some(t)) => clean(t.nearest(lat, lon)),
            _ => 0.0,
        }
    }

    /// Altitude (m) par interpolation bilinéaire ; 0 m pour la mer ou hors emprise chargée.
    pub fn elevation(&self, lat: f64, lon: f64) -> f64 {
        let lon = normalize_lon(lon);
        let t = match self.tiles.get(&TileKey::containing(lat, lon)) {
            Some(Some(t)) => t,
            _ => return 0.0,
        };
        let fc = (lon - t.lon0) / t.dlon;
        let fr = (t.lat0 - lat) / t.dlat;
        let c0 = fc.floor();
        let r0 = fr.floor();
        let (wx, wy) = (fc - c0, fr - r0);
        let (c0, r0) = (c0 as i64, r0 as i64);
        let get = |r: i64, c: i64| -> f64 {
            if r >= 0 && c >= 0 && (r as usize) < t.height && (c as usize) < t.width {
                clean(t.px(r as usize, c as usize))
            } else {
                // Pixel dans une tuile voisine
                let la = t.lat0 - r as f64 * t.dlat;
                let lo = normalize_lon(t.lon0 + c as f64 * t.dlon);
                self.nearest_global(la, lo)
            }
        };
        let v00 = get(r0, c0);
        let v01 = get(r0, c0 + 1);
        let v10 = get(r0 + 1, c0);
        let v11 = get(r0 + 1, c0 + 1);
        let top = v00 * (1.0 - wx) + v01 * wx;
        let bot = v10 * (1.0 - wx) + v11 * wx;
        top * (1.0 - wy) + bot * wy
    }
}

pub fn normalize_lon(lon: f64) -> f64 {
    let mut l = lon;
    while l >= 180.0 {
        l -= 360.0;
    }
    while l < -180.0 {
        l += 360.0;
    }
    l
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tile_names() {
        assert_eq!(
            TileKey::containing(45.5, 5.2).name(),
            "Copernicus_DSM_COG_30_N45_00_E005_00_DEM"
        );
        assert_eq!(
            TileKey::containing(-0.5, -0.5).name(),
            "Copernicus_DSM_COG_30_S01_00_W001_00_DEM"
        );
        assert_eq!(
            TileKey::containing(48.85, 2.35).name(),
            "Copernicus_DSM_COG_30_N48_00_E002_00_DEM"
        );
        assert_eq!(
            TileKey::containing(-33.9, 151.2).name(),
            "Copernicus_DSM_COG_30_S34_00_E151_00_DEM"
        );
    }

    #[test]
    fn bbox_keys() {
        let k = keys_for_bbox(45.2, 5.8, 46.1, 6.3);
        assert_eq!(k.len(), 4);
    }
}

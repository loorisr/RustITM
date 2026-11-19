# itm-api

> **⚠️ 100% vibe coded by Claude Opus 5.5, for testing.**
> Every line of this project (code, tests, web UI, Docker and CI setup, this README) was written by Claude Opus 5.5, an AI model, as an experiment. No human wrote or reviewed it. Treat it as a test project, not as validated engineering software: verify the results before relying on them for real-world radio planning.
>
> What has been checked: the ITM port reproduces the 5 official NTIA test cases, and the full test suite passes. A headless-browser end-to-end test of the web UI ran with no JavaScript errors. What has **not** been checked yet: it has never run against real Copernicus tiles (the tests use synthetic tiles in the exact GLO-90 format), and the Docker image has never been built (the CI smoke test will be its first build).

A Rust HTTP API for radio propagation calculations, using the **ITM (Longley-Rice)** model and the **Copernicus GLO-90** digital elevation model (DEM).

- **ITM in pure Rust** (`crates/itm`): a faithful port of the NTIA v1.3 reference implementation, point-to-point mode. The 5 official NTIA test cases are reproduced to 0.01 dB; they cover line-of-sight, diffraction and troposcatter.
- **Copernicus GLO-90**: COG tiles are read in pure Rust, without GDAL. Tiles come from a local folder and/or are downloaded automatically from the public AWS bucket, with a disk cache and an in-memory LRU cache.
- **Coverage maps built from point-to-point radials**: ITM's statistical "area" mode is not used. Every pixel gets a real P2P calculation over the actual terrain, as in SPLAT!/Signal-Server. The output grid is **the native GLO-90 grid**: each coverage pixel is one DEM pixel. Output as PNG, GeoTIFF, JSON or raw float32.
- **Web UI embedded in the binary**, at `http://localhost:8080/`: site management, links with a terrain profile and Fresnel zone, and stackable coverage layers.
- **Static multi-architecture Docker image** (amd64/arm64), published to GHCR by GitHub Actions.

Note: the web UI, API error messages, log lines and code comments are in French.

## Getting started

### Docker (recommended)

```bash
docker compose up -d                       # builds the image locally
# or use the published image:
ITM_IMAGE=ghcr.io/<owner>/itm-api:latest docker compose up -d
# UI: http://localhost:8080/
```

Without compose:

```bash
docker run -d -p 8080:8080 -v itm-dem:/data/dem-cache ghcr.io/<owner>/itm-api:latest
# with a read-only folder of local tiles:
docker run -d -p 8080:8080 -v /path/to/glo90:/dem:ro -e DEM_LOCAL_DIR=/dem ghcr.io/<owner>/itm-api:latest
```

The image is built `FROM scratch`. It contains only the static (musl) binary, which embeds the web UI and the TLS root certificates. It runs as user 10001 (non-root), and its `HEALTHCHECK` uses `itm-server healthcheck`. The tile cache lives in the `/data/dem-cache` volume. If you mount a host folder instead of a named volume, it must be writable by UID 10001 (`chown 10001 ./dem-cache`).

**Offline use.** First pre-download the tiles for a region:

```bash
docker compose run --rm itm-api prefetch --bbox 44,5,46.5,7.5          # south,west,north,east
docker compose run --rm itm-api prefetch --center 45.19,5.72 --radius-km 150
```

Then run the service with `DEM_DOWNLOAD=false`. The OpenStreetMap background map in the UI still needs internet access.

### Without Docker

```bash
cargo build --release
./target/release/itm-server              # same as `itm-server serve`; listens on 0.0.0.0:8080
./target/release/itm-server prefetch --bbox 44,5,46.5,7.5
./target/release/itm-server --help
```

### Configuration (CLI option or environment variable)

| Variable | Default | Purpose |
|---|---|---|
| `ITM_BIND` | `0.0.0.0:8080` | Listen address |
| `DEM_LOCAL_DIR` | — | Read-only tile folder (checked first) |
| `DEM_CACHE_DIR` | `./dem-cache` | Cache for downloaded tiles |
| `DEM_DOWNLOAD` | `true` | Download missing tiles automatically |
| `DEM_BASE_URL` | `https://copernicus-dem-90m.s3.amazonaws.com` | Tile source |
| `DEM_MEM_TILES` | `64` | Tiles kept in memory (~5.8 MB each) |
| `MAX_P2P_KM` | `1000` | Max point-to-point distance |
| `MAX_RADIUS_KM` | `150` | Max coverage radius |
| `MAX_GRID_SIDE` | `6001` | Max grid side (pixels) |
| `MAX_TILES` | `64` | Max tiles per request |
| `PROFILE_STEP_M` | `92.66` (3″) | Point-to-point profile step |
| `COVERAGE_CONCURRENCY` | `2` | Simultaneous coverage jobs (beyond that: HTTP 503) |
| `RUST_LOG` | `info` | Log level |

Local tiles are looked up as both `DIR/<name>.tif` and `DIR/<name>/<name>.tif` (the AWS bucket layout), using the official name, e.g. `Copernicus_DSM_COG_30_N45_00_E005_00_DEM`. A tile that doesn't exist in the Copernicus dataset (open ocean) is treated as 0 m. This is remembered with a `.missing` file in the cache, so the tile isn't requested again. `HTTPS_PROXY` and `NO_PROXY` are honoured.

To pre-fill a local folder with the AWS CLI: `aws s3 sync --no-sign-request s3://copernicus-dem-90m/ ./glo90 --exclude "*" --include "*N4[4-6]*E00[4-7]*"`.

## API

### Common ITM parameters

| Field | Default | |
|---|---|---|
| `frequency_mhz` | *required* | 20–20,000 MHz (warning outside 40–10,000) |
| `polarization` | `vertical` | `horizontal` / `vertical` |
| `climate` | `continental_temperate` | name or code 1–7: `equatorial`, `continental_subtropical`, `maritime_subtropical`, `desert`, `continental_temperate`, `maritime_temperate_over_land`, `maritime_temperate_over_sea` |
| `n0` | `301` | Surface refractivity (250–400 N-units) |
| `epsilon` | `15` | Ground relative permittivity |
| `sigma` | `0.005` | Ground conductivity (S/m) |
| `mdvar` | `12` | Mode of variability: 0 single message, 1 accidental, 2 mobile, 3 broadcast; +10 removes location variability, +20 removes situation variability |
| `time`, `location`, `situation` | `50` | Percentages (strictly between 0 and 100) |
| `eirp_dbm` | — | EIRP, needed for received power and field strength |
| `rx_gain_dbi` | `0` | Receive antenna gain |

### `POST /v1/p2p`: point-to-point link

```bash
curl -X POST localhost:8080/v1/p2p -H 'content-type: application/json' -d '{
  "tx": {"lat": 45.05, "lon": 5.50, "height_m": 30},
  "rx": {"lat": 45.35, "lon": 6.10, "height_m": 2},
  "frequency_mhz": 446, "eirp_dbm": 37, "include_profile": false
}'
```

The response contains `path_loss_db` (basic transmission loss), `free_space_loss_db`, `excess_loss_db`, `rx_power_dbm`, `field_strength_dbuvm`, `distance_km`, `bearing_deg` and `tx_ground_m` / `rx_ground_m`, plus:

- `mode`: the region of ITM's reference curve (`line_of_sight`, `diffraction`, `troposcatter`), exactly as in the NTIA reference;
- `path_geometry`: the actual path geometry over the terrain (`line_of_sight`, `single_horizon`, `double_horizon`). The two can differ: ITM can stay in its "LOS" region on an obstructed path;
- `warnings`: NTIA warning flags and descriptions;
- `intermediate`: Δh, effective heights, horizons, Ns;
- `profile`, optional (step and elevations).

Out-of-range input returns HTTP 422 with the NTIA error code (`itm_code`).

### `POST /v1/coverage`: coverage map

```bash
curl -X POST localhost:8080/v1/coverage -H 'content-type: application/json' -o cov.png -D - -d '{
  "tx": {"lat": 45.12, "lon": 5.62, "height_m": 30},
  "rx_height_m": 2, "radius_km": 30,
  "frequency_mhz": 446, "eirp_dbm": 37,
  "output": "rx_power", "format": "png"
}'
```

| Field | Default | |
|---|---|---|
| `radius_km` | *required* | ≤ `MAX_RADIUS_KM` |
| `downsample` | `1` | 1 = native GLO-90 grid; 2, 4… (max 8) = one GLO-90 pixel out of N, for a quick preview |
| `rx_height_m` | `2` | |
| `output` | `path_loss` | `path_loss` (dB), `rx_power` (dBm), `field_strength` (dBµV/m). The last two need `eirp_dbm` |
| `format` | `png` | `png` (RGBA, transparent below `color_weak`), `geotiff` (float32, EPSG:4326, nodata −9999), `json`, `f32` (raw little-endian float32, rows north to south, NaN = no data; dimensions in the `x-width`/`x-height` headers) |
| `color_weak`, `color_strong` | depends on `output` | PNG palette bounds |
| `png_alpha` | `200` | PNG opacity |

Response headers give `x-bounds` (`west,south,east,north`, ready for a Leaflet `L.imageOverlay`), `x-unit` and `x-coverage-stats` (radials, ITM calls, duration).

**Grid.** The output grid is the GLO-90 grid: 3″ in latitude (≈ 93 m). In longitude the step is 3″ below 50° latitude, then 4.5″, 6″, 9″, 15″ and 30″ towards the poles, matching the Copernicus tiles. Coverage pixel centres coincide with DEM pixel centres (checked with GDAL). The radial step is 3″ (≈ 93 m).

**Method.** There are about 2πR / (smallest pixel side) radials, capped at 7200. Each radial's terrain profile is extracted once, then ITM P2P is called at every step with the sub-profile `[0..k]`. The polar result is resampled onto the lat/lon grid by bilinear interpolation in (azimuth, distance). In tests, pixel values match a direct `/v1/p2p` call to the same point to within 0.1 dB.

**Measured performance** (2 cores, about 4 µs per ITM call): at 45° latitude on the native grid, a 25 km radius gives a 767 × 541 px grid and about 650,000 ITM calls, computed in 1.3 s. The work is parallelised per radial with rayon, so time scales roughly with the number of cores.

### Elevation

- `GET /v1/elevation?lat=45.2&lon=5.75`
- `POST /v1/elevation` with `{"points":[{"lat":..,"lon":..}, ...]}` (up to 10,000 points)

### Other

`GET /health`, `GET /v1` (index), `GET /v1/info` (version, limits, grid).

## Web UI

The UI is served at `/`. It is plain JavaScript with no build step, and Leaflet 1.9.4 is embedded in the binary (BSD-2 licence, `web/vendor/leaflet/LICENSE`).

- **Sites**: add by clicking on the map, move by drag-and-drop. Each site has a name, height, EIRP and colour. Sites are saved in the browser, with JSON import/export.
- **Link**: a transmitter to a point on the map or to another site; shortcut: <kbd>Shift</kbd> + click. Results show received power, field strength, losses, path geometry, ITM mode and warnings. A **terrain profile** shows the curved earth (k = 4/3), the line of sight, the first Fresnel zone, and obstacles that intrude into 60% of F1. Hovering over the profile moves a cursor along the path on the map.
- **Coverage**: native grid or ×2/×4 preview, with a cost estimate before you run it. Layers can be stacked. Each layer has its own opacity and colour range (changed without recalculating, since colouring happens in the browser), and exports to GeoTIFF or PNG.
- **Radio**: band presets (PMR446, LoRa, marine VHF, GSM, Wi-Fi…), ground types, climate and advanced ITM parameters.
- The status bar shows the cursor position, DEM elevation and the value of the coverage layer under the cursor.

The UI fetches coverage in the `f32` format and reprojects it row by row to Web Mercator in the browser, so it lines up exactly under Leaflet. PNG and GeoTIFF files produced by the server stay in EPSG:4326.

## CI / publishing

- `.github/workflows/ci.yml`: `fmt`, `clippy -D warnings`, the tests (including reading real GDAL-written COGs) and a JavaScript syntax check. It then builds the amd64 image and smoke-tests it (`/health`, `/v1/info`, UI, P2P, Docker health check).
- `.github/workflows/release.yml`: builds a multi-arch `linux/amd64,linux/arm64` image and pushes it to `ghcr.io/<owner>/<repo>`. Pushes to `main` produce the `edge` tag; tags `vX.Y.Z` produce `X.Y.Z`, `X.Y` and `latest`. The arm64 build runs under QEMU emulation: it is slower (roughly 10–30 min), but needs no cross-compiler.

To publish: push the repository to GitHub, then `git tag v0.2.0 && git push --tags`. The GHCR package is private by default; make it public in its *Package settings* if needed.

## Things to know

- **GLO-90 is a surface model (DSM)**: it includes buildings and tree canopy. In towns or forests, a receive antenna "on the ground" is actually sitting on the rooftops or treetops. Antenna heights are relative to that surface.
- Elevations are relative to the EGM2008 geoid, which suits ITM.
- The output grid follows the transmitter's latitude band. A coverage area that crosses 50°, 60°… latitude keeps the longitude step of the transmitter's band.
- ITM's validity range: 20 MHz–20 GHz, 1–2000 km, antennas from 0.5 to 3000 m. Below 1 km ITM raises a warning and results are unreliable. In coverage maps, the first 2 steps reuse the value of the 3rd.
- Geometry is spherical (great circle, R = 6371 km), and latitude is limited to ±85°.

## Tests

```bash
cargo test                                   # NTIA cases, geometry, DEM, simulated HTTP download
# read real GDAL COGs (Deflate + floating-point predictor, tiled like Copernicus):
pip install rasterio && python3 scripts/make_tiles.py /tmp/tiles
ITM_TEST_TILES=/tmp/tiles cargo test
cargo run --release -p itm --example bench   # cost of one ITM call
```

## Licences

- `crates/itm` is derived from the NTIA/ITS ITM software (https://github.com/NTIA/itm), a work of the US government that is not subject to copyright in the United States. The C++ → Rust port was done in 2026.
- Copernicus DEM GLO-90 data is free to reuse. Its licence requires crediting the source (DLR / Airbus / the EU and ESA Copernicus programme) in derived products.

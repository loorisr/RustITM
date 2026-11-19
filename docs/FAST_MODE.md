# ITM fast mode (`fast`)

The `/v1/coverage` request accepts a `fast` field, modelled on Signal-Server's `-fast N`
option. It only affects coverage maps; `/v1/p2p` always runs the exact reference model.

| `fast` | Mode | Result |
|---|---|---|
| absent, `false`, `0` | exact (default) | NTIA ITM v1.3 reference at every point |
| `1` | quasi-exact | fast horizons and prefix sums; Δh still computed at every point |
| `N` ≥ 2 | fast | as above, and Δh recomputed every N steps, interpolated in between |
| `true` | fast | `N = 50` (same default as Signal-Server) |

The code is in `crates/itm/src/fast.rs` (`itm::FastRadial`).

## What changes

The coverage calls ITM for every step `k` of a radial, on the sub-profile `[0..=k]`. Without
`fast`, each call is O(k): horizon search, mean path height, Δh and the linear fits all scan
the profile. In fast mode, one `FastRadial` per radial keeps what can be reused from one
distance to the next:

1. **Horizons in O(log n).** Seen from either terminal, the horizon is the point of tangency
   with the upper convex hull of the points `(xᵢ, zᵢ − xᵢ²/2a)`, where `a` is the effective
   earth radius. The hull is built incrementally along the radial (amortised O(1) per point),
   and each query is a binary search. `a` depends on the mean height of each sub-profile, so
   the hull is built for a reference `a` and rebuilt when the curvature drifts by more than
   0.1 %. The chosen point is then refined along the hull with the exact `a`, and the angle
   is recomputed with the reference formula.
2. **Horizon distances to the bit.** The reference accumulates distances in floating point
   (`d_tx += xi`, `d_rx -= xi`), and the bounds of the linear fits that follow fall exactly on
   multiples of the step. A distance that is off by 1 ulp can move a fit bound by one point
   and change the loss by 0.5 dB. The TX distances come from a per-radial table. The RX
   distance replays the recurrence `x -= xi` in O(number of binades): within a binade the
   recurrence is an exact arithmetic progression.
3. **Prefix sums** for the mean path height and the linear fits: O(1) instead of O(n).
4. **Δh every N steps** (N ≥ 2): Δh is exact on the first N steps of each radial, then
   computed only at the anchors `N, 2N, 3N…` (with the reference horizons of the anchor
   sub-profile) and interpolated linearly in between. The last anchor is the end of the
   radial.

Longley-Rice and the variability stay the reference code. They are what remains at large N:
about 1 µs per call.

## Measurements

Copernicus GLO-90, transmitter at 45.5° N 6.0° E (French Alps), 30 m mast, receiver 2 m,
145 MHz, radius 100 km, `interp: nearest`: 3081 × 2161 px, 7200 radials, 4.47 M ITM calls.
Reference: the exact mode. Times: best of 3, Intel Core Ultra 7 265K (20 threads), tiles
already loaded. "Contour": pixels that change side of the −120 dBm threshold at 52 dBm EIRP.

| Mode | 20 threads | 1 thread | Speed-up (1 thread) | Mean error | ≤1 dB | ≤3 dB | >6 dB | Max | Contour |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| Exact | 0.81 s | 12.28 s | 1.00× | 0 | 100 % | 100 % | 0 % | 0 | 0 % |
| `fast 1` | 0.59 s | 8.95 s | 1.37× | 0.000 dB | 100 % | 100 % | 0 % | 0.0 dB | 0 % |
| `fast 5` | 0.45 s | | | 0.049 dB | 99.962 % | 99.998 % | 0.0000 % | 6.2 dB | 0.051 % |
| `fast 10` | 0.37 s | 5.38 s | 2.28× | 0.066 dB | 99.842 % | 99.995 % | 0.0001 % | 7.6 dB | 0.066 % |
| `fast 20` | 0.33 s | 4.72 s | 2.61× | 0.089 dB | 99.475 % | 99.969 % | 0.0020 % | 14.0 dB | 0.090 % |
| **`fast 50`** | 0.31 s | 4.31 s | **2.85×** | 0.150 dB | 97.960 % | **99.724 %** | 0.0277 % | 13.0 dB | 0.182 % |
| `fast 100` | 0.30 s | | | 0.235 dB | 95.458 % | 99.292 % | 0.1281 % | 11.9 dB | 0.312 % |

`interp: bilinear` on the same map: exact 1.09 s, `fast 1` 0.82 s (identical result),
`fast 20` 0.43 s (99.97 % within 3 dB), `fast 50` 0.42 s (99.73 %).

Flatter site (45.9° N 4.7° E, 50 km, `nearest`): `fast 20` is 1.96× faster than exact,
with 99.95 % of pixels within 3 dB.

Reading:

- `fast 1` gives the same map as the exact mode here (max difference 0.0 dB after rounding to
  float32); only the summation order of the mean path height differs, at the 1e-12 level.
- The speed-up levels off from N ≈ 20: what remains is the per-pixel Longley-Rice and
  variability computation, which does not depend on N.
- **Default `N = 50`** for `fast: true`, as in Signal-Server. N = 20 is a little more accurate
  (10× fewer pixels off by more than 6 dB) for nearly the same speed; ITM's own uncertainty
  is several dB, so either is reasonable.

Signal-Server also vectorises the loss computation (`lrprop`) over batches of 8 pixels
(AVX2 + libmvec). This port does not.

## Reproduce

```bash
cargo run --release -p itm-server --example fast_compare -- /path/to/glo90 45.5 6.0 100 1,10,20,50
RAYON_NUM_THREADS=1 cargo run --release -p itm-server --example fast_compare -- /path/to/glo90
INTERP=bilinear cargo run --release -p itm-server --example fast_compare -- /path/to/glo90
```

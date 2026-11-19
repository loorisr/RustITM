# Génère des tuiles synthétiques au format exact de Copernicus GLO-90 (COG, Deflate, prédicteur 3).
import sys, os, numpy as np, rasterio
from rasterio.transform import from_origin
out = sys.argv[1]
def terrain(lat, lon):
    # relief analytique : ondulations + massif gaussien près de (45.2, 5.75)
    return (400 + 250*np.sin(lat*12)*np.cos(lon*9)
            + 1800*np.exp(-(((lat-45.2)/0.08)**2 + ((lon-5.75)/0.1)**2))).astype(np.float32)
def tile(lat, lon, width):
    dy = 1/1200; dx = 1/width
    rows = np.arange(1200); cols = np.arange(width)
    la = (lat+1) - rows*dy; lo = lon + cols*dx      # centres de pixels (convention Copernicus)
    z = terrain(la[:,None], lo[None,:])
    name = f"Copernicus_DSM_COG_30_{'N' if lat>=0 else 'S'}{abs(lat):02d}_00_{'E' if lon>=0 else 'W'}{abs(lon):03d}_00_DEM"
    os.makedirs(f"{out}/{name}", exist_ok=True)
    tr = from_origin(lon - dx/2, lat + 1 + dy/2, dx, dy)
    with rasterio.open(f"{out}/{name}/{name}.tif", "w", driver="COG", width=width, height=1200, count=1,
                       dtype="float32", crs="EPSG:4326", transform=tr, compress="DEFLATE", predictor=3,
                       blocksize=1024) as d:
        d.write(z, 1)
for la, lo in [(44,5),(44,6),(45,5),(45,6)]:
    tile(la, lo, 1200)
tile(50, 5, 800)   # bande 50-60° : 800 colonnes
print("ok")

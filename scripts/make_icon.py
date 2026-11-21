"""App-Icon der Bench aus einem Entwurf mit hellem, deckendem Hintergrund
erzeugen: Hintergrund freistellen (wie ephemerline/tools/make_icon.py), Motiv
mit Rand auf ein transparentes Quadrat setzen und assets/256x256.png,
assets/icon.icns und assets/icon.ico schreiben.

Warum: Ein deckend weißes Quadrat zeigt das macOS-Dock als eckige weiße
Kachel, die größer wirkt als die Nachbarn (14.11.2026, gleiches Problem wie
bei ephemerline am 24.10.).

  python3 scripts/make_icon.py <entwurf.png> [anteil=0.80]

anteil = Kantenlänge des Motivs relativ zur Kachel (macOS-Raster ~0,8).
Braucht Pillow, numpy, scipy; für .icns iconutil (macOS)."""
import os, subprocess, sys, tempfile
import numpy as np
from PIL import Image, ImageFilter
from scipy import ndimage

src = Image.open(sys.argv[1]).convert('RGB')
share = float(sys.argv[2]) if len(sys.argv) > 2 else 0.80
os.chdir(os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', 'assets'))

a = np.asarray(src).astype(np.float32)
edge = np.concatenate([a[:8].reshape(-1, 3), a[-8:].reshape(-1, 3), a[:, :8].reshape(-1, 3), a[:, -8:].reshape(-1, 3)])
bg = np.median(edge, axis=0)
print('Hintergrund', bg)
d = np.sqrt(((a - bg) ** 2).sum(-1))
t0, t1 = 18.0, 70.0
al = np.clip((d - t0) / (t1 - t0), 0, 1)
lab, n = ndimage.label(al > 0.3)
sizes = np.bincount(lab.ravel())[1:]
keep = np.zeros_like(al, dtype=bool)
for i, s in enumerate(sizes):
    if s > sizes.max() * 0.02:  # alle nennenswerten Teile (beide Kettenglieder)
        keep |= lab == i + 1
keep = ndimage.binary_dilation(keep, iterations=4)
al = al * keep
safe = np.maximum(al, 1e-3)[..., None]
fg = np.clip((a - bg * (1 - al[..., None])) / safe, 0, 255)
im = Image.fromarray(np.dstack([fg, al * 255]).astype(np.uint8), 'RGBA')
bbox = Image.fromarray((al > 0.3).astype(np.uint8) * 255).getbbox()
print('Motiv', bbox)
im = im.crop(bbox)

N = 1024
cw, ch = im.size
k = N * share / max(cw, ch)
im = im.resize((max(1, round(cw * k)), max(1, round(ch * k))), Image.LANCZOS)
sq = Image.new('RGBA', (N, N), (0, 0, 0, 0))
sq.paste(im, ((N - im.size[0]) // 2, (N - im.size[1]) // 2))

def size(n):
    r = sq.resize((n, n), Image.LANCZOS)
    return r.filter(ImageFilter.UnsharpMask(radius=0.6, percent=60, threshold=0)) if n <= 48 else r

size(256).save('256x256.png')
size(256).save('icon.ico', sizes=[(n, n) for n in [16, 24, 32, 48, 64, 128, 256]],
               append_images=[size(n) for n in [16, 24, 32, 48, 64, 128]])
with tempfile.TemporaryDirectory() as t:
    iconset = os.path.join(t, 'icon.iconset')
    os.mkdir(iconset)
    for n in [16, 32, 128, 256, 512]:
        size(n).save(os.path.join(iconset, f'icon_{n}x{n}.png'))
        size(n * 2).save(os.path.join(iconset, f'icon_{n}x{n}@2x.png'))
    subprocess.run(['iconutil', '-c', 'icns', iconset, '-o', 'icon.icns'], check=True)
print('fertig: assets/256x256.png, icon.icns, icon.ico')

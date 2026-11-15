"""Draws hyperWIN's icon (assets/hyperwin.ico): a monitor with a second one behind it, on a blue
rounded square. Run it again after changing it: python tools/mkicon.py (needs Pillow)."""
from PIL import Image, ImageDraw
import os

def draw(size):
    k = 8  # drawn large, then scaled down: smooth edges
    n = size * k
    im = Image.new("RGBA", (n, n), (0, 0, 0, 0))
    d = ImageDraw.Draw(im)
    u = n / 64.0
    r = lambda x0, y0, x1, y1, rad, **kw: d.rounded_rectangle([x0 * u, y0 * u, x1 * u, y1 * u], radius=rad * u, **kw)
    r(2, 2, 62, 62, 13, fill=(0, 103, 192, 255))
    # the VM behind
    r(22, 13, 53, 35, 3, fill=(120, 180, 235, 255))
    # the PC's screen in front, with its stand
    r(10, 21, 44, 45, 3.5, fill=(255, 255, 255, 255))
    r(13, 24, 41, 42, 1.5, fill=(0, 72, 140, 255))
    d.rectangle([24.5 * u, 45 * u, 29.5 * u, 50 * u], fill=(255, 255, 255, 255))
    r(18, 49.5, 36, 53, 1.5, fill=(255, 255, 255, 255))
    return im.resize((size, size), Image.LANCZOS)

here = os.path.dirname(os.path.abspath(__file__))
out = os.path.join(here, "..", "assets", "hyperwin.ico")
sizes = [16, 20, 24, 32, 40, 48, 64, 128, 256]
images = [draw(s) for s in sizes]
images[-1].save(out, format="ICO", sizes=[(s, s) for s in sizes], append_images=images[:-1])
images[-1].save(os.path.join(here, "..", "assets", "hyperwin.png"))
print(out)

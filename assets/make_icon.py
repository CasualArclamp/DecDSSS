# made by CasualArclamp
# DecDSSS icon: a spread signal under a noise floor, its despread peak breaking
# out above it, and a +-1 chip sequence. Drawn at 4x and scaled down.
#   python assets/make_icon.py assets/decdsss
# writes decdsss.png (1024), decdsss-256.png, decdsss.ico (16-256) and
# decdsss-social.png (GitHub's 1280x640 social preview). Needs numpy and Pillow;
# the preview's lettering uses Windows' Bahnschrift and Segoe UI.
import sys, numpy as np
from PIL import Image, ImageDraw, ImageFilter, ImageFont

OUT = sys.argv[1]
S = 4096  # supersampled canvas for a 1024 icon
rng = np.random.default_rng(11)
CYAN = (58, 209, 255); MAG = (255, 61, 240); NAVY0 = (10, 14, 32); NAVY1 = (24, 30, 64)


def icon(size=1024):
    k = S / 1024
    img = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    # rounded-square body with a vertical gradient
    grad = np.linspace(0, 1, S)[:, None]
    body = np.zeros((S, S, 4), np.uint8)
    for c in range(3):
        body[..., c] = (NAVY1[c] * (1 - grad) + NAVY0[c] * grad).astype(np.uint8)
    body[..., 3] = 255
    body = Image.fromarray(body)
    mask = Image.new("L", (S, S), 0)
    ImageDraw.Draw(mask).rounded_rectangle([int(40 * k), int(40 * k), S - int(40 * k), S - int(40 * k)], int(210 * k), fill=255)
    img.paste(body, (0, 0), mask)

    def X(f):  # 0..1 across the plot area
        return (150 + f * 724) * k

    def Y(db):  # dB: +18 top .. -12 bottom of the plot
        return (175 + (18 - db) / 30 * 590) * k

    lay = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    d = ImageDraw.Draw(lay)
    # spread signal: a wide flat plateau with RRC-ish shoulders, 8 dB below the floor
    xs = np.linspace(0, 1, 400)
    edge = 0.06
    shape = np.clip(np.minimum((xs - 0.08) / edge, (0.92 - xs) / edge), 0, 1)
    shape = 0.5 - 0.5 * np.cos(np.pi * shape)
    spread_db = -8 + 20 * np.log10(shape + 1e-3) / 3
    pts = [(X(x), Y(max(-12, v))) for x, v in zip(xs, spread_db)]
    d.polygon([(X(0), Y(-12))] + pts + [(X(1), Y(-12))], fill=CYAN + (60,))
    d.line(pts, fill=CYAN + (170,), width=int(10 * k))
    # noise floor: a jagged line around 0 dB
    nx = np.linspace(0, 1, 130)
    nv = rng.normal(0, 1.1, len(nx))
    nv = np.convolve(nv, [0.25, 0.5, 0.25], "same")
    npts = [(X(x), Y(v)) for x, v in zip(nx, nv)]
    nl = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    ImageDraw.Draw(nl).line(npts, fill=(200, 205, 225, 255), width=int(16 * k), joint="curve")
    # despread peak: narrow, tall, glowing, straight through the noise
    px = 0.5
    w = 0.045
    peak = [(X(px - w * 1.6), Y(-12)), (X(px - w * 0.5), Y(4)), (X(px - 0.016), Y(17)), (X(px + 0.016), Y(17)),
            (X(px + w * 0.5), Y(4)), (X(px + w * 1.6), Y(-12))]
    pl = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    pd = ImageDraw.Draw(pl)
    pd.polygon(peak, fill=MAG + (255,))
    glow = pl.filter(ImageFilter.GaussianBlur(60 * k))
    img.alpha_composite(lay)
    img.alpha_composite(nl)
    for _ in range(2):
        img.alpha_composite(glow)
    img.alpha_composite(pl)
    # chip sequence along the bottom: +-1 spreading code
    chips = [1, 1, 1, -1, -1, 1, -1, 1, 1, -1, 1, -1, -1, -1, 1]
    cl = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    cd = ImageDraw.Draw(cl)
    y0, amp = 855 * k, 40 * k
    cw = 724 / len(chips)
    seq = []
    for i, c in enumerate(chips):
        x0, x1 = (150 + i * cw) * k, (150 + (i + 1) * cw) * k
        seq += [(x0, y0 - c * amp), (x1, y0 - c * amp)]
    cd.line(seq, fill=CYAN + (255,), width=int(12 * k), joint="curve")
    img.alpha_composite(cl)
    # clip everything to the rounded square
    a = Image.fromarray(np.minimum(np.asarray(img.getchannel("A")), np.asarray(mask)))
    img.putalpha(a)
    return img.resize((size, size), Image.LANCZOS)


big = icon(1024)
big.save(OUT + ".png")
sizes = [16, 24, 32, 48, 64, 128, 256]
big.save(OUT + ".ico", sizes=[(s, s) for s in sizes])
for s in (256,):
    big.resize((s, s), Image.LANCZOS).save(f"{OUT}-{s}.png")

# GitHub social preview, 1280x640
W, H = 1280, 640
sp = Image.new("RGBA", (W, H), NAVY0 + (255,))
g = np.linspace(0, 1, W)[None, :]
arr = np.zeros((H, W, 3), np.uint8)
for c in range(3):
    arr[..., c] = (NAVY1[c] * (1 - g) + NAVY0[c] * g).astype(np.uint8)
sp = Image.fromarray(arr).convert("RGBA")
sp.alpha_composite(big.resize((440, 440), Image.LANCZOS), (70, 100))
dr = ImageDraw.Draw(sp)
fb = ImageFont.truetype("C:/Windows/Fonts/bahnschrift.ttf", 124)
fb.set_variation_by_name("Bold")
fs = ImageFont.truetype("C:/Windows/Fonts/bahnschrift.ttf", 38)
fs.set_variation_by_name("SemiBold")
fm = ImageFont.truetype("C:/Windows/Fonts/segoeui.ttf", 30)
dr.text((570, 170), "DecDSSS", font=fb, fill=(255, 255, 255))
dr.text((575, 320), "NARROWBAND SPREAD SPECTRUM", font=fs, fill=CYAN)
dr.text((575, 368), "FOR AMATEUR RADIO", font=fs, fill=CYAN)
dr.text((575, 450), "DS-PSK + CSK  ·  voice  ·  pictures  ·  text  ·  files", font=fm, fill=(210, 215, 230))
dr.text((575, 495), "500 Hz  ·  2.4 kHz  ·  3 kHz", font=fm, fill=(210, 215, 230))
sp.convert("RGB").save(OUT + "-social.png")
print("ok")

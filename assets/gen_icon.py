#!/usr/bin/env python3
"""Generate Kova app icon — minimalist terminal prompt on dark background."""

from PIL import Image, ImageDraw, ImageFont
import subprocess, os, shutil

SIZE = 1024
CORNER_R = 220  # macOS-style rounded rect

# Colors
BG_TOP = (30, 30, 38)
BG_BOT = (18, 18, 24)
PROMPT_COLOR = (0, 230, 160)      # green prompt '>'
CURSOR_COLOR = (120, 140, 255)    # blue/purple block cursor
SUBTLE_LINE = (255, 255, 255, 18) # faint horizontal rules


def rounded_rect_mask(size, radius):
    """Create an anti-aliased rounded rectangle mask at 4x then downscale."""
    scale = 4
    big = Image.new("L", (size[0] * scale, size[1] * scale), 0)
    d = ImageDraw.Draw(big)
    d.rounded_rectangle([0, 0, big.width - 1, big.height - 1],
                        radius=radius * scale, fill=255)
    return big.resize(size, Image.LANCZOS)


def gradient_bg(size, top, bot):
    img = Image.new("RGB", size)
    for y in range(size[1]):
        t = y / (size[1] - 1)
        r = int(top[0] + (bot[0] - top[0]) * t)
        g = int(top[1] + (bot[1] - top[1]) * t)
        b = int(top[2] + (bot[2] - top[2]) * t)
        for x in range(size[0]):
            img.putpixel((x, y), (r, g, b))
    return img


def draw_icon():
    img = gradient_bg((SIZE, SIZE), BG_TOP, BG_BOT)
    draw = ImageDraw.Draw(img, "RGBA")

    # Subtle horizontal lines (like terminal rows)
    for y in range(180, SIZE, 56):
        draw.line([(100, y), (SIZE - 100, y)], fill=SUBTLE_LINE, width=1)

    # Try to use SF Mono or Menlo for the prompt character
    font_size = 420
    font = None
    for name in ["SF-Mono-Bold.otf", "SFMono-Bold.otf", "Menlo-Bold.ttf", "Menlo.ttc"]:
        for base in ["/System/Library/Fonts", "/Library/Fonts",
                     "/System/Library/Fonts/Supplemental"]:
            path = os.path.join(base, name)
            if os.path.exists(path):
                try:
                    font = ImageFont.truetype(path, font_size)
                    break
                except Exception:
                    pass
        if font:
            break
    if not font:
        font = ImageFont.load_default()

    # Draw '>' prompt
    prompt = ">"
    bbox = font.getbbox(prompt)
    pw, ph = bbox[2] - bbox[0], bbox[3] - bbox[1]
    px = SIZE // 2 - pw // 2 - 80
    py = SIZE // 2 - ph // 2 - bbox[1]

    # Slight glow behind prompt
    for offset in range(8, 0, -2):
        alpha = 15
        glow_col = (*PROMPT_COLOR, alpha)
        draw.text((px - offset, py), prompt, fill=glow_col, font=font)
        draw.text((px + offset, py), prompt, fill=glow_col, font=font)

    draw.text((px, py), prompt, fill=PROMPT_COLOR, font=font)

    # Block cursor
    cursor_w, cursor_h = 48, ph - 20
    cx = px + pw + 30
    cy = py + 10
    draw.rounded_rectangle([cx, cy, cx + cursor_w, cy + cursor_h],
                           radius=8, fill=CURSOR_COLOR)

    # Apply rounded rect mask
    mask = rounded_rect_mask((SIZE, SIZE), CORNER_R)
    # Composite on transparent
    result = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
    result.paste(img, mask=mask)

    return result


def make_icns(icon_img, out_dir):
    iconset = os.path.join(out_dir, "kova.iconset")
    if os.path.exists(iconset):
        shutil.rmtree(iconset)
    os.makedirs(iconset)

    sizes = [16, 32, 64, 128, 256, 512]
    for s in sizes:
        icon_img.resize((s, s), Image.LANCZOS).save(
            os.path.join(iconset, f"icon_{s}x{s}.png"))
        s2 = s * 2
        icon_img.resize((s2, s2), Image.LANCZOS).save(
            os.path.join(iconset, f"icon_{s}x{s}@2x.png"))

    icns_path = os.path.join(out_dir, "kova.icns")
    subprocess.run(["iconutil", "-c", "icns", iconset, "-o", icns_path], check=True)
    shutil.rmtree(iconset)
    print(f"Created {icns_path}")
    return icns_path


if __name__ == "__main__":
    out = os.path.dirname(os.path.abspath(__file__))
    icon = draw_icon()
    icon.save(os.path.join(out, "kova_icon.png"))
    make_icns(icon, out)

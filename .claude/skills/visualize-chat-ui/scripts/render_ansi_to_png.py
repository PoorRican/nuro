#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = ["pillow>=10.0.0"]
# ///

import re
import sys
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont


DEFAULT_FG = "#dbe2ea"
DEFAULT_BG = "#0b0f14"

BASE_COLORS = [
    "#1d1f21",
    "#cc6666",
    "#b5bd68",
    "#f0c674",
    "#81a2be",
    "#b294bb",
    "#8abeb7",
    "#c5c8c6",
]
BRIGHT_COLORS = [
    "#666666",
    "#d54e53",
    "#b9ca4a",
    "#e7c547",
    "#7aa6da",
    "#c397d8",
    "#70c0b1",
    "#ffffff",
]


def hex_to_rgb(color):
    color = color.lstrip("#")
    return (int(color[0:2], 16), int(color[2:4], 16), int(color[4:6], 16))


def rgb_to_hex(rgb):
    r, g, b = rgb
    return f"#{r:02x}{g:02x}{b:02x}"


def blend(fg, bg, factor):
    fr, fgc, fb = hex_to_rgb(fg)
    br, bgc, bb = hex_to_rgb(bg)
    return rgb_to_hex(
        (
            int(fr * factor + br * (1 - factor)),
            int(fgc * factor + bgc * (1 - factor)),
            int(fb * factor + bb * (1 - factor)),
        )
    )


def xterm_256_to_hex(index):
    if index < 0:
        index = 0
    if index > 255:
        index = 255
    if index < 8:
        return BASE_COLORS[index]
    if index < 16:
        return BRIGHT_COLORS[index - 8]
    if index < 232:
        i = index - 16
        r = i // 36
        g = (i % 36) // 6
        b = i % 6
        levels = [0, 95, 135, 175, 215, 255]
        return rgb_to_hex((levels[r], levels[g], levels[b]))
    gray = 8 + (index - 232) * 10
    return rgb_to_hex((gray, gray, gray))


def fresh_style():
    return {
        "fg": DEFAULT_FG,
        "bg": DEFAULT_BG,
        "bold": False,
        "dim": False,
        "italic": False,
        "underline": False,
        "inverse": False,
    }


def parse_codes(params):
    if params == "":
        return [0]
    out = []
    for item in params.split(";"):
        if item == "":
            out.append(0)
            continue
        try:
            out.append(int(item))
        except ValueError:
            out.append(0)
    return out


def apply_sgr(style, codes):
    i = 0
    while i < len(codes):
        code = codes[i]
        if code == 0:
            style.clear()
            style.update(fresh_style())
        elif code == 1:
            style["bold"] = True
        elif code == 2:
            style["dim"] = True
        elif code == 3:
            style["italic"] = True
        elif code == 4:
            style["underline"] = True
        elif code == 7:
            style["inverse"] = True
        elif code == 22:
            style["bold"] = False
            style["dim"] = False
        elif code == 23:
            style["italic"] = False
        elif code == 24:
            style["underline"] = False
        elif code == 27:
            style["inverse"] = False
        elif 30 <= code <= 37:
            style["fg"] = BASE_COLORS[code - 30]
        elif code == 39:
            style["fg"] = DEFAULT_FG
        elif 40 <= code <= 47:
            style["bg"] = BASE_COLORS[code - 40]
        elif code == 49:
            style["bg"] = DEFAULT_BG
        elif 90 <= code <= 97:
            style["fg"] = BRIGHT_COLORS[code - 90]
        elif 100 <= code <= 107:
            style["bg"] = BRIGHT_COLORS[code - 100]
        elif code in (38, 48):
            is_fg = code == 38
            if i + 1 < len(codes):
                mode = codes[i + 1]
                if mode == 5 and i + 2 < len(codes):
                    value = xterm_256_to_hex(codes[i + 2])
                    if is_fg:
                        style["fg"] = value
                    else:
                        style["bg"] = value
                    i += 2
                elif mode == 2 and i + 4 < len(codes):
                    r = max(0, min(255, codes[i + 2]))
                    g = max(0, min(255, codes[i + 3]))
                    b = max(0, min(255, codes[i + 4]))
                    value = rgb_to_hex((r, g, b))
                    if is_fg:
                        style["fg"] = value
                    else:
                        style["bg"] = value
                    i += 4
                else:
                    i += 1
        i += 1


def resolve_style(style):
    fg = style["fg"]
    bg = style["bg"]
    if style["inverse"]:
        fg, bg = bg, fg
    if style["dim"]:
        fg = blend(fg, bg, 0.65)
    return (fg, bg, style["bold"], style["italic"], style["underline"])


def parse_line(raw_line, style, max_cols):
    row = [(" ", DEFAULT_FG, DEFAULT_BG, False, False, False) for _ in range(max_cols)]
    col = 0
    i = 0
    while i < len(raw_line):
        ch = raw_line[i]
        if ch == "\x1b" and i + 1 < len(raw_line):
            nxt = raw_line[i + 1]
            if nxt == "[":
                j = i + 2
                while j < len(raw_line) and not ("@" <= raw_line[j] <= "~"):
                    j += 1
                if j < len(raw_line):
                    cmd = raw_line[j]
                    params = raw_line[i + 2 : j]
                    if cmd == "m":
                        apply_sgr(style, parse_codes(params))
                    i = j + 1
                    continue
            elif nxt == "]":
                j = i + 2
                while j < len(raw_line):
                    if raw_line[j] == "\x07":
                        j += 1
                        break
                    if (
                        raw_line[j] == "\x1b"
                        and j + 1 < len(raw_line)
                        and raw_line[j + 1] == "\\"
                    ):
                        j += 2
                        break
                    j += 1
                i = j
                continue
            else:
                i += 2
                continue

        if ch == "\t":
            spaces = 4 - (col % 4)
            for _ in range(spaces):
                if col >= max_cols:
                    break
                row[col] = (" ", *resolve_style(style))
                col += 1
            i += 1
            continue

        if ch == "\r":
            col = 0
            i += 1
            continue

        if ord(ch) < 32:
            i += 1
            continue

        if col < max_cols:
            row[col] = (ch, *resolve_style(style))
            col += 1
        i += 1
    return row


def load_font(size):
    candidates = [
        "/System/Library/Fonts/Menlo.ttc",
        "/System/Library/Fonts/SFNSMono.ttf",
        "/System/Library/Fonts/Monaco.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
        "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
    ]
    for path in candidates:
        try:
            return ImageFont.truetype(path, size=size)
        except OSError:
            pass
    try:
        return ImageFont.truetype("DejaVuSansMono.ttf", size=size)
    except OSError:
        return ImageFont.load_default()


def measure_text_cell(font):
    probe = Image.new("RGB", (32, 32), DEFAULT_BG)
    draw = ImageDraw.Draw(probe)
    left, top, right, bottom = draw.textbbox((0, 0), "M", font=font)
    cell_w = max(1, right - left)
    if hasattr(font, "getmetrics"):
        ascent, descent = font.getmetrics()
        line_h = max(1, ascent + descent + 4)
        text_y_offset = 2
        underline_y_offset = min(line_h - 2, ascent + 2)
    else:
        line_h = max(1, bottom - top + 4)
        text_y_offset = 2
        underline_y_offset = max(1, line_h - 3)
    return cell_w, line_h, text_y_offset, underline_y_offset


def render_png(grid, max_cols, max_rows, out_path):
    font = load_font(size=15)
    cell_w, line_h, text_y_offset, underline_y_offset = measure_text_cell(font)

    padding_x = 16
    padding_y = 16
    img_w = int((padding_x * 2) + (max_cols * cell_w))
    img_h = int((padding_y * 2) + (max_rows * line_h))

    image = Image.new("RGB", (img_w, img_h), DEFAULT_BG)
    draw = ImageDraw.Draw(image)

    for row_idx, row in enumerate(grid):
        y = padding_y + row_idx * line_h

        c = 0
        while c < max_cols:
            bg = row[c][2]
            run_start = c
            c += 1
            while c < max_cols and row[c][2] == bg:
                c += 1
            if bg != DEFAULT_BG:
                x = padding_x + run_start * cell_w
                w = (c - run_start) * cell_w
                draw.rectangle((x, y, x + w, y + line_h), fill=bg)

        c = 0
        while c < max_cols:
            ch, fg, _bg, bold, _italic, underline = row[c]
            style_key = (fg, bold, underline)
            run_start = c
            chars = [ch]
            c += 1
            while c < max_cols:
                nch, nfg, _nbg, nbold, _nitalic, nunderline = row[c]
                if (nfg, nbold, nunderline) != style_key:
                    break
                chars.append(nch)
                c += 1

            text = "".join(chars)
            if text.strip() == "":
                continue

            x = padding_x + run_start * cell_w
            text_y = y + text_y_offset
            draw.text((x, text_y), text, font=font, fill=fg)
            if bold:
                draw.text((x + 1, text_y), text, font=font, fill=fg)
            if underline:
                underline_y = y + underline_y_offset
                draw.line((x, underline_y, x + len(text) * cell_w, underline_y), fill=fg, width=1)

    image.save(out_path, format="PNG")


def main():
    if len(sys.argv) != 5:
        print(
            "Usage: render_ansi_to_png.py <ansi_file> <png_out> <max_cols> <max_rows>",
            file=sys.stderr,
        )
        return 2

    in_path, out_path, max_cols_str, max_rows_str = sys.argv[1:5]
    max_cols = int(max_cols_str)
    max_rows = int(max_rows_str)

    raw = Path(in_path).read_bytes().decode("utf-8", "replace")
    raw = raw.replace("\r\n", "\n").replace("\r", "\n")
    raw = re.sub(r"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)", "", raw)
    lines = raw.split("\n")
    if lines and lines[-1] == "":
        lines = lines[:-1]

    if len(lines) > max_rows:
        lines = lines[-max_rows:]
    if len(lines) < max_rows:
        lines.extend([""] * (max_rows - len(lines)))

    style = fresh_style()
    grid = [parse_line(line, style, max_cols) for line in lines]

    render_png(grid, max_cols, max_rows, out_path)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

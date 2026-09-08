"""Generate the placeholder application icon for relief-forge.

The same stepped-relief glyph the web build uses as its favicon, so the desktop
installer, the Start Menu entry and the browser tab all carry one mark. Drawn
rather than committed as artwork: it is a placeholder, and regenerating it is a
one-line change.
"""

from PIL import Image, ImageDraw

SIZE = 256
SCALE = SIZE / 32.0
BACKGROUND = (25, 28, 30)
RELIEF = (233, 165, 66)

# M4 24 h4 v-6 h4 v-5 h4 V8 h4 v5 h4 v5 h4 v6 z — a stepped relief in section.
GLYPH = [
    (4, 24), (8, 24), (8, 18), (12, 18), (12, 13), (16, 13), (16, 8),
    (20, 8), (20, 13), (24, 13), (24, 18), (28, 18), (28, 24),
]

# Supersample, then downscale: PIL has no antialiased polygon fill.
OVER = 4
canvas = Image.new('RGBA', (SIZE * OVER, SIZE * OVER), (0, 0, 0, 0))
draw = ImageDraw.Draw(canvas)
draw.rounded_rectangle(
    [0, 0, SIZE * OVER - 1, SIZE * OVER - 1],
    radius=int(48 * OVER * SIZE / 256),
    fill=BACKGROUND,
)
draw.polygon([(x * SCALE * OVER, y * SCALE * OVER) for x, y in GLYPH], fill=RELIEF)
icon = canvas.resize((SIZE, SIZE), Image.LANCZOS)

root = r'C:\Users\phil\Documents\code\relief-forge\resources'
icon.save(root + r'\icon.png')
icon.save(
    root + r'\icon.ico',
    sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)],
)
print('wrote icon.png and icon.ico')

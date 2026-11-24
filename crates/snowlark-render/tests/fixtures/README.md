# Test fixtures

- `search-button.html`, `search-button.css`: a minimal stand-in for the header search form of
  https://en.wikipedia.org/wiki/Snowlark (Vector 2022 skin, Codex `cdx-button`), written by hand for this
  test: the same structure (a parser-created `<link rel=stylesheet>` in `<head>`, design tokens as custom
  properties with fallbacks, colours applied through `.cdx-button:enabled`, a 0.1s transition on them) and
  the same token values (#f8f9fa, #404244). No Wikimedia source text is copied, so they are under this
  repository's licence.
- `anim-rgb.gif`, `anim-rgb.png`, `anim-rgb.webp`, `anim-default.png`, `anim-move.gif`: animated images
  made for the animated image tests with Pillow 12.3 by this script (written for this repository):

  ```python
  from PIL import Image, ImageDraw
  RED, LIME, BLUE, YELLOW = (255, 0, 0, 255), (0, 255, 0, 255), (0, 0, 255, 255), (255, 255, 0, 255)
  frames = [Image.new("RGBA", (16, 16), c) for c in (RED, LIME, BLUE)]
  d = [100, 200, 300]
  frames[0].convert("RGB").save("anim-rgb.gif", save_all=True,
                                append_images=[f.convert("RGB") for f in frames[1:]], duration=d, loop=0)
  frames[0].save("anim-rgb.png", save_all=True, append_images=frames[1:], duration=d, loop=2)
  Image.new("RGBA", (16, 16), YELLOW).save("anim-default.png", save_all=True, append_images=frames,
                                           duration=d, loop=0, default_image=True)
  frames[0].save("anim-rgb.webp", save_all=True, append_images=frames[1:], duration=d, loop=1,
                 lossless=True)
  mv = []
  for i in range(3):
      im = Image.new("RGB", (30, 10), (255, 255, 255))
      ImageDraw.Draw(im).rectangle([i * 10, 0, i * 10 + 9, 9], fill=(255, 0, 0))
      mv.append(im)
  mv[0].save("anim-move.gif", save_all=True, append_images=mv[1:], duration=[100] * 3, loop=0,
             optimize=True)
  ```

  The GIF loops forever (NETSCAPE2.0 loop count 0), the APNG plays twice, the WebP once;
  `anim-default.png`'s default image (yellow) is not one of its frames; `anim-move.gif`'s second and
  third frames are 20 px wide pieces with transparent pixels where nothing changed.

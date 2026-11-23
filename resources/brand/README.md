# Brand assets — Posthaste

Final mark set. Hand-written SVG, rendered with headless Chrome; no image models.
Only the two marks that ship live here — exploration drafts stay outside the repo.

```
app-icon/        A2 / THE STAMP   the stamp mark (square, works at 16px)
brand-mark/      C3 / THE WAX     the brand mark and the application icon
scenarios.html   the files placed in real surfaces — open it in a browser
```

`scenarios.html` is a review page, not an asset: it drops the real PNGs into a title bar,
splash, about dialog, taskbar, browser tabs, installer, docs header, README header, social card
and print sheet, at the sizes they actually ship at.

## Which file, when

| File | Use at | Notes |
|---|---|---|
| `app-icon/icon-tiny-16.png`, `-32.png` | 16 / 32 | 16px simplified cut: no perforations, no postage dot, −4° |
| `app-icon/icon-64..512.png` | 64 – 512 | installer, Dock, taskbar |
| `app-icon/icon.svg` | any | 512 master, vector |
| `app-icon/stamp.png` | 40 – 256 | transparent stamp, no tile — the welcome panel |
| `brand-mark/badge.png`, `badge-512/256/128.png` | 128 – 1024 | launch screen, about, release art, docs header |
| `brand-mark/seal.png` | 16 – 256 | transparent seal, application icon and title bar |
| `brand-mark/badge.svg` | any | 1024 master, vector |
| `*/lockup.svg`, `lockup-dark.svg` | 1024 × 320 | README, site nav, docs header, email signature |
| `*/mono.svg`, `mono-inverse.svg` | any | one ink: print, stamps, embossing, watermark |

## Rules

- **C3 is normally kept at 128px and up.** Its rim, sheen and shadow have no room below that, and
  a flat cut stops being wax. The application icon is the deliberate exception: it uses the
  transparent seal (without the badge background or wordmark) at every Windows icon size, because
  the shell needs one mark that survives 16px. Do not use the full badge below 128px.
- A2 remains available for surfaces that need the stamp; it is no longer the application icon.
  Inside the app it is drawn as the transparent `stamp.png`, not as the tiled icon: the launcher
  tile belongs to the shell, and a panel should not carry it.
- At 128 the badge's name line is ~4px and reads as a grey rule, and the ring inscription has
  already turned into a fine dashed texture. Use 128 where the mark alone carries it
  (favicon-sized); use 256 and up wherever the name has to be read.
- **A2's 16px cut has no red postage dot.** The UI's red is reserved for DELETE (`#b85c5a`).
- Keep the wax red out of surfaces that already show DELETE's wine. Use the mark, not a recolour.
- Both marks are one system: same wax red `#E2604C`, same paper `#F2EDE3`, same two-part speed
  rhythm. C3 sets its own name in the inner ring — `POST` over the top arc, `HASTE` under the
  bottom, a dot at 3 and 9 o'clock, centre empty; A2 keeps the double chevron.
- The ring inscription is live Bahnschrift 500 pressed into the wax: one body layer plus a shadow
  offset up-left. Do not add a rim light — on strokes this thin it outlines both edges and reads
  as raised instead of pressed. Being live text, it needs the font; the PNGs and the ICO are
  rasterised on the build machine, so those do not.

## Packed so far

- The Windows `.ico` (`resources/icons/posthaste.ico`, seven frames) and the
  16 px title-bar mark (`posthaste-mark.png`) are packed from the C3 seal by
  `scripts/generate-icon.py`. `.icns` is not built.
- Not packed: 1280×630 social card, favicon set (16/32/48 + apple-touch 180 +
  maskable 512), transparent / `currentColor` mono SVGs (the mono files here
  are plates with a background).

## Name check

A practical collision review was done on 2026-11-20 (non-commercial, pre-1.0):
the `posthaste` crate name is free on crates.io, PyPI and npm hold unrelated
utilities, and GitHub has several same-named projects, including a REST API
testing toolkit. The trademark databases were not reachable from the build
environment and formal clearance was not performed.

## Provenance

The generators and exploration sheets are kept outside the repository. The
SVGs are plain and editable; the PNGs are Chrome
rasterisations at 1× with a LANCZOS ladder. `brand-mark/seal.png` is a
transparent Chrome render of the seal group from `badge.svg` (viewBox
`250 160 524 524`, 1024×1024, no background or wordmark); the icon packer
reduces it to every Windows size. `app-icon/stamp.png` is the same kind of
render of the stamp group from `app-icon/icon.svg` — tile and its hairline
removed, cropped to the stamp with a 4 % margin — used where the mark has to
float on the app's own surface instead of sitting on an icon tile.

Type in the lockups is a placeholder: Bahnschrift 700 / Consolas. No licensed face chosen.
Only the practical collision review above was done — Post Haste (Digital Rebellion)
and EasyPost remain known collisions, and the project is not affiliated with either.

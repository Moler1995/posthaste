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
| `brand-mark/seal.png` | 128 – 256 | transparent detailed seal: inscription + hare, ICO frames 128 and 256 |
| `brand-mark/seal-small.png` | 16 – 64 | transparent character-only seal: the hare alone on bare wax, the rest of the ICO and the 32px in-app mark |
| `brand-mark/badge.svg`, `seal-small.svg` | any | 1024 masters, vector |
| `*/lockup.svg`, `lockup-dark.svg` | 1024 × 320 | README, site nav, docs header, email signature |
| `*/mono.svg`, `mono-inverse.svg` | any | one ink: print, stamps, embossing, watermark |

## Rules

- The lockups float the seal on the document background — cream in `lockup.svg`,
  near-black in `lockup-dark.svg` — with no icon tile behind it. The seal is sized
  to the slot the badge tile used to occupy.
- **C3 is normally kept at 128px and up.** Its rim and shadow have no room below that, and a flat
  cut stops being wax. The application icon is the deliberate exception: it uses the transparent
  seal (without the badge background or wordmark) at every Windows icon size, because the shell
  needs one mark that survives 16px. Below 128px it uses `seal-small` — the same wax with the
  inscription dropped and the hare scaled up to fill the face, because the 26-unit ring text
  lands on two pixels at those sizes and turns the face into dark specks. Do not use the full
  badge below 128px.
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
  bottom — around a single dark hare; A2 keeps the double chevron.
- **The wax face is pressed, not modelled.** Light comes from the upper left, as everywhere in
  the UI, but the face is a nearly flat die impression, not a bowl: only a short shadow along
  its inner top edge and a thin reflection along its inner bottom edge. The rim between the face
  and the silhouette is the raised lip, light on its outer upper left and dark on its outer lower
  right, and a soft gloss lies over the upper left of the face. A fine grain breaks the colour
  into wax, and the blob casts a short contact shadow. Give `badge.svg`, both lockups and
  `seal-small.svg` the same shading. A deep saucer face, a uniform fill, or a specular dot all
  read as pressed plasticine — that is the look this rule replaced.
- **The rim's irregularity is gentle, not broken.** The twelve shallow humps are of similar size
  and the mass sits on the centre, so the edge reads as wax pressed under a seal. Deep dips,
  narrow points, or a strongly off-centre mass read as a chipped or damaged seal (残缺感), not as
  wax — an outline generated from a deliberately uneven profile was tried and rejected for
  exactly that. If the 16 px cut needs to read more like wax, change the shading or the face, not
  the edge.
- The ring inscription and the hare are the same ink: a dark body layer plus a faint light copy
  offset down-right, the wall that catches light. The inscription is live Bahnschrift 600 at 26
  units, so it needs the font; the PNGs and the ICO are rasterised on the build machine, so those
  do not. The hare is a single filled contour — a lowered head, one long ear laid back, one front
  leg and one hind leg — with no interior detail, so it survives the small sizes.

## Packed so far

- The Windows `.ico` (`resources/icons/posthaste.ico`, seven frames) and the
  32 px in-app mark (`posthaste-mark.png`, drawn at 16 logical px in the sidebar's
  brand row) are packed from the two C3 seals by `scripts/generate-icon.py`:
  frames below 128 px and the mark from `seal-small.png` (the hare alone), the
  128 and 256 px frames from `seal.png` (inscription + hare). `.icns` is not
  built.
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
reduces it to the 128 and 256 px frames, and reduces `seal-small.png` — the
same kind of render of `seal-small.svg` — to the smaller frames and the in-app
mark. `app-icon/stamp.png` is the same kind of
render of the stamp group from `app-icon/icon.svg` — tile and its hairline
removed, cropped to the stamp with a 4 % margin — used where the mark has to
float on the app's own surface instead of sitting on an icon tile.

Type in the lockups is a placeholder: Bahnschrift 700 / Consolas. No licensed face chosen.
Only the practical collision review above was done — Post Haste (Digital Rebellion)
and EasyPost remain known collisions, and the project is not affiliated with either.

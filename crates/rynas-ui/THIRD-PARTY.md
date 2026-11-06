# Third-party material in rynas-ui

| Component | Where | License |
|---|---|---|
| [Lucide](https://lucide.dev) icons (lucide-static 1.68.0, only the paths used) | `src/icons.rs` | ISC |
| [Inter](https://rsms.me/inter/) 4.1, variable, instanced to wght 400–700 / opsz 14, Latin subset | `assets/fonts/inter-latin.woff2` | SIL OFL 1.1 (`assets/fonts/Inter-LICENSE.txt`) |
| [qrcodegen](https://github.com/nayuki/QR-Code-generator) 1.8 (Rust crate, compiled in) | QR code of the TOTP enrollment, `src/qr.rs` | MIT |
| [JetBrains Mono](https://www.jetbrains.com/lp/mono/) 2.304, Regular, Latin subset | `assets/fonts/jbmono-latin.woff2` | SIL OFL 1.1 (`assets/fonts/JetBrainsMono-OFL.txt`) |
| File manager: the SynoFile interface (same author), ported | `files-app/` | MIT |
| [CodeMirror](https://codemirror.net) 6 (state, view, commands, search), bundled with esbuild | `files-app/vendor/codemirror/` | MIT (`files-app/vendor/codemirror/LICENSE`) |
| [libheif-js](https://github.com/catdad-experiments/libheif-js) 1.23.2 (libheif + libde265 in WebAssembly), unmodified | `files-app/vendor/libheif/` | LGPL-3.0 (`files-app/vendor/libheif/LICENSE`) |
| [micro](https://github.com/zyedidia/micro) syntax files, converted | `files-app/js/syntax/` | MIT |
| [eza](https://github.com/eza-community/eza) icon and file-type tables | `files-app/js/eza.js` | MIT / EUPL-1.2 |
| [Nerd Fonts](https://www.nerdfonts.com) symbols, subset | `files-app/fonts/eza-icons.woff2` | MIT / SIL OFL 1.1 |

## Lucide — ISC License

Copyright (c) 2026 Lucide Icons and Contributors
(some icons derive from the Feather project, MIT, Copyright (c) 2013-2023 Cole Bemis)

Permission to use, copy, modify, and/or distribute this software for any purpose with or without
fee is hereby granted, provided that the above copyright notice and this permission notice appear
in all copies.

THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES WITH REGARD TO THIS
SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE
AUTHOR BE LIABLE FOR ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN ACTION OF CONTRACT,
NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF OR IN CONNECTION WITH THE USE OR PERFORMANCE
OF THIS SOFTWARE.

## Font subsetting

```sh
fonttools varLib.instancer InterVariable.ttf wght=400:700 opsz=14 -o inter-inst.ttf
pyftsubset inter-inst.ttf --unicodes='U+0000-00FF,U+0131,U+0152-0153,U+02BB-02BC,U+02C6,U+02DA,U+02DC,U+2000-206F,U+2190-2199,U+2212,U+2215,U+20AC,U+2122,U+25CF,U+25B2,U+2715,U+25CC,U+2318' \
  --layout-features='kern,liga,calt,tnum,cv11,ss03,case' --flavor=woff2 --no-hinting --desubroutinize \
  --output-file=inter-latin.woff2
# JetBrains Mono: same unicodes from the wght=400 instance, layout feature kern only
```

# Rust Imaging & Font Rasterization Research Report (`mibot-lite`)

Date: 2026-11-27  
Target Go Packages Replaced:
- `internal/imaging` (`image`, `image/png`, `image/jpeg`, `image/draw`, `golang.org/x/image/draw`, `golang.org/x/image/webp`)
- `internal/statuscard` (`golang.org/x/image/font`, `font/opentype`, `font/sfnt`, `math/fixed`, `vector`)
- `internal/media/sticker.go` (`ResizeSticker`, `WebPSize`)

Target Constraints (`rust/DECISIONS.md` ADR-001/004/006/007, `rust/RULES.md` §3, §7):
- **Pure Rust only**: zero C/C++ dependencies (`libwebp-sys`, `freetype-sys`, `libpng-sys`, `zlib-ng-sys`, `vips`), supporting `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`, `aarch64-apple-darwin`.
- **`MemoryDenyWriteExecute=yes` compatible**: no runtime JIT / RWX pages.
- **No unchecked panics** on fallible decoding/encoding/font paths.
- **Behavioral equivalence** (ADR-004): exact layout/dimensions/bounds/alpha invariants; minor anti-aliasing pixel differences permitted within threshold.

---

## 1. Recommended Crate Stack & Exact Versions

Verified against crates.io and docs.rs:

| Crate | Latest Version | Feature Flags | Role in `mibot-lite` | Pure Rust? |
| :--- | :--- | :--- | :--- | :--- |
| **`image`** | `0.25.10` | `default-features = false, features = ["png", "jpeg", "webp"]` | PNG/JPEG/WebP decoding, header-only dimension limits (`ImageReader::into_dimensions` + `Limits`), streaming `PngEncoder` | Yes (`png` + `miniz_oxide`/`fdeflate`, `zune-jpeg`, `image-webp`) |
| **`fast_image_resize`** | `6.2.0` | `features = ["image"]` (default includes `std`) | High-quality `FilterType::CatmullRom` resizing (`Resize`, `ResizeCover`, `ResizeSticker`) with alpha pre-multiplication | Yes (safe Rust + `std::arch` SIMD intrinsics, no JIT) |
| **`tiny-skia`** | `0.12.0` | `default-features = false, features = ["std", "simd", "png-format"]` | Vector shapes, linear gradient, alpha compositing, `DestinationIn` / `Mask` masking, affine `Transform` rotation (`FilterQuality::Bicubic`) | Yes (pure Rust Skia CPU rasterizer + `png` optional) |
| **`ab_glyph`** | `0.2.32` | default | TTF/OTF parsing (`owned_ttf_parser`), glyph coverage checks (`glyph_id(c) != GlyphId(0)`), metrics, and glyph outline rasterization (`ab_glyph_rasterizer`) | Yes |

> **Alternative considered for fonts**: `fontdue` (`0.9.5`) — also pure Rust (`no_std + alloc`, `lookup_glyph_index(c) == 0`), faster at batch text layout, and takes `ppem` directly (like Go `opentype.FaceOptions{Size, DPI: 72}`). However, `ab_glyph` (`0.2.32`) is already specified in `rust/DECISIONS.md` ADR-006, supports both glyf and CFF outlines via `owned_ttf_parser`, and maps cleanly once its `PxScale` height convention is accounted for (see §5.2).

### Recommended `Cargo.toml` Dependency Block

```toml
[dependencies]
image = { version = "0.25.10", default-features = false, features = ["png", "jpeg", "webp"] }
fast_image_resize = { version = "6.2.0", features = ["image"] }
tiny-skia = { version = "0.12.0", default-features = false, features = ["std", "simd", "png-format"] }
ab_glyph = "0.2.32"
```

*(Note: `png` crate is shared transitively by both `image` and `tiny-skia`; if direct control over `png::Compression` / `DeflateCompression::Fast` is desired beyond `image::codecs::png::CompressionType::Fast`, `png` can also be used directly or via `image::codecs::png::PngEncoder::new_with_quality`.)*

---

## 2. Image Decoding, Header Limits & Streaming PNG Encoding

### 2.1 Go Baseline (`internal/imaging/imaging.go`)
- `MaxDimension = 2048` (`imaging.go:30`).
- `DecodePNG(data)` (`imaging.go:33-42`): calls `png.DecodeConfig` first; rejects `< 1` or `> 2048` on either axis *before* allocating pixel buffers (`TestDecodeRejectsOversize` in `imaging_test.go:43-54`).
- `Decode(data)` (`imaging.go:46-56`): sniffs PNG/JPEG/WebP via `image.DecodeConfig`, enforces `1..=2048`, then decodes.
- `EncodePNG(img)` (`imaging.go:59-66`) & `WritePNG(w, img)` (`imaging.go:70-72`): encodes PNG into `Vec<u8>` or `impl io::Write` (used per-frame in `internal/commands/eatgif/eatgif.go:410` to avoid holding all encoded frames in memory).
- `statuscard.Render` (`card.go:152`): uses `png.BestSpeed` rather than `png.DefaultCompression` for the 1600×900 status card.
- `WebPSize(data)` (`imaging.go:223-250`): parses RIFF `VP8X`, `VP8 `, `VP8L` headers without full decode. Can be kept as a direct ~25-line slice parser in Rust **or** backed by `ImageReader::with_guessed_format()` + `into_dimensions()`. Keeping the explicit 25-line RIFF parser (or matching `ImageFormat::WebP` + `into_dimensions`) guarantees exact parity with `TestWebPSize` (`imaging_test.go:145-173`), especially for synthetic 30-byte test fixtures!

### 2.2 Rust Replacement (`image` `0.25.10`)
- **Pure-Rust Feature Flags**:
  - `"png"` → `png` crate (uses `miniz_oxide` + `fdeflate` + `flate2` with pure-Rust `miniz_oxide` backend).
  - `"jpeg"` → `zune-core` + `zune-jpeg` (pure Rust, no `libjpeg-turbo`).
  - `"webp"` → `image-webp` (100% safe pure Rust; supports lossy VP8, lossless VP8L, alpha, extended VP8X, animation).
  - **CRITICAL**: Do **NOT** add the standalone `webp` crate (`crates.io/crates/webp`), which wraps `libwebp-sys` (C code). Keep `default-features = false` on `image` so unwanted decoders (like `avif`/`dav1d` or `tiff`) are not pulled in.
- **Header-Only Dimension Check (`MaxDimension = 2048`)**:
  ```rust
  use std::io::Cursor;
  use image::{ImageFormat, ImageReader, Limits};

  let mut limits = Limits::default();
  limits.max_image_width = Some(2048);
  limits.max_image_height = Some(2048);
  // Optional: also cap max_alloc (e.g., 2048 * 2048 * 4 * 4 = 64 MiB)

  let mut reader = ImageReader::new(Cursor::new(data)).with_guessed_format()?;
  // For DecodePNG, explicitly assert format == Some(ImageFormat::Png)
  // For Decode, allow only Some(ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP)
  reader.set_format(format); // or keep guessed after checking reader.format()
  reader.limits(limits);
  let dynamic = reader.decode()?; // enforces limits before full pixel allocation
  ```
  *(Note: calling `into_dimensions()` consumes `ImageReader`, so if you want a two-step check, read dimensions from a fresh `ImageReader` or rely on `reader.limits(limits)` + explicit `width >= 1 && height >= 1 && width <= 2048 && height <= 2048` validation using `into_dimensions()` first and a second `ImageReader` for `decode()`, matching Go's `DecodeConfig` + `Decode` pattern.)*
- **Streaming PNG Encoding**:
  ```rust
  use image::codecs::png::{CompressionType, FilterType, PngEncoder};
  use image::{ExtendedColorType, ImageEncoder};

  let encoder = PngEncoder::new_with_quality(
      writer, // any impl std::io::Write (e.g., BufWriter<File> or &mut Vec<u8>)
      CompressionType::Default, // or CompressionType::Fast for statuscard
      FilterType::Adaptive,
  );
  encoder.write_image(rgba.as_raw(), rgba.width(), rgba.height(), ExtendedColorType::Rgba8)?;
  ```

### 2.3 Semantic Traps (Decoding & Encoding)
1. **Format Allowlist**: Go's `imaging.Decode` only imports `image/png`, `image/jpeg`, `golang.org/x/image/webp`. Because we configure `image` with `default-features = false, features = ["png", "jpeg", "webp"]`, only those 3 formats are compiled in, matching Go's format surface. Still explicitly verify `reader.format()` is `Png`/`Jpeg`/`WebP` (and `Png` only for `decode_png`).
2. **Zero-Dimension Rejection**: `Limits::max_image_width` only checks the upper bound; explicitly check `w >= 1 && h >= 1`.
3. **WebP Header-Only Fixture (`TestWebPSize`)**: `TestWebPSize` (`imaging_test.go:145-173`) constructs minimal 30-byte `RIFF....WEBPVP8 ` / `WEBPVP8X` buffers without valid bitstream payload. `image-webp` may reject truncated RIFF chunk sizes; keeping the 25-line `webp_size(data: &[u8])` header parser from `imaging.go:223-250` is cleaner, zero-allocation, and 100% compatible.
4. **Premultiplied vs Straight RGBA on PNG Encode**: `image::RgbaImage` is **straight (unpremultiplied)** RGBA, which `PngEncoder` expects directly. `tiny_skia::Pixmap` is **premultiplied** RGBA: `Pixmap::encode_png()` automatically unpremultiplies it, whereas passing `pixmap.data()` to `image::PngEncoder` without unpremultiplying will darken semi-transparent edges!

---

## 3. Resizing (`CatmullRom`) & Affine Rotation Comparison

### 3.1 Resizing: `Resize`, `ResizeCover`, `ResizeSticker`
- **Go Baseline**: `xdraw.CatmullRom.Scale(dst, dst.Bounds(), src, crop, draw.Src, nil)` (`imaging.go:75-95`).
- **Recommended Crate**: `fast_image_resize` `6.2.0` (`features = ["image"]`).
  - Has exact `FilterType::CatmullRom` (`ResizeAlg::Convolution(FilterType::CatmullRom)`).
  - Supports source sub-rectangle cropping via `ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::CatmullRom)).crop(left, top, width, height)`.
  - **`use_alpha(true)` (default)** automatically handles alpha premultiplication (`MulDiv`) when resizing straight RGBA images, preventing black fringes around transparent PNG/WebP sticker edges. (Note: if resizing a *premultiplied* buffer, set `.use_alpha(false)`!).
  - **`image` crate itself** also has `imageops::resize(..., FilterType::CatmullRom)`, which works, **but** `image::imageops::resize` operates on straight RGBA without premultiplication (`MulDiv`), causing dark halos on semi-transparent edges unless premultiplied manually — AND `image::imageops::crop_imm` + `resize` allocates an extra cropped buffer, whereas `fast_image_resize` crops in-place via `ResizeOptions::crop` and runs SIMD-accelerated (`SSE4.1`/`AVX2`/`NEON`).
  - **Tiny-dimension edge case**: For `ResizeCover` (`imaging.go:84-94`), replicate exact rounding & clamping:
    ```rust
    let scale = (width as f64 / src_w as f64).max(height as f64 / src_h as f64);
    let crop_w = ((width as f64 / scale).round() as u32).clamp(1, src_w);
    let crop_h = ((height as f64 / scale).round() as u32).clamp(1, src_h);
    let crop_x = (src_w - crop_w) / 2;
    let crop_y = (src_h - crop_h) / 2;
    ```

### 3.2 Rotation Comparison: `tiny-skia` vs `imageproc` vs `image`

| Operation | `tiny-skia` `0.12.0` | `imageproc` `0.27.0` | `image` `0.25.10` |
| :--- | :--- | :--- | :--- |
| **Arbitrary-angle rotation** | `PixmapMut::draw_pixmap` + `Transform::from_rotate_at` | `rotate_about_center` | Only 90°/180°/270° (`rotate90`, etc.) |
| **Interpolation quality** | `FilterQuality::Nearest`, `Bilinear`, **`Bicubic`** | `Interpolation::Nearest`, `Bilinear` (no `Bicubic` in `imageproc::geometric_transformations::Interpolation`!) | N/A |
| **Alpha handling** | Native premultiplied RGBA (matches Go `*image.RGBA` and avoids dark fringes on rotated edges!) | Straight RGBA with `Border::Zero` (risks dark edge interpolation unless premultiplied) | N/A |
| **Extra dependencies** | Already needed for `statuscard` & masking | Adds `imageproc` + `nalgebra`/`num-traits`/etc. | N/A |

**Verdict**: Use **`tiny-skia`** (`0.12.0`) for both `Rotate` and `RotateExpand` (as well as `ApplyMask`, `Composite`, `Flatten`, and `statuscard`), avoiding `imageproc` entirely!

### 3.3 Implementing `Rotate` & `RotateExpand` in `tiny-skia`
- **`Rotate(source, degrees)`** (fixed canvas, `imaging.go:99-113`):
  - Canvas size remains `(w, h)`.
  - `let cx = w as f32 / 2.0; let cy = h as f32 / 2.0;`
  - `let transform = Transform::from_rotate_at(degrees as f32, cx, cy);`
  - `let paint = PixmapPaint { quality: FilterQuality::Bicubic, ..PixmapPaint::default() };`
  - `target.draw_pixmap(0, 0, src.as_ref(), &paint, transform, None);`
- **`RotateExpand(source, degrees)`** (expanded transparent canvas, `imaging.go:117-127`):
  - Compute expanded integer dimensions identically to Go:
    ```rust
    let rad = degrees.to_radians();
    let (sin, cos) = (rad.sin().abs(), rad.cos().abs());
    let new_w = ((src_w as f64 * cos + src_h as f64 * sin).ceil() as u32).max(1);
    let new_h = ((src_w as f64 * sin + src_h as f64 * cos).ceil() as u32).max(1);
    ```
  - Create zero-initialized `Pixmap::new(new_w, new_h)`.
  - Draw `src` centered and rotated:
    ```rust
    let transform = Transform::from_translate(new_w as f32 / 2.0, new_h as f32 / 2.0)
        .pre_rotate(degrees as f32)
        .pre_translate(-(src_w as f32 / 2.0), -(src_h as f32 / 2.0));
    target.draw_pixmap(0, 0, src.as_ref(), &paint, transform, None);
    ```
  - **Bonus vs Go**: Go's `RotateExpand` copies `source` to `padded` at integer `offset` (`(new_w - src_w) / 2`) and then rotates `padded`; doing the centered `Transform` directly in `tiny-skia` eliminates the intermediate `padded` allocation and handles odd-dimension centering cleanly without clipping corners.

### 3.4 Semantic Traps (Rotation & Color Space)
1. **Rotation Sign / Direction**: In Go's Y-down screen coordinates (`imaging.go:107-110`):
   $$x' = x\cos\theta - y\sin\theta, \quad y' = x\sin\theta + y\cos\theta$$
   Positive `degrees` rotates **clockwise** on screen. Verify `tiny-skia::Transform::from_rotate_at(deg, cx, cy)` has the same clockwise convention in Y-down coordinates via a unit test with an asymmetric quadrant image (e.g., red top-left pixel rotated +90° lands in top-right).
2. **`draw_pixmap(x, y, ...)` + `Transform`**: `tiny-skia::PixmapMut::draw_pixmap` treats `(x, y)` as pattern offset; pass `(0, 0)` whenever using a non-identity `Transform` (like `from_translate` + `pre_rotate`) so translation order is unambiguous.
3. **Premultiplied vs Straight Pixel Format Boundary**:
   - Standardize the Rust `imaging` buffer type! Either:
     - Store **`tiny_skia::Pixmap`** (premultiplied RGBA) internally in `imaging` — making `Opacity`, `Brightness`, `ApplyMask`, `Composite`, `Flatten`, `Rotate`, `RotateExpand` zero-conversion, and converting to/from `image::RgbaImage` (straight) only at `Decode` / `Resize` (`fast_image_resize` with `.use_alpha(false)` if already premultiplied, or premultiply/unpremultiply at boundary) / `EncodePNG`.
     - **WAIT — check `fast_image_resize` with premultiplied RGBA**: If `imaging` stores **premultiplied** RGBA (`tiny_skia::Pixmap`), `fast_image_resize` can resize raw `U8x4` bytes **with `.use_alpha(false)`** directly without any premul/unpremul round-trip, because Catmull-Rom linear convolution on premultiplied RGBA is mathematically valid and matches Go's `xdraw.CatmullRom` on `*image.RGBA`! Just remember to clamp `R, G, B <= A` (or note Catmull-Rom negative lobes can slightly overshoot `A`, which `MulDiv` or explicit clamp handles — actually Go's `xdraw.CatmullRom` clamps channels to `[0, 255]` / `[0, A]` when writing `*image.RGBA`!). Let's verify: clamping `r = r.min(a); g = g.min(a); b = b.min(a)` after `fast_image_resize` on premultiplied `U8x4` preserves the premultiplied invariant (`TestBrightnessClampsToAlpha`).
     - And for `EncodePNG` / `WritePNG`, unpremultiply before passing to `image::codecs::png::PngEncoder` (or use `Pixmap::encode_png()` for buffered encoding).

---

## 4. Pixel Ops & Alpha Mask Compositing (`imaging.go`)

### 4.1 `Opacity` & `Brightness` (`imaging.go:129-159`)
- **`Opacity(source, factor)`**:
  - `factor = factor.clamp(0.0, 1.0)`.
  - On **premultiplied** RGBA (`Pixmap` bytes): multiply every byte `[R, G, B, A]` by `factor` and `.round() as u8`. (Matches `imaging_test.go:192-202`: `(200, 0, 0, 255) * 0.5 -> r=100, a=128`).
  - Trap: If operating on *straight* RGBA, multiplying `R, G, B` by `factor` would darken RGB twice when later premultiplied! On premultiplied `Pixmap`, multiplying all 4 channels is exact.
- **`Brightness(source, factor)`**:
  - If `factor == 1.0`, clone.
  - On **premultiplied** RGBA: multiply `R, G, B` by `factor`, clamp each to `[0.0, A as f64]` (`value.clamp(0.0, alpha) as u8`), leaving `A` untouched. Matches `TestBrightnessClampsToAlpha` (`imaging_test.go:105-121`).

### 4.2 `ApplyMask` (`imaging.go:161-182`)
- **Go Behavior**:
  ```go
  bounds := mask.Bounds()
  target := image.NewRGBA(image.Rect(0, 0, bounds.Dx(), bounds.Dy()))
  draw.DrawMask(target, target.Bounds(), source, source.Bounds().Min, alphaOf(mask), bounds.Min, draw.Over)
  ```
  Key semantics (`eatgif.go:237`, `imaging_test.go:87-102`):
  1. **Output dimensions come from `mask`**, NOT `source` (if `source` and `mask` differ in size, pixels outside `source` stay transparent `(0,0,0,0)`).
  2. **Only `mask`'s alpha channel matters**; `mask`'s RGB values are ignored (`alphaOf`).
  3. Result pixel is `source_premul * (mask_a / 255.0)`.
- **Rust (`tiny-skia`) Mapping**:
  - Create `let mut target = Pixmap::new(mask.width(), mask.height())?;`
  - Construct `Mask` from `mask`:
    `let alpha_mask = tiny_skia::Mask::from_pixmap(mask.as_ref(), tiny_skia::MaskType::Alpha);`
  - Draw `source` onto `target` with `alpha_mask`:
    `target.draw_pixmap(0, 0, source.as_ref(), &PixmapPaint::default(), Transform::identity(), Some(&alpha_mask));`
  - Alternatively, draw `source` onto `target` with `BlendMode::Source`, then draw `mask` with `BlendMode::DestinationIn` — `Mask::from_pixmap(..., MaskType::Alpha)` is even cleaner because it explicitly isolates `mask`'s alpha channel and handles any size difference cleanly.

### 4.3 `Composite` & `Flatten` (`imaging.go:184-209`)
- **`Composite(canvas, overlay, x, y)`**:
  - `canvas.draw_pixmap(x, y, overlay.as_ref(), &PixmapPaint::default(), Transform::identity(), None);` (`PixmapPaint::default()` uses `BlendMode::SourceOver`, which is Porter-Duff `Over`).
  - Verify negative `(x, y)` clipping: `tiny-skia::PixmapMut::draw_pixmap` takes `x: i32, y: i32` and clips to destination bounds safely without panicking.
- **`Flatten(source, background)`**:
  - Create `Pixmap::new(w, h)`, call `target.fill(bg_color)`, then `target.draw_pixmap(0, 0, source.as_ref(), &PixmapPaint::default(), Transform::identity(), None)`.

---

## 5. `statuscard`: Vector Shapes + TTF Subset Font Rasterization

### 5.1 Vector Shapes & Gradients (`tiny-skia` `0.12.0`)
Inspecting `internal/statuscard/card.go:112-370`, every primitive maps directly and more cleanly to `tiny-skia`:
1. **Background 3-stop linear gradient** (`card.go:190-209`):
   - Go uses `t = (x*W + y*H) / (W^2 + H^2)` with stops at `0.0` (`#071927`), `0.58` (`#071520`), `1.0` (`#05111a`).
   - Geometrically, `(x*W + y*H) / (W^2 + H^2)` is the exact orthogonal projection onto the vector from `(0, 0)` to `(1600, 900)`!
   - Maps 1:1 to `tiny_skia::LinearGradient::new(Point::from_xy(0.0, 0.0), Point::from_xy(1600.0, 900.0), stops, SpreadMode::Pad, Transform::identity())` filled over `Rect::from_xywh(0.0, 0.0, 1600.0, 900.0)`.
2. **Grid lines & dividers** (`card.go:211-219`, `126`, `147`, `362`):
   - `PathBuilder::move_to` / `line_to` + `pixmap.stroke_path(..., &Stroke { width, .. }, ...)` (with `paint.anti_alias = true`).
3. **Rounded rectangles (filled & stroked)** (`card.go:122`, `315-332`, `345-346`):
   - Go manually builds polygon vertices; `tiny-skia` `PathBuilder` supports `cubic_to` (or `PathBuilder` rounded-rect helper with kappa `0.5522847498 * r`) and native `stroke_path` with `Stroke { width, .. }`.
4. **Circles & Glow layers** (`card.go:130-134`, `277`, `339`):
   - `PathBuilder::from_circle(cx, cy, r)` + `fill_path(..., FillRule::Winding, ...)`.
5. **Arcs & Round Caps** (`card.go:288-299`, `335-356`):
   - Go manually computes outer/inner polygon + end circles for `round = true`.
   - `tiny-skia` `stroke_path` natively supports `Stroke { width, line_cap: LineCap::Round, .. }` (or `LineCap::Butt` for the full background track `round = false` at `card.go:348`). Using cubic Bezier arc approximation (`<= 90°` segments) with `Stroke` gives cleaner anti-aliased caps and much less code!
6. **Color Alpha Handling**:
   - Note Go's `withAlpha(c, alpha)` (`card.go:163-165`) manually premultiplies `RGBA`.
   - In `tiny-skia`, `Color::from_rgba8(r, g, b, (255.0 * alpha) as u8)` takes **straight** RGBA and `Paint::set_color` premultiplies internally when rendering — do **not** double-premultiply!

### 5.2 Font Rasterization & `NotoSansSC-status-subset.ttf` (`ab_glyph` `0.2.32`)

Live inspection of `/home/ezhang/mibot-lite/internal/statuscard/NotoSansSC-status-subset.ttf` confirmed:
- Size: `73,568` bytes (`include_bytes!("NotoSansSC-status-subset.ttf")`).
- SFNT tag: `0x00010000` (TrueType `glyf`/`loca` outlines, NOT CFF).
- Tables: `GPOS`, `GSUB`, `OS/2`, `STAT`, `cmap`, `gasp`, `glyf`, `head`, `hhea`, `hmtx`, `loca`, `maxp`, `name`, `post`, `prep`, `vhea`, `vmtx`.
- Key metrics: `unitsPerEm = 1000`, `hhea.ascender = 1160`, `hhea.descender = -288`, `hhea.lineGap = 0` (`height_unscaled = 1448`).

#### Glyph Coverage Check (`TestFontCoversCardText` in `font_test.go:11-25`)
- Go checks: `index, err := face.GlyphIndex(&buffer, r); err != nil || index == 0`.
- In `ab_glyph`:
  ```rust
  use ab_glyph::{Font, FontRef, GlyphId};

  let font = FontRef::try_from_slice(FONT_DATA)?;
  for ch in text.chars() {
      if font.glyph_id(ch) == GlyphId(0) {
          panic!("missing glyph '{}' (U+{:04X}) in {}", ch, ch as u32, text);
      }
  }
  ```
  - `GlyphId(0)` is guaranteed by the OpenType spec and `ab_glyph` docs to be the `.notdef` (missing) glyph.
  - Note: unlike `fontdue` (where `lookup_glyph_index` returns `u16` and `0` means missing), `ab_glyph::Font::glyph_id(char)` returns `GlyphId(0)` directly.

#### CRITICAL SEMANTIC TRAPS for Font Rendering (`ab_glyph` vs Go `opentype`)

1. **Trap 1 — `PxScale` vs `FaceOptions{Size, DPI: 72}` (31% / 33% size mismatch if wrong!)**:
   - In Go (`card.go:376`), `opentype.NewFace(c.font, &opentype.FaceOptions{Size: size, DPI: 72, Hinting: font.HintingNone})` sets **pixels-per-em** (`ppem = size * 72 / 72 = size`).
   - In `ab_glyph`, `PxScale` is **total text height in pixels** (`ascent - descent`), NOT `ppem`! `ab_glyph`'s `pt_to_px_scale(pt)` also assumes **96 DPI** (`96.0 / 72.0`), not Go's `72` DPI!
   - For `NotoSansSC-status-subset.ttf` (`units_per_em = 1000`, `height_unscaled = 1448`):
     - `PxScale::from(size)` would render **~31% too small** (`1000 / 1448`).
     - `font.pt_to_px_scale(size)` would render **33.3% too large** (`96 / 72`).
   - **Exact conversion formula** matching Go `Size` at `DPI: 72`:
     ```rust
     let units_per_em = font.units_per_em().ok_or(...)?;
     let px_scale = PxScale::from(size * (font.height_unscaled() / units_per_em));
     ```
     Verify: `scaled = font.as_scaled(px_scale)` will then have `scaled.ascent() == size * 1.160` and `scaled.descent() == size * -0.288`, matching Go's `face.Metrics()`!

2. **Trap 2 — `descent` Sign Convention in `baseline` Calculation**:
   - Go `font.Metrics.Descent` (`card.go:407`) is **positive** downward (`+0.288 * size`), so Go computes:
     `baseline = y + (metrics.Ascent - metrics.Descent) / 2` (which equals `y + (1.160 - 0.288) * size / 2`).
   - In `ab_glyph`, `ScaleFont::descent()` is **negative** (`-0.288 * size`)!
   - Therefore, to match Go's `Ascent - Descent`, Rust must compute:
     `let baseline = y + (scaled.ascent() + scaled.descent()) / 2.0;`
     *(If you wrote `ascent - descent`, you would add `|descent|` instead of subtracting it, shifting all text downward!)*

3. **Trap 3 — Manual Kerning & `measure_string` / `fitted`**:
   - Go's `font.MeasureString(face, s)` and `font.Drawer.DrawString(s)` automatically apply pairwise kerning (`face.Kern(prev, r)`).
   - `ab_glyph` does **not** automatically lay out strings; when iterating `text.chars()`, track `prev: Option<GlyphId>` and advance `pen_x += scaled.kern(prev, gid)` before positioning each glyph, then `pen_x += scaled.h_advance(gid)`.
   - Match Go's `fitted` (`card.go:386-398`): step `size` down by `2.0` from `maximum` to `minimum` using `measure_width.round() <= max_width`, and if still too wide at `minimum`, pop `char`s from the end until `measure(prefix + "…").round() <= max_width`.

4. **Trap 4 — `OutlinedGlyph` Pixel Bounds & Alpha Compositing onto `tiny-skia::Pixmap`**:
   - `outlined.px_bounds()` returns a `Rect { min: Point { x, y }, max: ... }` in **canvas pixel coordinates** (when `glyph.position = point(pen_x, baseline)`).
   - `outlined.draw(|gx, gy, coverage| ...)` passes **`gx, gy` relative to `(px_bounds().min.x, px_bounds().min.y)`** (starting at `0, 0`), NOT absolute canvas coordinates!
   - Canvas coordinate is `(px_bounds().min.x as i32 + gx as i32, px_bounds().min.y as i32 + gy as i32)` — clamp/skip if outside `0..1600` × `0..900`.
   - When compositing `coverage: f32` with text color `(r, g, b)` (`a = 255`) onto premultiplied `Pixmap` (`SourceOver`):
     - Compute `cov = coverage.clamp(0.0, 1.0)` (equivalent to premultiplied source `(r*cov, g*cov, b*cov, 255*cov)`).
     - Blend with destination pixel `dst` using standard `SourceOver`: `out = src_premul + dst_premul * (1.0 - cov)`, or use `tiny_skia::PremultipliedColorU8::from_rgba` / integer `blend` helper.

---

## 6. Pure-Rust & `MemoryDenyWriteExecute=yes` Verification

| Requirement | Verification Result |
| :--- | :--- |
| **Zero C dependencies** | `image 0.25.10` (`default-features = false, features = ["png", "jpeg", "webp"]`) uses `png` (`miniz_oxide`/`fdeflate`/`flate2` pure-Rust backend), `zune-jpeg`, and `image-webp` — zero `*-sys` crates. `fast_image_resize 6.2.0`, `tiny-skia 0.12.0`, and `ab_glyph 0.2.32` are 100% pure Rust. |
| **Cross-compilation (`musl` / `aarch64-apple-darwin`)** | No C compiler/sysroot required by any of these 4 crates. |
| **`MemoryDenyWriteExecute=yes`** | None of `image`, `fast_image_resize`, `tiny-skia`, or `ab_glyph` allocate RWX memory or use JIT compilation. `fast_image_resize` and `tiny-skia` use static compile-time SIMD + safe runtime `cpufeatures` / `is_x86_feature_detected!` dispatch. |
| **Recommended verification in Wave 0/1** | After adding dependencies to `rust/Cargo.toml`, run `cargo tree -e normal` and verify zero `*-sys` / `cc` / `bindgen` crates appear in the dependency graph. |

---

## 7. Primary Sources

- `image` v0.25.10: [crates.io/crates/image](https://crates.io/crates/image), [docs.rs/image/latest](https://docs.rs/image/latest/image/) (`ImageReader`, `Limits`, `PngEncoder::new_with_quality`, feature flags `"png"`, `"jpeg"`, `"webp"`).
- `image-webp`: [crates.io/crates/image-webp](https://crates.io/crates/image-webp) (pure-Rust WebP decoder/lossless encoder, zero `unsafe`).
- `png`: [docs.rs/crate/png/latest/features](https://docs.rs/crate/png/latest/features) (`miniz_oxide`, `fdeflate`, `flate2`).
- `fast_image_resize` v6.2.0: [crates.io/crates/fast_image_resize](https://crates.io/crates/fast_image_resize), [docs.rs/fast_image_resize/latest](https://docs.rs/fast_image_resize/latest/fast_image_resize/) (`FilterType::CatmullRom`, `ResizeOptions::crop`, `ResizeOptions::use_alpha`, `IntoImageView` via `image` feature).
- `tiny-skia` v0.12.0: [crates.io/crates/tiny-skia](https://crates.io/crates/tiny-skia), [docs.rs/tiny-skia/latest](https://docs.rs/tiny-skia/latest/tiny_skia/) (`Pixmap`, `PixmapMut::draw_pixmap`, `FilterQuality::Bicubic`, `Transform::from_rotate_at`, `Mask::from_pixmap`, `LinearGradient`, `Stroke`, `png-format`).
- `ab_glyph` v0.2.32: [crates.io/crates/ab_glyph](https://crates.io/crates/ab_glyph), [docs.rs/ab_glyph/latest](https://docs.rs/ab_glyph/latest/ab_glyph/) (`Font::glyph_id`, `Font::pt_to_px_scale`, `PxScale`, `ScaleFont`, `OutlinedGlyph::px_bounds`, `OutlinedGlyph::draw`).
- `imageproc` v0.27.0: [crates.io/crates/imageproc](https://crates.io/crates/imageproc), [docs.rs/imageproc/0.27.0](https://docs.rs/imageproc/0.27.0/imageproc/geometric_transformations/enum.Interpolation.html) (evaluated & rejected for rotation due to lack of `Bicubic` interpolation and redundant dependency footprint).
- `fontdue` v0.9.5: [crates.io/crates/fontdue](https://crates.io/crates/fontdue), [docs.rs/fontdue/latest](https://docs.rs/fontdue/latest/fontdue/) (evaluated alternative).
- Repository references:
  - `internal/imaging/imaging.go`, `internal/imaging/imaging_test.go`, `internal/media/sticker.go`
  - `internal/statuscard/card.go`, `internal/statuscard/font_test.go`, `internal/statuscard/card_test.go`, `internal/statuscard/NotoSansSC-status-subset.ttf`
  - `rust/DECISIONS.md`, `rust/RULES.md`

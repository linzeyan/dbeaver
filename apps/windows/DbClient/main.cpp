// The Windows front end, from the bottom.
//
// The plan is WinUI 3 with a Direct2D grid, and of those two the XAML shell is
// the part that is well understood and the part whose problems are somebody
// else's build system. The grid is Direct2D and DirectWrite, it is where every
// hard question lives — text metrics, DPI, how much can be drawn in a frame —
// and none of it needed an HWND to be wrong. So the rendering stack was asked
// first, headless, and the window came second.
//
// Run with no arguments it opens that window. `--verify-drivers` and
// `--verify-grid` run the checks instead, inside the real binary behind a flag,
// the same arrangement the macOS app uses rather than a test target that would
// have to reproduce this link. They draw into a WIC bitmap instead of onto a
// window, so they run on a machine with no display and no GPU, which is what a
// CI runner is — and they draw it by calling `draw_grid`, which is also what the
// window calls. That sharing is the only reason the checks still mean anything
// now that there is a window: the parts CI cannot reach are the HWND and the
// message loop, and everything the user actually looks at is on both paths.
//
// Built with `cl` directly, like `apps/windows/ffi-check` next door. No package
// manager and no project file, because everything here ships with the Windows
// SDK, and a first brick that needs a NuGet restore to say whether Direct2D
// works is a brick that answers two questions and tells you neither.

// Every Win32 call here is spelled with its `W` suffix, and this says so once so
// the macros agree. Without it `IDC_ARROW` and its neighbours expand to their
// ANSI form and are rejected by the `W` function they are passed to — a type
// error about `LPSTR` in a file that never mentions one.
#define UNICODE
#define _UNICODE
#define WIN32_LEAN_AND_MEAN
#include <windows.h>

// For GET_X_LPARAM. A click's coordinates are two signed shorts packed into one
// LPARAM, and pulling them out with LOWORD gives the wrong answer for a negative
// one — which a drag off the left edge produces.
#include <windowsx.h>

#include <d2d1.h>
#include <dwmapi.h>
#include <dwrite.h>
#include <wincodec.h>
#include <wrl/client.h>

#include <cmath>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <cwchar>
#include <string>
#include <vector>

#include "dbffi.h"

using Microsoft::WRL::ComPtr;

namespace {

// The bitmap the checks draw into. Big enough to hold what they draw with room
// left over — a layout that overflowed would otherwise be indistinguishable
// from one that fitted exactly.
constexpr UINT kWidth = 640;
constexpr UINT kHeight = 640;

// The macOS grid's numbers, not new ones. Two front ends that disagree about
// row height disagree about how much of a table a screen holds, and that is a
// difference a user would notice long before anybody found it written down.
// `GridRenderer.swift` is where these come from and where they belong.
constexpr float kRowHeight = 20.0f;
constexpr float kHeaderHeight = 32.0f;
constexpr float kCellPadding = 6.0f;
constexpr float kMinColumnWidth = 56.0f;
constexpr float kMaxColumnWidth = 340.0f;
// DirectWrite takes a size in DIPs, not points, and this is deliberately the
// same number the macOS grid uses for points: a DIP is a ninety-sixth of an inch
// and an AppKit point is a seventy-second, but both are one pixel at unit scale,
// which is the scale each side's layout is written in. Naming it `kFontSize`
// rather than `kPointSize` is the whole of the protection against reading that
// coincidence as an identity.
constexpr float kFontSize = 12.0f;

// Where text sits inside its band, transcribed rather than computed. Centring
// the line inside the row by the face's own metrics put this side a point lower
// than the other one — a disagreement about what a row looks like, arrived at
// for no better reason than that it was the easier thing to work out.
//
// Two lines in the header, which is what the band is 32 tall for: the column's
// name, and under it the type its values were declared with. A type appended to
// the name would compete for width in a column already sized to its own content,
// and the loser would be the name — the thing the grid is navigated by.
constexpr float kHeaderNameY = 2.0f;
constexpr float kHeaderTypeY = 15.0f;
constexpr float kCellTextY = 3.0f;

// How much of a say the type gets in how wide its column is. It has to be
// recognisable, not complete: a declared type is routinely longer than every
// value under it, and letting it size the column would push real data off the
// screen to spell out a label.
constexpr size_t kMaxTypeChars = 13;

// The scrollbar's gutter is the target, not the paint. Twelve DIPs to hit and
// five to see: a thumb drawn at the width it can be grabbed at would be a bar
// down the side of the result rather than a mark on it, and one grabbable only
// where it is painted would be a five-DIP target.
//
// The floor on the thumb's length is the trade every platform makes. Sized
// purely by proportion it disappears on a million rows, and at that point it has
// stopped reporting how much there is and become something to take hold of.
// How near a column boundary a press counts as aiming at it. `AppController`'s
// number, and it is per side: the handle is eight DIPs wide across a line one
// DIP thick.
constexpr float kEdgeTolerance = 4.0f;

constexpr float kScrollbarGutter = 12.0f;
constexpr float kScrollbarThumb = 5.0f;
constexpr float kMinThumbLength = 28.0f;

// Rows a wheel notch moves. `AppController.swift`'s multiplier, and it is the
// distance sideways too: one row would be an accurate wheel and a useless one,
// since a result is read in pages and a notch that moved a single line would
// need forty of them to cross a screen.
constexpr float kWheelRows = 3.0f;

// `Theme.swift`'s values, under the names it gives them, resolved once per
// appearance. Two tables rather than two ways of drawing: over there every one
// of these is the same token read against `isLight`, and the whole of dark mode
// is which column of numbers the same code reads.
//
// `rule` is the ramp's direction — black over a light surface, white over a
// dark one — and it is one colour at several alphas rather than several
// colours, which is what keeps banding, the separators and the scrollbar
// correct over whatever they land on. The alphas are not the same in both
// columns: black at 0.022 over white is a step nobody can see, so the light
// side is a shade stronger than its dark counterpart rather than the same
// number.
struct Tones {
    UINT32 canvas;       // Grid.background, Surface.canvas
    UINT32 header_band;  // Grid.header, Surface.raised
    UINT32 ink;          // Grid.text
    UINT32 header_ink;   // Grid.headerText, Text.secondary
    // Grid.sortedHeaderText, Text.primary. The ordered column's name is darker
    // than the others as well as marked, so which way the result is sorted is
    // legible without resolving a six-point triangle.
    UINT32 sorted_header_ink;
    UINT32 muted_ink;  // Grid.nullText, Text.dataMuted
    UINT32 rule;
    float banding_alpha;
    float separator_alpha;
    // Accent.selection, at the two strengths Grid.selectedRow and
    // Grid.selectedCell use, and undiluted for Grid.cursor. Translucent so the
    // value underneath stays readable — which is also why the text is drawn
    // after them. The strengths do not change with the appearance; the accent
    // itself lightens, because a hue that reads over white is not the one that
    // reads over near-black.
    UINT32 accent;
    // Grid.pendingCell: Semantic.warning, which is the one tone in this table
    // that reports something about the data rather than about the chrome. A
    // cell somebody has typed into is neither an error nor a normal value, and
    // it has to stay legible under the selection it is usually sitting in.
    UINT32 pending;
    // Grid.scrollTrack, Grid.scrollThumb and Grid.scrollThumbActive: `rule`
    // again, at three strengths. The bar sits over the data rather than beside
    // it, so the track is barely there and the thumb carries the whole signal.
    float scroll_track_alpha;
    float scroll_thumb_alpha;
    float scroll_thumb_active_alpha;
};

constexpr Tones kLightTones = {
    0xFFFFFF, 0xF1F5F9, 0x1E293B, 0x475569, 0x0F172A, 0x51607A, 0x0F172A, 0.030f, 0.080f,
    0x4F46E5, 0xB45309, 0.040f,   0.180f,   0.320f,
};
constexpr Tones kDarkTones = {
    0x0F172A, 0x1E293B, 0xE2E8F0, 0x94A3B8, 0xF8FAFC, 0x7C8AA0, 0xFFFFFF, 0.022f, 0.060f,
    0x6366F1, 0xFBBF24, 0.035f,   0.220f,   0.380f,
};

// What a typed cell's mark is worth. Strong enough to find at a glance and
// weak enough that the value under it is still the thing being read — the
// mark says a cell has been changed, not what it was changed to.
constexpr float kPendingAlpha = 0.300f;

constexpr float kSelectedRowAlpha = 0.180f;
constexpr float kSelectedCellAlpha = 0.380f;

// A tone the way GDI wants one. The table above is written the way colours are
// written everywhere else — 0xRRGGBB, the order they appear in `Theme.swift` and
// in every stylesheet — and a `COLORREF` is the reverse. Only the box opened over
// a cell needs this, because it is the one thing here that Windows draws rather
// than Direct2D, and it asks in this order.
COLORREF colorref(UINT32 rgb) {
    return RGB((rgb >> 16) & 0xFF, (rgb >> 8) & 0xFF, rgb & 0xFF);
}

// How far from the canvas a channel has to move to count as a glyph when the
// bitmap is read back. 0xFF minus the 0xB4 this was written as when the canvas
// was always white, so light-mode readings are unchanged. See `Surface::ink_in`.
constexpr int kInkDistance = 0x4B;

int failures = 0;

void check(bool ok, const char* what) {
    std::printf("%s  %s\n", ok ? "ok  " : "FAIL", what);
    if (!ok) {
        failures += 1;
    }
}

bool failed(const char* what, HRESULT hr) {
    std::printf("FAIL  %s: hr=0x%08lx\n", what, static_cast<unsigned long>(hr));
    failures += 1;
    return false;
}

bool core_failed(const char* what, char* err) {
    std::printf("FAIL  %s: %s\n", what, err ? err : "(no message)");
    if (err != nullptr) {
        db_string_free(err);
    }
    failures += 1;
    return false;
}

std::wstring widen(const std::string& utf8) {
    if (utf8.empty()) {
        return std::wstring();
    }
    const int needed = MultiByteToWideChar(CP_UTF8, 0, utf8.data(),
                                           static_cast<int>(utf8.size()), nullptr, 0);
    std::wstring wide(static_cast<size_t>(needed), L'\0');
    MultiByteToWideChar(CP_UTF8, 0, utf8.data(), static_cast<int>(utf8.size()),
                        wide.data(), needed);
    return wide;
}

// The other way, for the one thing that goes back: a value the grid is showing,
// on its way into a statement the server will read.
std::string narrow(const std::wstring& wide) {
    if (wide.empty()) {
        return std::string();
    }
    const int needed = WideCharToMultiByte(CP_UTF8, 0, wide.data(), static_cast<int>(wide.size()),
                                           nullptr, 0, nullptr, nullptr);
    std::string utf8(static_cast<size_t>(needed), '\0');
    WideCharToMultiByte(CP_UTF8, 0, wide.data(), static_cast<int>(wide.size()), utf8.data(),
                        needed, nullptr, nullptr);
    return utf8;
}

// -------------------------------------------------------------------------
// The device, kept together because every check needs all of it
// -------------------------------------------------------------------------

struct Surface {
    ComPtr<ID2D1Factory> d2d;
    ComPtr<IDWriteFactory> dwrite;
    ComPtr<IWICImagingFactory> wic;
    ComPtr<IWICBitmap> bitmap;
    ComPtr<ID2D1RenderTarget> target;
    // What the last drawing cleared to, which is what `ink_in` measures
    // distance from. Carried on the surface rather than passed to every read,
    // because a bitmap holds one appearance at a time and every measurement of
    // it is about that one.
    UINT32 canvas = kLightTones.canvas;

    bool open() {
        HRESULT hr = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, __uuidof(ID2D1Factory),
                                       reinterpret_cast<void**>(d2d.GetAddressOf()));
        if (FAILED(hr)) {
            return failed("D2D1CreateFactory", hr);
        }
        hr = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED, __uuidof(IDWriteFactory),
                                 reinterpret_cast<IUnknown**>(dwrite.GetAddressOf()));
        if (FAILED(hr)) {
            return failed("DWriteCreateFactory", hr);
        }
        hr = CoCreateInstance(CLSID_WICImagingFactory, nullptr, CLSCTX_INPROC_SERVER,
                              IID_PPV_ARGS(&wic));
        if (FAILED(hr)) {
            return failed("CLSID_WICImagingFactory", hr);
        }
        hr = wic->CreateBitmap(kWidth, kHeight, GUID_WICPixelFormat32bppPBGRA,
                               WICBitmapCacheOnLoad, &bitmap);
        if (FAILED(hr)) {
            return failed("IWICImagingFactory::CreateBitmap", hr);
        }
        // Software rather than whatever the machine has: a runner has no GPU,
        // and a check that quietly needed one would fail here for a reason that
        // has nothing to do with the code being checked.
        const D2D1_RENDER_TARGET_PROPERTIES properties = D2D1::RenderTargetProperties(
            D2D1_RENDER_TARGET_TYPE_SOFTWARE,
            D2D1::PixelFormat(DXGI_FORMAT_B8G8R8A8_UNORM, D2D1_ALPHA_MODE_PREMULTIPLIED));
        hr = d2d->CreateWicBitmapRenderTarget(bitmap.Get(), properties, &target);
        if (FAILED(hr)) {
            return failed("CreateWicBitmapRenderTarget", hr);
        }
        return true;
    }

    // Glyph pixels inside one rectangle.
    //
    // Every call in a draw can succeed and leave nothing readable behind — a
    // brush the same colour as the background, a layout positioned off the edge,
    // a font that resolved to nothing — and `EndDraw` reports none of it. Asked
    // of a rectangle rather than of the whole surface because "something was
    // drawn" and "something was drawn *there*" are different questions, and only
    // the second one notices a grid that painted every row on top of the first.
    //
    // Far from the canvas rather than merely different from it, which is what
    // this counted before the grid had any chrome. A banded, ruled,
    // header-banded grid differs from its background almost everywhere, so that
    // test now answers yes for a grid with no text written on it at all.
    //
    // Measured rather than assumed, by drawing exactly that: with the old
    // predicate and every glyph suppressed, five of the seven checks that read
    // the bitmap went green. The two that did not are the two that ask something
    // sharper than "is there ink here" — one compares tones, the other requires
    // a region to be *empty* — which is the shape the rest of them should grow
    // towards.
    //
    // Distance from `canvas` rather than darkness, so the question means the
    // same thing in both appearances: on a dark canvas the text is the light
    // thing, and a measure that counted dark pixels would report an unpainted
    // dark grid as painted from edge to edge. On white the two are the same
    // predicate — every case in this file lands identically either way.
    //
    // The threshold has room on both sides: the faintest thing this grid writes
    // is `Grid.nullText`, which is 133 from the canvas at its nearest channel,
    // and the boldest thing it fills is a separator over the header band, which
    // gets to 31.
    bool ink_in(float x0, float y0, float x1, float y1, UINT* out) const {
        const int want[3] = {static_cast<int>(canvas & 0xFF), static_cast<int>((canvas >> 8) & 0xFF),
                             static_cast<int>((canvas >> 16) & 0xFF)};
        UINT painted = 0;
        const bool read = each_pixel(x0, y0, x1, y1, [&painted, &want](const BYTE* px) {
            // BGRA, premultiplied over an opaque clear, so the channels are the
            // colour. A glyph is the only thing here that moves all three of
            // them this far from the surface it is written on.
            for (int c = 0; c < 3; ++c) {
                if (std::abs(static_cast<int>(px[c]) - want[c]) <= kInkDistance) {
                    return;
                }
            }
            painted += 1;
        });
        *out = painted;
        return read;
    }

    // The darkest pixel in a rectangle, as its lightest channel.
    //
    // Toward black, which is the direction that separates one text tone from
    // another on a light canvas. A dark-canvas comparison wants the other end,
    // and every caller of this is a light-appearance check.
    //
    // What separates one text tone from another. `ink_in` above answers whether
    // anything was written; this answers which brush wrote it, which is the only
    // way a check can tell a NULL drawn as a value from a NULL drawn as a NULL —
    // both put the same number of pixels in the same cell.
    bool darkest_in(float x0, float y0, float x1, float y1, BYTE* out) const {
        BYTE darkest = 0xFF;
        const bool read = each_pixel(x0, y0, x1, y1, [&darkest](const BYTE* px) {
            const BYTE lightest = px[0] > px[1] ? (px[0] > px[2] ? px[0] : px[2])
                                                : (px[1] > px[2] ? px[1] : px[2]);
            darkest = lightest < darkest ? lightest : darkest;
        });
        *out = darkest;
        return read;
    }

    // The strongest tint towards blue in a rectangle: blue minus red, at the
    // pixel where that difference is widest.
    //
    // Whether an accent-coloured mark is there, which neither of the two above
    // can answer. `ink_in` wants every channel to have left the canvas and the
    // accent's blue is 26 from white, so it counts the sort marker as nothing at
    // all; `darkest_in` sees it but cannot tell it from `Grid.headerText`, which
    // is eight away in its darkest channel. What separates them is the hue, and
    // that is what this reads. Taken as the widest difference over a region
    // rather than from one pixel because a triangle six DIPs across has few
    // pixels that are entirely its own — and picking which one to sample is the
    // trap this file fell into once already, over the cursor's edge.
    bool bluest_in(float x0, float y0, float x1, float y1, int* out) const {
        int widest = 0;
        const bool read = each_pixel(x0, y0, x1, y1, [&widest](const BYTE* px) {
            const int tint = static_cast<int>(px[0]) - static_cast<int>(px[2]);
            widest = tint > widest ? tint : widest;
        });
        *out = widest;
        return read;
    }

    // One pixel, as B, G, R — for the fills, which are flat and can be compared
    // against the tone they were asked for rather than merely against each other.
    bool pixel_at(float x, float y, BYTE* bgr) const {
        return each_pixel(x, y, x + 1.0f, y + 1.0f, [bgr](const BYTE* px) {
            bgr[0] = px[0];
            bgr[1] = px[1];
            bgr[2] = px[2];
        });
    }

  private:
    // Locking, clamping and walking, in one place because three readers now do
    // it. Getting any line of it wrong reads as a bitmap that was never drawn on,
    // which is indistinguishable from the failure these are looking for.
    template <typename Body>
    bool each_pixel(float x0, float y0, float x1, float y1, Body&& body) const {
        WICRect all{0, 0, static_cast<INT>(kWidth), static_cast<INT>(kHeight)};
        ComPtr<IWICBitmapLock> locked;
        if (FAILED(bitmap->Lock(&all, WICBitmapLockRead, &locked))) {
            return false;
        }
        UINT size = 0;
        UINT stride = 0;
        BYTE* pixels = nullptr;
        if (FAILED(locked->GetStride(&stride)) || FAILED(locked->GetDataPointer(&size, &pixels))) {
            return false;
        }

        const UINT from_x = static_cast<UINT>(x0 < 0 ? 0 : x0);
        const UINT from_y = static_cast<UINT>(y0 < 0 ? 0 : y0);
        const UINT to_x = static_cast<UINT>(x1 > kWidth ? kWidth : x1);
        const UINT to_y = static_cast<UINT>(y1 > kHeight ? kHeight : y1);

        for (UINT y = from_y; y < to_y; ++y) {
            const BYTE* row = pixels + static_cast<size_t>(y) * stride;
            for (UINT x = from_x; x < to_x; ++x) {
                body(row + static_cast<size_t>(x) * 4);
            }
        }
        return true;
    }
};

// -------------------------------------------------------------------------
// The grid's text model
// -------------------------------------------------------------------------

// A monospaced face and the one number the grid's layout is built out of.
//
// The macOS grid rasterizes ninety-five ASCII shapes once and positions them by
// a single advance; every column width and every caret position there is that
// number times a character count. This is the same model, which matters more
// than it looks: a proportional face would make "the width of eleven characters"
// depend on which eleven, and every geometry the two front ends agree on today
// would have to be recomputed per string.
struct Monospace {
    ComPtr<IDWriteTextFormat> format;
    ComPtr<IDWriteInlineObject> ellipsis;
    float advance = 0.0f;
    float line_height = 0.0f;

    // Consolas rather than Cascadia Mono, which is the better face and is not on
    // every supported Windows. A missing font resolves to a proportional
    // fallback rather than to an error, so choosing the one that is certainly
    // there is choosing not to have a layout that is subtly wrong on old
    // machines. Revisit with a real desktop to look at.
    bool open(const ComPtr<IDWriteFactory>& dwrite) {
        static const WCHAR* kFamily = L"Consolas";

        ComPtr<IDWriteFontCollection> collection;
        HRESULT hr = dwrite->GetSystemFontCollection(&collection);
        if (FAILED(hr)) {
            return failed("GetSystemFontCollection", hr);
        }
        UINT32 index = 0;
        BOOL exists = FALSE;
        hr = collection->FindFamilyName(kFamily, &index, &exists);
        if (FAILED(hr) || !exists) {
            return failed("the system has no Consolas", hr);
        }
        ComPtr<IDWriteFontFamily> family;
        hr = collection->GetFontFamily(index, &family);
        if (FAILED(hr)) {
            return failed("GetFontFamily", hr);
        }
        ComPtr<IDWriteFont> font;
        hr = family->GetFirstMatchingFont(DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_STRETCH_NORMAL,
                                          DWRITE_FONT_STYLE_NORMAL, &font);
        if (FAILED(hr)) {
            return failed("GetFirstMatchingFont", hr);
        }
        ComPtr<IDWriteFontFace> face;
        hr = font->CreateFontFace(&face);
        if (FAILED(hr)) {
            return failed("CreateFontFace", hr);
        }

        // Every printable ASCII glyph, measured in design units and compared.
        // The macOS side asserts the same thing for the same reason: a face that
        // is not monospaced makes every column position in this grid wrong, and
        // it goes wrong quietly, as text that drifts a little further right on
        // each row.
        UINT32 codepoints[95];
        for (UINT32 i = 0; i < 95; ++i) {
            codepoints[i] = 0x20 + i;
        }
        UINT16 glyphs[95] = {};
        hr = face->GetGlyphIndices(codepoints, 95, glyphs);
        if (FAILED(hr)) {
            return failed("GetGlyphIndices", hr);
        }
        DWRITE_GLYPH_METRICS metrics[95] = {};
        hr = face->GetDesignGlyphMetrics(glyphs, 95, metrics);
        if (FAILED(hr)) {
            return failed("GetDesignGlyphMetrics", hr);
        }
        DWRITE_FONT_METRICS face_metrics{};
        face->GetMetrics(&face_metrics);
        if (face_metrics.designUnitsPerEm == 0) {
            return failed("the face reports no em size", E_FAIL);
        }

        // And the two shapes outside ASCII that the grid draws. DirectWrite
        // answers a missing glyph by falling back to another family rather than
        // by failing, so a face without these would put a triangle from some
        // other font in the header and nothing would say so. Glyph zero is the
        // face's own way of saying it does not have the character.
        //
        // Their advance is asked for as well, because the header gives the
        // marker a box exactly one character wide: a face that drew its
        // triangles double-width — several do, for the look of it — would have
        // that box trim the marker away to an ellipsis, which is a sorted column
        // marked with the truncation sign.
        const UINT32 markers[2] = {0x25B2, 0x25BC};
        UINT16 marker_glyphs[2] = {};
        DWRITE_GLYPH_METRICS marker_metrics[2] = {};
        hr = face->GetGlyphIndices(markers, 2, marker_glyphs);
        if (SUCCEEDED(hr)) {
            hr = face->GetDesignGlyphMetrics(marker_glyphs, 2, marker_metrics);
        }
        check(SUCCEEDED(hr) && marker_glyphs[0] != 0 && marker_glyphs[1] != 0,
              "the face has both sort markers of its own");
        check(marker_metrics[0].advanceWidth == metrics[0].advanceWidth
                  && marker_metrics[1].advanceWidth == metrics[0].advanceWidth,
              "and draws them in one character's width");

        bool uniform = true;
        for (UINT32 i = 1; i < 95; ++i) {
            if (metrics[i].advanceWidth != metrics[0].advanceWidth) {
                uniform = false;
            }
        }
        check(uniform, "every printable ASCII glyph has the same advance");

        const float per_em = static_cast<float>(face_metrics.designUnitsPerEm);
        advance = static_cast<float>(metrics[0].advanceWidth) * kFontSize / per_em;
        // Asked of the face rather than assumed from the size, and then checked
        // rather than used to position anything. Where the text sits is
        // transcribed from the macOS grid; what this answers is whether the line
        // still fits under that number, which is the thing a different face
        // would silently break — text clipped at the bottom of every row.
        line_height = static_cast<float>(face_metrics.ascent + face_metrics.descent
                                         + face_metrics.lineGap)
                      * kFontSize / per_em;
        check(line_height <= kRowHeight - kCellTextY, "a line of text fits inside a row");

        hr = dwrite->CreateTextFormat(kFamily, nullptr, DWRITE_FONT_WEIGHT_NORMAL,
                                      DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_STRETCH_NORMAL,
                                      kFontSize, L"en-us", &format);
        if (FAILED(hr)) {
            return failed("CreateTextFormat", hr);
        }

        // Made once, from the format, because every cell that overruns its
        // column shows the same one.
        hr = dwrite->CreateEllipsisTrimmingSign(format.Get(), &ellipsis);
        if (FAILED(hr)) {
            return failed("CreateEllipsisTrimmingSign", hr);
        }
        return uniform;
    }
};

// -------------------------------------------------------------------------
// One result, as the grid holds it
// -------------------------------------------------------------------------

// The word, not an empty cell. `GridRenderer.swift` spells NULL out in a dimmer
// tone because an empty cell and a NULL are different values and a user editing
// one of them needs to know which — and it sets a floor of four characters on
// the column for exactly that reason.
constexpr const wchar_t* kNullText = L"NULL";

// The sort markers `GlyphAtlas.swift` bakes, by codepoint rather than as
// literals: this file is compiled without `/utf-8`, so a triangle typed into the
// source would arrive as whatever the compiler guessed the encoding was.
constexpr const wchar_t* kSortAscending = L"\u25B2";   // BLACK UP-POINTING TRIANGLE
constexpr const wchar_t* kSortDescending = L"\u25BC";  // BLACK DOWN-POINTING TRIANGLE

struct Column {
    // As the server spelled it, for the ORDER BY. `heading` is the same name
    // widened for drawing, and it is deliberately not the one that goes into
    // SQL: a round trip through UTF-16 and back is a chance to change an
    // identifier the server has to match exactly.
    std::string name;
    std::wstring heading;
    // What the server called this column's type, drawn under its name. Empty
    // where the driver had no answer to give, which is a state with its own
    // meaning rather than a blank to fill in.
    std::wstring type_name;
    std::vector<std::wstring> cells;
    std::vector<bool> nulls;
    // Rows somebody has typed a value into. Parallel to `cells` rather than a
    // set of indices, because every reader of one is already walking the other.
    std::vector<bool> edited;
    // A property of the type, not of any value in it. A column of numbers is
    // read by comparing magnitudes down it, and that only works when the units
    // line up; `GridRenderer.swift` asks the column's kind the same question.
    bool numeric = false;
    float x = 0.0f;
    float width = 0.0f;
};

// Which column the result is ordered by, and which way. `GridSort` over there.
struct Sort {
    int column = 0;
    bool descending = false;
};

// A click on a heading, as the three states `AppModel.toggleSort` cycles
// through: unsorted becomes ascending, ascending becomes descending, and
// descending clears — false here, which is why this answers with a bool rather
// than always producing a sort.
//
// Three states rather than two because the unsorted result is a state worth
// being able to get back to: it is the order the server chose, and on a browse
// that is the order the rows are stored in. A two-state toggle can only ever
// offer a user who sorted by mistake some other sort.
//
// A click on a different column always starts at ascending, whichever way the
// previous one was pointing. Carrying the direction across would make the same
// click mean two different things depending on where the last one landed.
bool next_sort(const Sort* current, int column, Sort* out) {
    if (current != nullptr && current->column == column) {
        if (current->descending) {
            return false;
        }
        *out = Sort{column, true};
        return true;
    }
    *out = Sort{column, false};
    return true;
}

// The ORDER BY the sort asks the server for.
//
// The server sorts, not this. A grid holding a page of a million rows can only
// order the page it has, and a page ordered by itself is a lie told in the one
// place the user is most likely to believe it — the rows would be sorted and
// still be the wrong rows.
//
// By name rather than by ordinal, because a name survives a change to the
// select list and an ordinal does not. Quoted, so a column called `order` or
// one with a capital in it still resolves; an embedded quote is doubled, which
// is how both DuckDB and PostgreSQL spell one inside an identifier. (The Swift
// side wraps the name without doubling, and a column named with a quote in it
// breaks the statement there — worth fixing on that side.)
std::string quoted_identifier(const std::string& name) {
    std::string quoted = "\"";
    for (const char c : name) {
        quoted += c;
        if (c == '"') {
            quoted += c;
        }
    }
    return quoted + "\"";
}

std::string order_clause(const Sort* sort, const std::vector<Column>& columns) {
    if (sort == nullptr || sort->column < 0
        || static_cast<size_t>(sort->column) >= columns.size()) {
        return std::string();
    }
    const std::string quoted = quoted_identifier(columns[sort->column].name);
    return sort->descending ? quoted + " DESC" : quoted;
}

// The four questions the menu asks about a cell.
enum class Filter { Equals, NotEquals, IsNull, IsNotNull };

// A value the server will read back as a value rather than as SQL. The same
// doubling as an identifier, on the other quote.
//
// This is the only place in this front end where something a server sent goes
// back to it inside a statement, which is the whole reason the doubling is here
// and not left to the caller to remember.
std::string quoted_text(const std::string& value) {
    std::string quoted = "'";
    for (const char c : value) {
        quoted += c;
        if (c == '\'') {
            quoted += c;
        }
    }
    return quoted + "'";
}

// What one menu item means as SQL, about one cell.
//
// Equals on an absent value is `IS NULL` rather than `= NULL`, which is never
// true: an item called "Equals This Value" that always emptied the grid would
// be reporting a fact about three-valued logic instead of doing what it says.
// `AppController.swift` carries the same distinction as a nil value and lets
// the core spell it; there is no core in this statement's path, so it is spelled
// here.
//
// By name rather than by position, for `order_clause`'s reason: a name survives
// a change to the select list.
std::string cell_predicate(const Column& column, int row, Filter op) {
    const std::string name = quoted_identifier(column.name);
    const size_t at = static_cast<size_t>(row);
    const bool absent = row < 0 || at >= column.nulls.size() || column.nulls[at];
    switch (op) {
    case Filter::IsNull:
        return name + " IS NULL";
    case Filter::IsNotNull:
        return name + " IS NOT NULL";
    case Filter::Equals:
        return absent ? name + " IS NULL"
                      : name + " = " + quoted_text(narrow(column.cells[at]));
    case Filter::NotEquals:
        return absent ? name + " IS NOT NULL"
                      : name + " <> " + quoted_text(narrow(column.cells[at]));
    }
    return std::string();
}

// The stack of them, in the order they were added. ANDed, because that is what
// adding one to another means — each answer narrows what the last one left.
std::string where_clause(const std::vector<std::string>& filters) {
    std::string clause;
    for (const std::string& one : filters) {
        if (!clause.empty()) {
            clause += " AND ";
        }
        clause += one;
    }
    return clause;
}

// The context menu's items. Numbered from one because `TrackPopupMenu` answers
// nought for a menu that was dismissed, so an item with that id could never be
// told from somebody pressing Escape.
//
// Up here with the clauses rather than down with the window, because which item
// was chosen is a question that can be asked without one — and until the filters
// arrived, nothing had ever asked it.
constexpr UINT kCopyValue = 1;
constexpr UINT kCopyRows = 2;
constexpr UINT kCopyRowsAsCsv = 3;
constexpr UINT kClearFilter = 4;

// The four filters, twice over: once replacing whatever is on and once adding to
// it. Encoded rather than named eight times, because `TrackPopupMenu` hands back
// one number and eight constants would be eight things to keep in step with the
// four entries the submenu is built from.
constexpr UINT kFilterFirst = 8;
constexpr UINT kFilterOps = 4;

bool filter_chosen(int id, Filter* op, bool* extend) {
    const int at = id - static_cast<int>(kFilterFirst);
    if (at < 0 || at >= static_cast<int>(kFilterOps) * 2) {
        return false;
    }
    *op = static_cast<Filter>(at % static_cast<int>(kFilterOps));
    *extend = at >= static_cast<int>(kFilterOps);
    return true;
}

// One submenu of four, built twice over.
//
// The two NULL entries are spelled the way SQL spells them, which is what
// `AppController.swift` says about its own: the item is naming the predicate it
// produces, and somebody reading it is reading SQL rather than this menu.
HMENU filter_submenu(bool extend) {
    HMENU menu = CreatePopupMenu();
    if (menu == nullptr) {
        return nullptr;
    }
    const wchar_t* const names[kFilterOps] = {L"Equals This Value", L"Does Not Equal This Value",
                                              L"IS NULL", L"IS NOT NULL"};
    for (UINT at = 0; at < kFilterOps; ++at) {
        AppendMenuW(menu, MF_STRING, kFilterFirst + at + (extend ? kFilterOps : 0), names[at]);
    }
    return menu;
}

// Which cell the keyboard acts on, and how far a shift-held arrow has taken the
// band away from it.
//
// The cursor stays a single cell even when many rows are selected.
// `MetalGridView.swift` says why: the cell inspector, the scroll-into-view and
// the keyboard all need one point to work from, and a range without a moving end
// has nothing to extend.
struct Selection {
    int row = 0;
    int column = 0;
    // The row a shift-extended range grew from. Absent for a plain single-cell
    // selection, which is what lets an unshifted arrow collapse the range by
    // clearing it rather than by having to compute a new one.
    bool anchored = false;
    int anchor = 0;

    int first_row() const { return anchored && anchor < row ? anchor : row; }
    int last_row() const { return anchored && anchor > row ? anchor : row; }
};

// A cell as it should land on the clipboard: the value, not the way the grid
// spells it. NULL copies as nothing rather than as the word, which would paste
// into the next tool as a four-character string that nothing there reads as
// absent. `GridClipboard.swift` draws the same line.
std::wstring copied_value(const Column& column, int row) {
    const size_t at = static_cast<size_t>(row);
    if (row < 0 || at >= column.cells.size() || column.nulls[at]) {
        return std::wstring();
    }
    return column.cells[at];
}

// A tab or a newline inside a value would add columns and rows nobody selected,
// so they collapse to spaces. Quoting them is CSV's answer and would stop this
// being pasteable as plain text, which is the whole of what tab-separated is
// for.
std::wstring sanitized(std::wstring value) {
    for (wchar_t& c : value) {
        if (c == L'\t' || c == L'\n' || c == L'\r') {
            c = L' ';
        }
    }
    return value;
}

// Rows as tab-separated text with the column names on the first line, which is
// the one shape a spreadsheet, a SQL console and a text editor all read
// unchanged.
//
// Kept out of the window so the checks can ask what a paste would contain
// without one, the same reason `GridClipboard.swift` is not in the view.
//
// CRLF rather than the bare newline the macOS side writes. This is the line
// ending every Windows program that reads `CF_UNICODETEXT` expects, and the
// older ones show a paste that lacks it as one long line.
std::wstring tab_separated(const std::vector<Column>& columns, int first, int last) {
    if (columns.empty()) {
        return std::wstring();
    }
    std::wstring out = columns[0].heading;
    for (size_t c = 1; c < columns.size(); ++c) {
        out += L'\t';
        out += columns[c].heading;
    }
    for (int r = first; r <= last; ++r) {
        out += L"\r\n";
        for (size_t c = 0; c < columns.size(); ++c) {
            if (c > 0) {
                out += L'\t';
            }
            out += sanitized(copied_value(columns[c], r));
        }
    }
    return out;
}

// One CSV field, quoted only where it has to be: a comma, a double quote or a
// line ending inside it, and a double quote doubled. RFC 4180 allows quoting
// everything, and quoting everything is correct and unreadable.
//
// A tab is an ordinary character here, unlike the rendering above — this format
// has a way to carry one, so there is nothing to protect the reader from.
std::wstring csv_field(const std::wstring& value) {
    if (value.find_first_of(L",\"\r\n") == std::wstring::npos) {
        return value;
    }
    std::wstring quoted = L"\"";
    for (const wchar_t c : value) {
        quoted += c;
        if (c == L'"') {
            quoted += c;
        }
    }
    return quoted + L'"';
}

// The same rows as RFC 4180 CSV: the column names, then one line per row.
std::wstring csv_rows(const std::vector<Column>& columns, int first, int last) {
    if (columns.empty()) {
        return std::wstring();
    }
    std::wstring out = csv_field(columns[0].heading);
    for (size_t c = 1; c < columns.size(); ++c) {
        out += L',';
        out += csv_field(columns[c].heading);
    }
    for (int r = first; r <= last; ++r) {
        out += L"\r\n";
        for (size_t c = 0; c < columns.size(); ++c) {
            if (c > 0) {
                out += L',';
            }
            out += csv_field(copied_value(columns[c], r));
        }
    }
    return out;
}

// What Ctrl+C puts on the clipboard.
//
// One row copies as the value under the cursor and nothing else — no name, no
// separator, and deliberately not sanitised: a lone value has no format to
// break, and a cell holding a tab should paste as the cell. Several rows copy
// as the rendering above. The menu offers all of them by name and so has no
// such rule; this is the one key, and it has to guess.
std::wstring clipboard_text(const std::vector<Column>& columns, const Selection& selection) {
    if (columns.empty()) {
        return std::wstring();
    }
    const int first = selection.first_row();
    const int last = selection.last_row();
    const size_t at = static_cast<size_t>(selection.column);
    if (first == last) {
        return at < columns.size() ? copied_value(columns[at], first) : std::wstring();
    }
    return tab_separated(columns, first, last);
}

// "1 row", "3 rows", for the menu items that offer to copy them.
//
// Ungrouped, which is where this parts company with `AppModel.pluralized`: that
// one reaches for a decimal `NumberFormatter` and writes 1,234. Doing it
// properly here means `LOCALE_SGROUPING`, which is not three digits everywhere,
// and this grid's result is forty rows. The divergence starts at a thousand,
// and this is the note to start from when something here can produce one.
std::wstring row_count_phrase(int count) {
    return std::to_wstring(count) + (count == 1 ? L" row" : L" rows");
}

// Puts one string on the clipboard, as the only format this grid has to offer.
//
// `CF_UNICODETEXT` and no second rendering: the text above is already the whole
// answer, and a program that wants bytes converts the Unicode itself — the
// synthesised `CF_TEXT` Windows offers on its own is better than one written
// here, since it uses the reader's own code page.
bool put_on_clipboard(HWND owner, const std::wstring& text) {
    if (OpenClipboard(owner) == 0) {
        return failed("OpenClipboard", HRESULT_FROM_WIN32(GetLastError()));
    }
    // Emptied before anything is set. Not because the set would fail without it
    // — it succeeds, which is what makes this worth writing down — but because
    // the formats the last owner left stay there otherwise, and a program that
    // prefers one of them pastes the previous copy out of a clipboard this one
    // appears to have taken. `NSPasteboard.clearContents()` is the same call for
    // the same reason.
    EmptyClipboard();

    // Terminated, and the terminator is inside the block. `CF_UNICODETEXT` is a
    // NUL-terminated string rather than a counted one — the allocation's size is
    // not where the reader stops.
    const size_t bytes = (text.size() + 1) * sizeof(wchar_t);
    // Moveable rather than fixed, because that is what `SetClipboardData`
    // documents: it takes ownership of a handle it is free to move, and
    // `GMEM_FIXED` answers with a pointer rather than a handle. A fixed block is
    // accepted here and pastes back correctly — measured, by giving this one
    // `GMEM_FIXED` — so nothing in this program can tell the two apart. The
    // contract is the reason, not an observed failure.
    HGLOBAL block = GlobalAlloc(GMEM_MOVEABLE, bytes);
    if (block == nullptr) {
        CloseClipboard();
        return failed("GlobalAlloc", HRESULT_FROM_WIN32(GetLastError()));
    }
    void* into = GlobalLock(block);
    if (into == nullptr) {
        GlobalFree(block);
        CloseClipboard();
        return failed("GlobalLock", HRESULT_FROM_WIN32(GetLastError()));
    }
    std::memcpy(into, text.c_str(), bytes);
    GlobalUnlock(block);

    // From here the block belongs to the clipboard whether or not this succeeds
    // — freeing it after a successful set is a free of somebody else's memory,
    // and not freeing it after a failed one is the leak.
    if (SetClipboardData(CF_UNICODETEXT, block) == nullptr) {
        GlobalFree(block);
        CloseClipboard();
        return failed("SetClipboardData", HRESULT_FROM_WIN32(GetLastError()));
    }
    CloseClipboard();
    return true;
}

// How far the view has been taken through the result, and how big the view is.
//
// `scroll_row` is fractional, like `GridRenderer.swift`'s. A wheel notch is a
// distance rather than a row, and rounding one to whole rows turns a smooth
// gesture into a series of jumps — on a trackpad, where the notches are small
// and continuous, into a series of jumps that mostly do nothing. `scroll_x` is
// in DIPs, because sideways there is no other unit: columns are not all one
// width, so there is no "column" to count in.
//
// One struct rather than a growing parameter list, because the two axes stopped
// being independent the moment the second one existed. The horizontal bar is
// drawn over the last row, so having it takes a gutter off the rows that can be
// scrolled to; the vertical bar takes a gutter off the horizontal one's track.
// Every function here has to be able to ask about both axes to answer about
// one, and six floats threaded through six of them is how the two answers start
// disagreeing.
struct View {
    float width = 0.0f;
    float height = 0.0f;
    // The sum of the column widths: what the horizontal bar reports against and
    // what a sideways scroll is held to.
    float content_width = 0.0f;
    size_t rows = 0;
    float scroll_row = 0.0f;
    float scroll_x = 0.0f;
};

float content_width_of(const std::vector<Column>& columns) {
    return columns.empty() ? 0.0f : columns.back().x + columns.back().width;
}

// Which bars are there. Both at once and in this order, because the dependency
// runs one way: whether the result is wider than the view is a fact about the
// columns, while whether it is taller depends on the gutter the horizontal bar
// takes. Asking the second question first is how a grid ends up with a bar it
// decided it needed because of a bar it then decided it did not.
struct Bars {
    bool vertical = false;
    bool horizontal = false;
};

// Rows that fit below the header and above the horizontal bar.
//
// The gutter comes off because the bar is drawn over the data rather than
// beside it: without this, scrolling to the end parks the last row underneath
// the bar, where it can be seen and not read. At least one, so a view shorter
// than its own header still has somewhere to put the cursor.
float visible_row_span(const View& view) {
    const float horizontal = view.content_width > view.width ? kScrollbarGutter : 0.0f;
    const float span = (view.height - kHeaderHeight - horizontal) / kRowHeight;
    return span < 1.0f ? 1.0f : span;
}

Bars bars_of(const View& view) {
    Bars bars;
    bars.horizontal = view.content_width > view.width;
    bars.vertical = static_cast<float>(view.rows) > visible_row_span(view);
    return bars;
}

// Rows that the grid draws at a given scroll position. One more than fits,
// because the first row on screen is usually only partly on it and so is the
// last: drawing exactly as many as fit leaves a gap along the bottom edge at
// every scroll position that is not a whole number of rows.
//
// Not `visible_row_span`, and the difference is deliberate: this one counts the
// rows under the horizontal bar too. The bar is four percent of a tint drawn
// over the data, so the row beneath it still has to be there.
struct RowSpan {
    size_t first = 0;
    size_t last = 0;
};

RowSpan visible_rows(const View& view) {
    RowSpan span;
    if (view.scroll_row > 0.0f) {
        span.first = static_cast<size_t>(view.scroll_row);
    }
    const float usable = view.height - kHeaderHeight;
    if (usable > 0.0f) {
        span.last = span.first + static_cast<size_t>(std::ceil(usable / kRowHeight)) + 1;
    }
    return span.last > view.rows ? RowSpan{span.first, view.rows} : span;
}

// Whole rows the view can show at once, which is not the same quantity as the
// fractional span above and is not interchangeable with it. This one answers
// "is that row on screen", so a row half under the bottom edge does not count.
float whole_visible_rows(const View& view) {
    const float span = std::floor(visible_row_span(view));
    return span < 1.0f ? 1.0f : span;
}

// How far a page key moves: every whole row the view holds, less one.
//
// The row that was against one edge is the row against the other once the key
// has been pressed, and that shared line is what a page of reading is joined to
// the last one by. A page of exactly the view's height leaves two consecutive
// screens with nothing in common, and no way to tell a row that was skipped from
// one that was read. `AppController.swift` keeps the same row back.
int page_rows(const View& view) {
    const int span = static_cast<int>(whole_visible_rows(view)) - 1;
    // A view with room for a single row still moves by one. Nought would be a
    // key that reads as broken, and the quantity above is allowed to be one.
    return span < 1 ? 1 : span;
}

// Where one key takes the cursor, or no answer at all for a key the grid does
// not use.
//
// One function rather than a case per key in the message handler, because this
// is the whole of what the keyboard means and `--verify-grid` has no window to
// press a key in. Everything it needs is a value — the cursor, the view that
// decides how far a page is, and where the result ends — so every answer below
// can be asked for directly instead of being inferred from a photograph.
//
// The virtual key rather than a decoded direction, so that the pairing of a key
// with what it does is inside the part that is checked. A vocabulary in between
// would be one more place for `VK_NEXT` to be given the wrong sign, and it would
// be the place with nothing asking.
bool key_moves(WPARAM key, const Selection& from, const View& view, bool extend, int last_row,
               int last_column, Selection* out) {
    int row = from.row;
    int column = from.column;
    bool extending = extend;
    switch (key) {
    case VK_UP:
        row -= 1;
        break;
    case VK_DOWN:
        row += 1;
        break;
    // Sideways never extends. The band is a range of rows, so a key that moves
    // across the columns has no range to grow, and holding shift while pressing
    // one asks for nothing this grid has; it does what the unshifted key does,
    // which is to drop the band.
    case VK_LEFT:
        column -= 1;
        extending = false;
        break;
    case VK_RIGHT:
        column += 1;
        extending = false;
        break;
    case VK_PRIOR:
        row -= page_rows(view);
        break;
    case VK_NEXT:
        row += page_rows(view);
        break;
    // The ends of the result, not the ends of the row. A grid's Home is the
    // first row: sideways the cursor is already a press or two from either end
    // of a handful of columns, while the far end of a result is a hundred
    // thousand rows away and otherwise reachable only by holding a key down.
    case VK_HOME:
        row = 0;
        break;
    case VK_END:
        row = last_row;
        break;
    default:
        return false;
    }

    Selection next = from;
    // Taken before the move, so a shift-held key grows the band from where the
    // cursor was rather than from where it lands.
    if (extending && !from.anchored) {
        next.anchored = true;
        next.anchor = from.row;
    } else if (!extending) {
        next.anchored = false;
    }
    // Clamped rather than wrapped: a key at the edge of a result does nothing,
    // which is what every grid does and what stops a keystroke from teleporting
    // the cursor to the far corner. Here rather than in each case above, because
    // six of the eight can name a row outside the result and this is the only
    // place that knows where it ends.
    next.row = row < 0 ? 0 : (row > last_row ? last_row : row);
    next.column = column < 0 ? 0 : (column > last_column ? last_column : column);
    *out = next;
    return true;
}

// Every row of the column the cursor is in, which is what Ctrl+A means here.
//
// The column is kept rather than reset: the band is a range of rows whatever
// the cursor is on, and the cell it acts from is the one the user was already
// looking at. `AppController.swift` keeps it for the same reason.
//
// Anchored at the first row with the cursor on the last, rather than the other
// way round. Either covers the same rows, but the cursor is the end a shift-key
// moves — and after selecting everything, the only direction left to go is back
// from the end.
Selection all_rows(const Selection& from, int last_row) {
    Selection every = from;
    every.anchored = true;
    every.anchor = 0;
    every.row = last_row;
    return every;
}

// The text a cell opens for editing with.
//
// Nothing where the value is absent. NULL is what the grid draws over an empty
// cell, not what the cell holds, and a box that opened on the word would have
// somebody editing a label — the first keystroke would be appended to it.
std::wstring edit_seed(const std::vector<Column>& columns, const Selection& at) {
    const size_t c = static_cast<size_t>(at.column);
    if (at.column < 0 || c >= columns.size()) {
        return std::wstring();
    }
    return copied_value(columns[c], at.row);
}

// One committed edit, written into the grid's own copy of the result.
//
// And no further, which is this brick's whole boundary. There is no Save on
// this side and nothing to write against — the statement behind this grid is a
// `SELECT` over `range(40)`, which no server would take an UPDATE for — so a
// commit produces a marked cell and a value that copies, and nothing leaves the
// process.
//
// When there is a Save, the staging will have to be keyed by the row rather
// than by where the row currently sits: `sort_by` re-reads the result and
// builds new columns, so these marks do not survive a sort. That is the same
// problem `dbedit` solves on the other side, and it belongs with the brick that
// can write.
void stage_edit(std::vector<Column>* columns, const Selection& at, const std::wstring& text) {
    const size_t c = static_cast<size_t>(at.column);
    if (at.column < 0 || c >= columns->size()) {
        return;
    }
    Column& column = (*columns)[c];
    const size_t r = static_cast<size_t>(at.row);
    if (at.row < 0 || r >= column.cells.size()) {
        return;
    }
    column.cells[r] = text;
    // Present, whatever it was. Somebody typed over it, and what they typed is
    // the value — an empty box is an empty string rather than a NULL, because
    // there is no key here that means "absent" and inventing one that happens
    // to be "type nothing" would make the two impossible to tell apart.
    column.nulls[r] = false;
    column.edited[r] = true;
}

// Where Tab leaves the cursor, or no answer at the two edges.
//
// Wrapping to the next row is what a spreadsheet does and this is not one: a
// row here is a row of a table somebody is about to write to, and a Tab that
// quietly moved to a different one would put the next thing typed somewhere
// nobody looked. `AppController.swift` refuses at the same two places, and also
// steps over columns the grid is not drawing — this side has no hidden columns
// to step over.
bool next_edit_column(int from, int step, int last_column, int* out) {
    const int next = from + step;
    if (next < 0 || next > last_column) {
        return false;
    }
    *out = next;
    return true;
}

// The largest scroll that still fills the view. Past it the grid would show
// blank space after the last row or column, and a scrollbar built on it would
// reach the end of its track before the result ran out.
float max_scroll_row(const View& view) {
    const float most = static_cast<float>(view.rows) - visible_row_span(view);
    return most > 0.0f ? most : 0.0f;
}

float max_scroll_x(const View& view) {
    const float most = view.content_width - view.width;
    return most > 0.0f ? most : 0.0f;
}

// The smallest movement that brings a row into view: above the fold it becomes
// the top row, below it the last whole one. `GridRenderer.swift` again — a grid
// that centred the row instead would move the whole page for a one-row step,
// and the user would lose their place on every arrow key.
float scroll_to_visible(const View& view, int row) {
    const float target = static_cast<float>(row);
    const float span = whole_visible_rows(view);
    float next = view.scroll_row;
    if (target < view.scroll_row) {
        next = target;
    } else if (target >= view.scroll_row + span - 1.0f) {
        next = target - span + 2.0f;
    }
    const float most = max_scroll_row(view);
    if (next > most) {
        next = most;
    }
    return next < 0.0f ? 0.0f : next;
}

// The same, sideways. In DIPs rather than in columns, and by the column's two
// edges rather than by its index: a column wider than the view can only be
// brought partly in, and this brings its leading edge, which is where the value
// starts and — in a numeric column — is the end that gets cut off.
float scroll_x_to_visible(const View& view, const std::vector<Column>& columns, int column) {
    if (column < 0 || static_cast<size_t>(column) >= columns.size()) {
        return view.scroll_x;
    }
    const float left = columns[static_cast<size_t>(column)].x;
    const float width = columns[static_cast<size_t>(column)].width;
    float next = view.scroll_x;
    if (left < view.scroll_x) {
        next = left;
    } else if (left + width > view.scroll_x + view.width) {
        next = left + width - view.width;
    }
    const float most = max_scroll_x(view);
    if (next > most) {
        next = most;
    }
    return next < 0.0f ? 0.0f : next;
}

// Where a scrollbar's track and thumb sit, along its own axis, in DIPs.
struct Scrollbar {
    float track_start = 0.0f;
    float track_length = 0.0f;
    float thumb_start = 0.0f;
    float thumb_length = 0.0f;
};

// Absent when the result already fits on that axis, which is the difference
// between a grid with nothing past the edge and a grid whose bar is pinned
// full-length and means nothing.
bool scrollbar_of(bool horizontal, const View& view, Scrollbar* out) {
    const Bars bars = bars_of(view);
    float showing = 0.0f;
    float total = 0.0f;
    float progress = 0.0f;
    if (horizontal) {
        if (!bars.horizontal) {
            return false;
        }
        out->track_start = 0.0f;
        out->track_length = view.width - (bars.vertical ? kScrollbarGutter : 0.0f);
        showing = view.width;
        total = view.content_width;
        const float most = max_scroll_x(view);
        progress = most > 0.0f ? view.scroll_x / most : 0.0f;
    } else {
        if (!bars.vertical) {
            return false;
        }
        out->track_start = kHeaderHeight;
        out->track_length =
            view.height - kHeaderHeight - (bars.horizontal ? kScrollbarGutter : 0.0f);
        showing = visible_row_span(view);
        total = static_cast<float>(view.rows);
        const float most = max_scroll_row(view);
        progress = most > 0.0f ? view.scroll_row / most : 0.0f;
    }
    if (out->track_length <= 0.0f) {
        return false;
    }
    // How much of the result is on screen, floored so it stays grabbable, and
    // capped at the track so a short result cannot ask for a thumb longer than
    // the space it runs in.
    const float proportional = out->track_length * showing / total;
    out->thumb_length = proportional < kMinThumbLength ? kMinThumbLength : proportional;
    if (out->thumb_length > out->track_length) {
        out->thumb_length = out->track_length;
    }
    // Along the travel the thumb has, not along the track: at the end of the
    // scroll the thumb's trailing edge is on the track's, and a fraction taken
    // of the track instead would leave it a thumb's length short.
    progress = progress < 0.0f ? 0.0f : (progress > 1.0f ? 1.0f : progress);
    out->thumb_start = out->track_start + (out->track_length - out->thumb_length) * progress;
    return true;
}

// The scroll that puts the thumb's leading edge at `thumb_start`. The inverse of
// the line above, and the whole of what a drag does.
float scroll_to_thumb(bool horizontal, float thumb_start, const View& view) {
    Scrollbar bar;
    if (!scrollbar_of(horizontal, view, &bar)) {
        return 0.0f;
    }
    const float travel = bar.track_length - bar.thumb_length;
    if (travel <= 0.0f) {
        return 0.0f;
    }
    float progress = (thumb_start - bar.track_start) / travel;
    progress = progress < 0.0f ? 0.0f : (progress > 1.0f ? 1.0f : progress);
    return progress * (horizontal ? max_scroll_x(view) : max_scroll_row(view));
}

// The gutter a point is in, if any. Both bars are drawn over the data, so this
// has to be asked before anything else that a press could mean.
bool scrollbar_axis_at(float x, float y, const View& view, bool* horizontal) {
    Scrollbar bar;
    if (scrollbar_of(false, view, &bar) && x >= view.width - kScrollbarGutter
        && y >= kHeaderHeight) {
        *horizontal = false;
        return true;
    }
    if (scrollbar_of(true, view, &bar) && y >= view.height - kScrollbarGutter) {
        *horizontal = true;
        return true;
    }
    return false;
}

// Which appearance the user is in, asked of Windows rather than chosen here.
//
// The registry rather than `UISettings`: this is one DWORD, and the WinRT route
// wants an apartment and a package identity to answer the same question. The
// value is what the personalisation page writes, and it is the one Explorer and
// the common controls read, so an app that follows it changes when everything
// else on the desktop does.
//
// Absent means light. It only exists once somebody has been to that page, and
// Windows treats its absence the same way.
bool windows_is_light() {
    DWORD value = 1;
    DWORD size = sizeof(value);
    const LSTATUS status =
        RegGetValueW(HKEY_CURRENT_USER,
                     L"Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize",
                     L"AppsUseLightTheme", RRF_RT_REG_DWORD, nullptr, &value, &size);
    return status != ERROR_SUCCESS || value != 0;
}

// The cell under a point, in DIPs.
//
// Pure, and separate from the window, because this is the half of pointing at
// something that can be checked without anything to point with. The window's
// share is converting a click into these coordinates and asking.
//
// Three misses rather than a nearest-cell answer: the header band, the space
// past the last column and the space below the last row are all places a click
// lands often, and none of them is a cell. Snapping to the closest one would
// move the selection somewhere the user did not point at.
bool cell_at(float x, float y, const View& view, const std::vector<Column>& columns,
             Selection* out) {
    if (x < 0.0f || y < kHeaderHeight) {
        return false;
    }
    // Through the same scroll the drawing used, on both axes, so the answer is
    // the cell the user is looking at rather than the one that would be there
    // unscrolled.
    const float at = view.scroll_row + (y - kHeaderHeight) / kRowHeight;
    if (at < 0.0f) {
        return false;
    }
    const auto row = static_cast<size_t>(at);
    if (row >= view.rows) {
        return false;
    }
    const float column_x = x + view.scroll_x;
    for (size_t c = 0; c < columns.size(); ++c) {
        if (column_x >= columns[c].x && column_x < columns[c].x + columns[c].width) {
            out->row = static_cast<int>(row);
            out->column = static_cast<int>(c);
            out->anchored = false;
            return true;
        }
    }
    return false;
}

// The heading under a point, which is the other half of the same question and
// deliberately not folded into `cell_at`. A click on the header is not a click
// on a cell that happens to be above the first row: it sorts rather than
// selects, and answering both from one function would mean one of the two
// callers throwing away an answer it must not act on.
bool header_column_at(float x, float y, float scroll_x, const std::vector<Column>& columns,
                      int* out) {
    if (x < 0.0f || y < 0.0f || y >= kHeaderHeight) {
        return false;
    }
    const float at = x + scroll_x;
    for (size_t c = 0; c < columns.size(); ++c) {
        if (at >= columns[c].x && at < columns[c].x + columns[c].width) {
            *out = static_cast<int>(c);
            return true;
        }
    }
    return false;
}

// The column whose trailing edge a point is on, for the resize handles.
//
// In the header only, so a drag across the data is never taken for one — down
// there the same x is the middle of a row somebody is selecting.
//
// The handle straddles the boundary rather than sitting inside the column: a
// separator is one DIP and nobody can hit it, and a person aiming at a line
// aims at the line rather than at one side of it. Four DIPs each way is
// `AppController.swift`'s tolerance.
//
// The last column has one too. Its edge leads to nothing, but it is the edge of
// that column, and a grid where the last column alone cannot be narrowed reads
// as a bug in the column rather than as a rule about the edge.
bool column_edge_at(float x, float y, float scroll_x, const std::vector<Column>& columns,
                    int* out) {
    if (y < 0.0f || y >= kHeaderHeight) {
        return false;
    }
    const float at = x + scroll_x;
    for (size_t c = 0; c < columns.size(); ++c) {
        if (std::fabs(columns[c].x + columns[c].width - at) <= kEdgeTolerance) {
            *out = static_cast<int>(c);
            return true;
        }
    }
    return false;
}

// Whether a press acts on a cell where it landed, and on which one. Both presses
// that mean a particular cell ask this: the right-click that opens the menu, and
// the double click that opens the box.
//
// The scrollbars are asked first and refuse, because both are drawn over the
// data: a menu offering to copy a row would cover the thing the pointer was on,
// and a double click meant for the thumb would open a box behind it.
// The header and the resize handles are not asked, and that is deliberate
// rather than forgotten — both live above the fold, `cell_at` refuses
// everything up there, and a guard that cannot change an answer is a guard
// nothing can check. `AppController.swift` asks all three because over there
// the header hit test reaches further down than the cells do.
//
// Shift extends from wherever the band already starts, the way a shift-click
// does, so the menu can offer more than the one cell under the pointer.
bool pointed_cell(float x, float y, const View& view, const std::vector<Column>& columns,
               const Selection* current, bool extend, Selection* out) {
    bool horizontal = false;
    if (scrollbar_axis_at(x, y, view, &horizontal)) {
        return false;
    }
    Selection hit;
    if (!cell_at(x, y, view, columns, &hit)) {
        return false;
    }
    if (extend && current != nullptr) {
        hit.anchored = true;
        hit.anchor = current->anchored ? current->anchor : current->row;
    }
    *out = hit;
    return true;
}

// Arrow's `utf8`: offsets in `buffers[1]`, bytes packed end to end in
// `buffers[2]` with no terminators.
std::string utf8_at(const ArrowArray& array, int64_t i) {
    const auto* offsets = static_cast<const int32_t*>(array.buffers[1]);
    const auto* data = static_cast<const char*>(array.buffers[2]);
    const int32_t from = offsets[array.offset + i];
    const int32_t to = offsets[array.offset + i + 1];
    return std::string(data + from, static_cast<size_t>(to - from));
}

bool valid_at(const ArrowArray& array, int64_t i) {
    const auto* bitmap = static_cast<const uint8_t*>(array.buffers[0]);
    if (bitmap == nullptr) {
        return true;
    }
    const int64_t at = array.offset + i;
    return (bitmap[at / 8] & (1u << (at % 8))) != 0;
}

// The key `dbconn::DECLARED_TYPE` writes the server's own type name under.
//
// Spelled out here as well as there because the C data interface carries no
// shared header: the string is the contract, and a rename on the Rust side that
// only its symbol followed would compile, pass, and quietly stop reaching this
// grid. `ArrowTable.swift` holds the same pair of strings for the same reason.
constexpr const char* kDeclaredType = "dbclient.declared_type";

// One key's value out of a field's metadata, or empty where it is not there.
//
// The C data interface counts its lengths rather than terminating them, so the
// buffer may hold NUL bytes and reading it as a C string would stop at the first
// one: an int32 pair count, then per pair an int32 key length, the key, an int32
// value length, the value. None of it is promised to be aligned, hence the
// copies rather than casts.
//
// A count or a length below zero answers empty rather than trapping — this is
// memory another language handed over, and a grid that crashed on it would be a
// worse failure than a column with no type under its name. A buffer that lies
// about a length in the other direction cannot be caught here: the format
// carries no total size to check one against.
std::string metadata_value(const char* metadata, const char* key) {
    if (metadata == nullptr) {
        return std::string();
    }
    const char* cursor = metadata;
    int32_t pairs = 0;
    std::memcpy(&pairs, cursor, sizeof(pairs));
    cursor += sizeof(pairs);
    const std::string wanted(key);
    for (int32_t p = 0; p < pairs; ++p) {
        std::string taken[2];
        for (std::string& into : taken) {
            int32_t length = 0;
            std::memcpy(&length, cursor, sizeof(length));
            if (length < 0) {
                return std::string();
            }
            cursor += sizeof(length);
            into.assign(cursor, static_cast<size_t>(length));
            cursor += length;
        }
        // The first match wins. A second entry under one key is not something
        // the writer can produce.
        if (taken[0] == wanted) {
            return taken[1];
        }
    }
    return std::string();
}

// Int64 and utf8 only, which is what the query below produces.
//
// A real grid needs every type the drivers can return, and the macOS side has
// that; putting it here now would be writing a second copy of it against a
// surface with no window on it. This is the first brick, and what it has to
// prove is the geometry.
//
// `order` is the ORDER BY the sort asked for, or empty for the result as it
// arrives. The base order is `i` rather than nothing at all so that the checks
// below have a fixed starting point to compare a sorted read against — DuckDB
// is free to hand back an unordered scan in any order it likes, and a check
// that assumed otherwise would be measuring the planner's mood.
//
// `where` is the filter menu's stack of predicates, or empty for all of them.
// It wraps rather than joining the select below: the predicates name the columns
// the grid is showing, which are projections, and a statement reading them where
// they are made is one that depends on DuckDB letting a WHERE see an alias.
bool read_columns(DbHandle* handle, const std::string& order, const std::string& where,
                  std::vector<Column>* out) {
    char* err = nullptr;
    int position = 0;
    const std::string select =
        std::string("SELECT i AS id,"
                    "       'driver-' || i AS name,"
                    "       CASE WHEN i = 1 THEN NULL"
                    "            ELSE 'read ' || (i * 100) END AS note,"
                    // Long enough to be clamped to `kMaxColumnWidth` and then to
                    // overrun that, which is the only way to have anything to
                    // say about what happens to a value the column cannot hold.
                    // With spaces in it, because wrapping breaks at a space and
                    // trimming does not — a value without any would be cut in
                    // the same place either way and the check below would not be
                    // able to tell them apart.
                    //
                    // Its name is long for a second reason. Every column is laid
                    // out one character wider than its widest content, so a
                    // heading can only reach its own trailing edge in a column
                    // that was clamped — and the trailing edge is where the sort
                    // marker goes. This is the only column in which "the marker
                    // is drawn beside the name" and "the marker is drawn on top
                    // of it" look different.
                    "       'a value long enough to run past the widest"
                    " column this grid allows ' || i"
                    " AS \"a heading long enough to be cut by the column it names\" "
                    // More rows than the check's bitmap can show, so that
                    // scrolling has somewhere to go and so that "draws every
                    // row" and "draws the rows in view" stop being the same
                    // statement.
                    "FROM range(40) t(i)");
    // The base order names the projected column rather than the one it was made
    // from: a filter wraps this select in another, and `i` exists only inside.
    const std::string ordered = std::string(" ORDER BY ")
                                + (order.empty() ? std::string("\"id\"") : order);
    const std::string statement = where.empty()
                                      ? select + ordered
                                      : "SELECT * FROM (" + select + ") WHERE " + where + ordered;
    DbQuery* query = db_query(handle, statement.c_str(), 1000, &err, &position);
    if (query == nullptr) {
        return core_failed("db_query", err);
    }

    ArrowSchema schema{};
    if (db_query_schema(query, &schema, &err) != 0) {
        db_query_free(query);
        return core_failed("db_query_schema", err);
    }
    // Nought rather than one is a result with no rows in it, which a filter that
    // matched nothing produces and which is not a failure. The columns are still
    // known — the schema arrived before this call — so an empty result is the
    // headings over nothing, rather than a grid that has lost its columns and
    // cannot say what it was showing.
    ArrowArray batch{};
    const int arrived = db_query_next(query, &batch, &err);
    if (arrived < 0) {
        schema.release(&schema);
        db_query_free(query);
        return core_failed("db_query_next", err);
    }

    for (int64_t c = 0; c < schema.n_children; ++c) {
        const ArrowSchema& field = *schema.children[c];
        const std::string format(field.format);
        Column column;
        column.name = field.name;
        column.heading = widen(field.name);
        column.type_name = widen(metadata_value(field.metadata, kDeclaredType));
        column.numeric = format == "l";
        const ArrowArray* values = arrived == 1 ? batch.children[c] : nullptr;
        for (int64_t r = 0; values != nullptr && r < batch.length; ++r) {
            const bool present = valid_at(*values, r);
            column.nulls.push_back(!present);
            column.edited.push_back(false);
            if (!present) {
                column.cells.push_back(kNullText);
            } else if (format == "u") {
                column.cells.push_back(widen(utf8_at(*values, r)));
            } else if (format == "l") {
                const auto* numbers = static_cast<const int64_t*>(values->buffers[1]);
                column.cells.push_back(std::to_wstring(numbers[values->offset + r]));
            } else {
                // Named rather than read anyway. Reading an `i` as an `l` is not
                // a crash, it is two columns of plausible-looking numbers, and
                // every geometry check below would pass over the top of it.
                std::printf("FAIL  column %s arrived as Arrow format '%s', which is not read here\n",
                            field.name, field.format);
                failures += 1;
                column.cells.push_back(L"?");
            }
        }
        out->push_back(std::move(column));
    }

    if (arrived == 1) {
        batch.release(&batch);
    }
    schema.release(&schema);

    // A statement has to be pulled to exhaustion; the header says so, and it is
    // also where a fault during execution would arrive.
    ArrowArray tail{};
    const int end = db_query_next(query, &tail, &err);
    if (end == 1) {
        tail.release(&tail);
    } else if (end < 0 && err != nullptr) {
        db_string_free(err);
    }
    db_query_free(query);
    return true;
}

// Where the columns start, given how wide they are.
//
// Apart from the measuring below because the two stopped happening together the
// moment a width could be dragged: a resize changes one width and every offset
// after it, and re-measuring there would answer with the width the content
// wants rather than the one the user just asked for.
void place_columns(std::vector<Column>* columns) {
    float x = 0.0f;
    for (Column& column : *columns) {
        column.x = x;
        x += column.width;
    }
}

// `GridRenderer.swift`'s rule, transcribed: the widest of the heading and the
// cells, one character of slack so the longest value stays off the separator,
// padding either side, clamped. NULL counts as four because it renders as the
// word.
void lay_out(std::vector<Column>* columns, float advance) {
    for (Column& column : *columns) {
        size_t chars = column.heading.size();
        // The type gets a say, bounded at `kMaxTypeChars`. Enough that a narrow
        // column of small numbers does not cut `TIMESTAMP` down to `TIME`, which
        // is a different type and would read as one.
        const size_t type = column.type_name.size();
        const size_t said = type > kMaxTypeChars ? kMaxTypeChars : type;
        chars = said > chars ? said : chars;
        for (const std::wstring& cell : column.cells) {
            chars = cell.size() > chars ? cell.size() : chars;
        }
        float width = kCellPadding * 2.0f + static_cast<float>(chars + 1) * advance;
        width = width < kMinColumnWidth ? kMinColumnWidth : width;
        width = width > kMaxColumnWidth ? kMaxColumnWidth : width;
        column.width = width;
    }
    place_columns(columns);
}

// A dragged width, held to the same bounds the measured ones are.
//
// The clamp is `setColumnWidth`'s and it is what keeps a drag from producing a
// column nobody can get back: dragged to nothing, a column has no edge left to
// grab, and dragged past the maximum it pushes every column after it off the
// side of a window that cannot scroll sideways yet.
void set_column_width(std::vector<Column>* columns, size_t index, float width) {
    if (index >= columns->size()) {
        return;
    }
    width = width < kMinColumnWidth ? kMinColumnWidth : width;
    (*columns)[index].width = width > kMaxColumnWidth ? kMaxColumnWidth : width;
    place_columns(columns);
}

// Carries widths from one result onto the next, when they are the same columns.
//
// `reconcileColumnLayout` over there, and its argument is what makes a drag feel
// like a setting rather than an accident: this grid re-runs its statement every
// time a heading is clicked, and re-measuring the result would undo the drag on
// the next sort. Different names are a different result and its widths mean
// nothing, so the caller measures afresh — false says so.
bool carry_widths(const std::vector<Column>& from, std::vector<Column>* to) {
    if (from.size() != to->size()) {
        return false;
    }
    for (size_t c = 0; c < from.size(); ++c) {
        if (from[c].name != (*to)[c].name) {
            return false;
        }
    }
    for (size_t c = 0; c < from.size(); ++c) {
        (*to)[c].width = from[c].width;
    }
    place_columns(to);
    return true;
}

// The brushes the grid draws with, made from whichever target is drawing.
//
// A brush belongs to the target it came from, so this is built twice — once for
// the bitmap the checks read back, once for the window — rather than held
// anywhere shared. Six of them together rather than made where each is first
// needed, because the set is the palette: a colour that appears in one path and
// not the other is the failure this whole arrangement is trying not to have.
struct Palette {
    ComPtr<ID2D1SolidColorBrush> ink;
    ComPtr<ID2D1SolidColorBrush> muted;
    ComPtr<ID2D1SolidColorBrush> header_ink;
    ComPtr<ID2D1SolidColorBrush> sorted_header_ink;
    ComPtr<ID2D1SolidColorBrush> header;
    ComPtr<ID2D1SolidColorBrush> banding;
    ComPtr<ID2D1SolidColorBrush> separator;
    ComPtr<ID2D1SolidColorBrush> selected_row;
    ComPtr<ID2D1SolidColorBrush> selected_cell;
    ComPtr<ID2D1SolidColorBrush> cursor;
    ComPtr<ID2D1SolidColorBrush> pending;
    ComPtr<ID2D1SolidColorBrush> scroll_track;
    ComPtr<ID2D1SolidColorBrush> scroll_thumb;
    ComPtr<ID2D1SolidColorBrush> scroll_thumb_active;
    // The one tone nothing draws with. `Clear` takes a colour rather than a
    // brush, and keeping it here is what lets the appearance travel as a single
    // thing: a palette is every colour resolved for one target, and the surface
    // under them is one of those colours.
    UINT32 canvas = kLightTones.canvas;

    bool open(ID2D1RenderTarget* target, const Tones& tones) {
        canvas = tones.canvas;
        struct Wanted {
            ComPtr<ID2D1SolidColorBrush>* into;
            UINT32 rgb;
            float alpha;
        };
        const Wanted wanted[] = {
            {&ink, tones.ink, 1.0f},
            {&muted, tones.muted_ink, 1.0f},
            {&header_ink, tones.header_ink, 1.0f},
            {&sorted_header_ink, tones.sorted_header_ink, 1.0f},
            {&header, tones.header_band, 1.0f},
            {&banding, tones.rule, tones.banding_alpha},
            {&separator, tones.rule, tones.separator_alpha},
            {&selected_row, tones.accent, kSelectedRowAlpha},
            {&selected_cell, tones.accent, kSelectedCellAlpha},
            {&cursor, tones.accent, 1.0f},
            {&pending, tones.pending, kPendingAlpha},
            {&scroll_track, tones.rule, tones.scroll_track_alpha},
            {&scroll_thumb, tones.rule, tones.scroll_thumb_alpha},
            {&scroll_thumb_active, tones.rule, tones.scroll_thumb_active_alpha},
        };
        for (const Wanted& one : wanted) {
            const HRESULT hr = target->CreateSolidColorBrush(D2D1::ColorF(one.rgb, one.alpha),
                                                            one.into->GetAddressOf());
            if (FAILED(hr)) {
                return failed("CreateSolidColorBrush", hr);
            }
        }
        return true;
    }
};

// One cell's text, laid out the way this grid lays out every cell.
//
// Its own function because a check needs to ask the layout two things the
// bitmap cannot answer — whether it is one line, and whether that line was
// trimmed — and a check that assembled its own layout would be asking them of a
// copy that could drift from this one.
//
// One layout per cell, for now. The macOS grid draws from a glyph atlas instead,
// because a grid draws the same ninety-five shapes tens of thousands of times a
// frame — but that is a decision made against a measurement, and there is no
// frame to measure here yet.
bool make_layout(IDWriteFactory* dwrite, const Monospace& font, const std::wstring& text,
                 float width, bool align_right, ComPtr<IDWriteTextLayout>* out) {
    const HRESULT hr = font.format ? dwrite->CreateTextLayout(text.c_str(),
                                                              static_cast<UINT32>(text.size()),
                                                              font.format.Get(), width,
                                                              kRowHeight, out->GetAddressOf())
                                   : E_FAIL;
    if (FAILED(hr)) {
        return failed("CreateTextLayout", hr);
    }
    if (align_right) {
        (*out)->SetTextAlignment(DWRITE_TEXT_ALIGNMENT_TRAILING);
    }

    // A cell is one line. Left to wrap, a value too wide for its column moves
    // the rest of itself onto a second line that the row's height then clips —
    // which is silent truncation with an extra step, and the part it hides is
    // the middle of the value rather than its end.
    (*out)->SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP);

    // And what does not fit ends in an ellipsis rather than simply stopping.
    // `GridRenderer.swift` calls this the worst failure the grid can have and it
    // is right: `123456789` cut to `12345` does not look truncated, it looks
    // like a different number. By character rather than by word, because a
    // column is a fixed number of characters wide and giving back the last
    // partial word would waste most of one.
    const DWRITE_TRIMMING trimming{DWRITE_TRIMMING_GRANULARITY_CHARACTER, 0, 0};
    (*out)->SetTrimming(&trimming, font.ellipsis.Get());
    return true;
}

bool draw_text(ID2D1RenderTarget* target, IDWriteFactory* dwrite, const Monospace& font,
               const std::wstring& text, float x, float y, float width,
               ID2D1SolidColorBrush* brush, bool align_right) {
    if (text.empty()) {
        return true;
    }
    ComPtr<IDWriteTextLayout> layout;
    if (!make_layout(dwrite, font, text, width, align_right, &layout)) {
        return false;
    }
    target->DrawTextLayout(D2D1::Point2F(x, y), layout.Get(), brush);
    return true;
}

// One frame of the grid, between somebody else's `BeginDraw` and `EndDraw`.
//
// Taking an `ID2D1RenderTarget` rather than the `Surface` above is what lets the
// check and the window draw through the same code: the check gives it a WIC
// bitmap it can then count pixels in, the window gives it an HWND target. A
// check that drew through its own copy of this would go on passing after the
// window stopped agreeing with it, which is the failure it exists to prevent.
//
// Begin and end stay with the caller, because they are not the same call twice:
// the window has to notice `D2DERR_RECREATE_TARGET` coming back from `EndDraw`
// and the bitmap can never see it.
void draw_grid(ID2D1RenderTarget* target, IDWriteFactory* dwrite, const Monospace& font,
               const std::vector<Column>& columns, const Palette& palette,
               const Selection* selection, const Sort* sort, float scroll_row, float scroll_x,
               Bars dragging) {
    const D2D1_SIZE_F size = target->GetSize();
    size_t rows = 0;
    for (const Column& column : columns) {
        rows = column.cells.size() > rows ? column.cells.size() : rows;
    }
    // Measured here rather than passed in. The view is the target's size and
    // the columns' total width, both of which this function already has, and a
    // caller that worked them out separately would be a second opinion about
    // how big the grid is.
    const View view{size.width, size.height, content_width_of(columns),
                    rows,       scroll_row,  scroll_x};
    const RowSpan span = visible_rows(view);

    // Every row's position comes from here, and it takes the row's own number
    // rather than its place on screen. Those are the same only at rest, and the
    // grid that confuses them looks right until it is scrolled: the banding
    // belongs to the row, so striping by screen position makes the stripes swap
    // places under a moving result.
    const auto row_y = [scroll_row](size_t r) {
        return kHeaderHeight + (static_cast<float>(r) - scroll_row) * kRowHeight;
    };
    // And every column's from here, for the same reason on the other axis. The
    // banding and the selected row are not in it: they run the width of the
    // view rather than the width of the content, so they do not move.
    const auto column_x = [scroll_x](const Column& column) { return column.x - scroll_x; };

    target->Clear(D2D1::ColorF(palette.canvas));

    // The order is `GridRenderer.swift`'s, and it is the order rather than a
    // set of layers: banding first so text lands on top of it, separators next,
    // then the header band over the top of the separators so that rows scroll
    // under an opaque strip rather than through a stack of lines.
    for (size_t r = span.first; r < span.last; ++r) {
        if (r % 2 == 0) {
            continue;
        }
        const float y = row_y(r);
        target->FillRectangle(D2D1::RectF(0.0f, y, view.width, y + kRowHeight),
                              palette.banding.Get());
    }

    // Between the banding and the separators, so the band reads as continuous
    // across the row and the grid lines stay on top of it — and under the text,
    // which is what keeps a selected value readable. Both fills are translucent
    // and the glyphs go on last; filling over them instead would put a wash on
    // the one cell the user is looking at.
    if (selection != nullptr) {
        for (int r = selection->first_row(); r <= selection->last_row(); ++r) {
            if (r < 0 || static_cast<size_t>(r) >= rows) {
                continue;
            }
            const float y = row_y(static_cast<size_t>(r));
            target->FillRectangle(D2D1::RectF(0.0f, y, view.width, y + kRowHeight),
                                  palette.selected_row.Get());
        }
    }

    // Cells somebody has typed into, under the cursor cell and over the row's
    // own fills: the mark has to survive the row being selected, because the row
    // somebody is editing is the row they are standing on.
    for (size_t c = 0; c < columns.size(); ++c) {
        const Column& column = columns[c];
        const float x = column_x(column);
        for (size_t r = span.first; r < span.last && r < column.edited.size(); ++r) {
            if (!column.edited[r]) {
                continue;
            }
            const float y = row_y(r);
            target->FillRectangle(D2D1::RectF(x, y, x + column.width, y + kRowHeight),
                                  palette.pending.Get());
        }
    }

    if (selection != nullptr) {
        if (selection->column >= 0 && static_cast<size_t>(selection->column) < columns.size()
            && selection->row >= 0 && static_cast<size_t>(selection->row) < rows) {
            const Column& column = columns[selection->column];
            const float x = column_x(column);
            const float y = row_y(static_cast<size_t>(selection->row));
            // Apart from the band because within a multi-row selection this is
            // the one cell the keyboard and the inspector act on, and it has to
            // stay distinguishable from the rows around it.
            target->FillRectangle(D2D1::RectF(x, y, x + column.width, y + kRowHeight),
                                  palette.selected_cell.Get());
            // A one-DIP edge on the leading side, at full strength. The fill
            // washes out over a dark value; this does not, so the cell stays
            // findable when it does.
            target->FillRectangle(D2D1::RectF(x, y, x + 1.0f, y + kRowHeight),
                                  palette.cursor.Get());
        }
    }

    // On the column's trailing edge, one DIP wide, and only between the columns
    // — the run starts below the header band and the last column has no rule
    // after it, because a line at the right of the last column would read as an
    // empty column beginning there.
    for (size_t c = 0; c + 1 < columns.size(); ++c) {
        const float x = column_x(columns[c]) + columns[c].width;
        target->FillRectangle(D2D1::RectF(x, kHeaderHeight, x + 1.0f, size.height),
                              palette.separator.Get());
    }

    target->FillRectangle(D2D1::RectF(0.0f, 0.0f, size.width, kHeaderHeight),
                          palette.header.Get());
    target->FillRectangle(D2D1::RectF(0.0f, kHeaderHeight - 1.0f, size.width, kHeaderHeight),
                          palette.separator.Get());

    // The text box is the cell inset by its padding on both sides, not the whole
    // column. Left-aligned that distinction never showed; right-aligned it is
    // the difference between a value on the padding and a value against the
    // separator.
    //
    // Headers are left-aligned whichever way their column is. A heading is a
    // name, and names read from the left even above a column of numbers.
    for (size_t c = 0; c < columns.size(); ++c) {
        const Column& column = columns[c];
        const float x = column_x(column);
        const bool ordered = sort != nullptr && sort->column == static_cast<int>(c);
        if (ordered) {
            // On the trailing edge of the heading's line, at full accent
            // strength — this is `Grid.cursor` rather than a header tone,
            // because it is the one mark in the band that reports state rather
            // than naming something.
            draw_text(target, dwrite, font,
                      sort->descending ? kSortDescending : kSortAscending,
                      x + column.width - kCellPadding - font.advance, kHeaderNameY,
                      font.advance, palette.cursor.Get(), false);
        }
        // The marker's character is taken out of the name's box rather than
        // drawn over it. A heading that reached the trailing edge would
        // otherwise have the triangle sitting on its last letter, and the
        // column that is most likely to be sorted is the one whose name fills
        // its width.
        draw_text(target, dwrite, font, column.heading, x + kCellPadding, kHeaderNameY,
                  column.width - kCellPadding * 2.0f - (ordered ? font.advance : 0.0f),
                  ordered ? palette.sorted_header_ink.Get() : palette.header_ink.Get(), false);
        // The type underneath, and across the full width unlike the name: the
        // sort marker is on the name's line, so there is nothing on this one to
        // keep clear of. In `Grid.nullText`'s tone, which is `Text.dataMuted` —
        // the same rung as the word NULL, because both are content somebody
        // reads rather than chrome, and neither is the value.
        draw_text(target, dwrite, font, column.type_name, x + kCellPadding, kHeaderTypeY,
                  column.width - kCellPadding * 2.0f, palette.muted.Get(), false);
    }

    // The values are clipped to below the band; the headings above are not.
    // Once the grid scrolls, the top row is usually only partly on screen, and
    // its text is drawn after the band that is meant to hide it — so without
    // this the row sliding out of view is legible across the column names for
    // the whole of the gesture. The fills need no clip: the band is opaque and
    // is drawn over them.
    //
    // `GridRenderer.swift` has no equivalent. Its Metal pass has no scissor
    // rect and the same guard on the same rows, so the partial row's glyphs go
    // into the buffer after the band's quad. That looks like the artifact this
    // clip prevents rather than a decision, and it is worth a look on that side.
    target->PushAxisAlignedClip(D2D1::RectF(0.0f, kHeaderHeight, size.width, size.height),
                                D2D1_ANTIALIAS_MODE_ALIASED);
    for (const Column& column : columns) {
        const float x = column_x(column) + kCellPadding;
        const float width = column.width - kCellPadding * 2.0f;
        for (size_t r = span.first; r < span.last && r < column.cells.size(); ++r) {
            const float y = row_y(r) + kCellTextY;
            // NULL stays on the left even in a numeric column: it is a word
            // rather than a quantity, and lining it up with the digits above it
            // would invite reading it as one of them.
            const bool null = column.nulls[r];
            draw_text(target, dwrite, font, column.cells[r], x, y, width,
                      null ? palette.muted.Get() : palette.ink.Get(),
                      column.numeric && !null);
        }
    }
    target->PopAxisAlignedClip();

    // Last, and over the data rather than beside it. The gutter is not taken out
    // of the row width — a bar that reserved twelve DIPs would narrow every row
    // in the result to say something about the four hundred of them that are not
    // on screen. Over the top, at four percent, it costs the trailing edge of the
    // widest column a tint and nothing else.
    const float inset = (kScrollbarGutter - kScrollbarThumb) / 2.0f;
    Scrollbar bar;
    if (scrollbar_of(false, view, &bar)) {
        const float x = size.width - kScrollbarGutter;
        target->FillRectangle(
            D2D1::RectF(x, bar.track_start, size.width, bar.track_start + bar.track_length),
            palette.scroll_track.Get());
        // Centred in the gutter it is grabbed by, which is what makes the target
        // wider than the paint without making the paint look misplaced.
        target->FillRectangle(
            D2D1::RectF(x + inset, bar.thumb_start, x + inset + kScrollbarThumb,
                        bar.thumb_start + bar.thumb_length),
            dragging.vertical ? palette.scroll_thumb_active.Get() : palette.scroll_thumb.Get());
    }
    if (scrollbar_of(true, view, &bar)) {
        const float y = size.height - kScrollbarGutter;
        target->FillRectangle(
            D2D1::RectF(bar.track_start, y, bar.track_start + bar.track_length, size.height),
            palette.scroll_track.Get());
        target->FillRectangle(
            D2D1::RectF(bar.thumb_start, y + inset, bar.thumb_start + bar.thumb_length,
                        y + inset + kScrollbarThumb),
            dragging.horizontal ? palette.scroll_thumb_active.Get() : palette.scroll_thumb.Get());
    }
}

// A connection, one result, and nothing left open. `read_columns` copies every
// value it wants into `Column`, so the handle has no reason to outlive it, and a
// window that held a live DuckDB connection for as long as it held a frame is a
// shape worth not starting.
//
// Which is also why a sort comes back through here rather than reaching for a
// connection somebody kept: sorting is a new statement, and a new statement is a
// connection, a result and a close. On this fixture that costs a few
// milliseconds against an in-memory database; a real client would be reusing a
// pooled connection, and nothing above this line would change.
bool load_grid(const std::string& order, const std::string& where, std::vector<Column>* out) {
    char* err = nullptr;
    DbHandle* handle = db_connect("duckdb://:memory:", nullptr, 10, &err);
    if (handle == nullptr) {
        return core_failed("db_connect", err);
    }
    const bool read = read_columns(handle, order, where, out);
    db_free(handle);
    return read;
}

bool the_grid_draws_a_result() {
    std::vector<Column> columns;
    if (!load_grid(std::string(), std::string(), &columns)) {
        return false;
    }

    check(columns.size() == 4, "the result has four columns");
    if (columns.size() != 4) {
        return false;
    }
    const size_t rows = columns[0].cells.size();
    check(rows == 40 && columns[2].nulls[1], "the second row of the third column is null");
    check(columns[2].cells[1] == kNullText, "a null cell holds the word NULL");

    Surface surface;
    if (!surface.open()) {
        return false;
    }
    Monospace font;
    if (!font.open(surface.dwrite)) {
        return false;
    }
    lay_out(&columns, font.advance);

    check(columns[0].x == 0.0f, "the first column starts at the left edge");
    bool rising = true;
    for (size_t c = 1; c < columns.size(); ++c) {
        if (!(columns[c].x > columns[c - 1].x)) {
            rising = false;
        }
    }
    check(rising, "each column starts to the right of the one before it");

    check(columns[0].numeric, "the id column is read as a number");
    check(!columns[1].numeric, "the name column is not");

    Palette palette;
    if (!palette.open(surface.target.Get(), kLightTones)) {
        return false;
    }

    surface.target->BeginDraw();
    draw_grid(surface.target.Get(), surface.dwrite.Get(), font, columns, palette, nullptr, nullptr,
              0.0f, 0.0f, Bars{});
    HRESULT hr = surface.target->EndDraw();
    if (FAILED(hr)) {
        return failed("ID2D1RenderTarget::EndDraw", hr);
    }

    const float right = columns.back().x + columns.back().width;

    // The bitmap as the geometry sees it: the size it was made at, the columns
    // laid out into it, and a scroll of nothing. Every question below about what
    // is on screen is asked of one of these rather than of loose numbers,
    // because the answers on the two axes depend on each other.
    const View view{static_cast<float>(kWidth), static_cast<float>(kHeight),
                    content_width_of(columns), rows,
                    0.0f,                      0.0f};
    const auto scrolled = [&view](float scroll_row, float scroll_x) {
        View moved = view;
        moved.scroll_row = scroll_row;
        moved.scroll_x = scroll_x;
        return moved;
    };

    // ------------------------------------------------------------------
    // The chrome, asked of the pixels rather than of the code that drew it
    // ------------------------------------------------------------------

    // Compared against the tone `Theme.swift` names, not merely against the
    // canvas. A header band drawn in some other pale colour differs from white
    // and would satisfy anything weaker, while being the exact thing that makes
    // one front end not look like the other.
    BYTE band[3] = {};
    if (!surface.pixel_at(4.0f, kHeaderHeight - 8.0f, band)) {
        return failed("reading the header band", E_FAIL);
    }
    check(band[2] == ((kLightTones.header_band >> 16) & 0xFF)
              && band[1] == ((kLightTones.header_band >> 8) & 0xFF)
              && band[0] == (kLightTones.header_band & 0xFF),
          "the header band is the tone Theme.swift names");

    BYTE under_header[3] = {};
    BYTE canvas[3] = {};
    if (!surface.pixel_at(4.0f, kHeaderHeight - 1.0f, under_header)
        || !surface.pixel_at(4.0f, kHeaderHeight + 4.0f, canvas)) {
        return failed("reading the rule under the header", E_FAIL);
    }
    check(under_header[2] < band[2], "a rule closes the header band");
    check(canvas[2] == 0xFF, "the first row sits on the canvas, not on the band");

    // Sampled past the last column, where no glyph can reach: banding runs the
    // width of the view, and a check taken inside a cell would be answering
    // about the text on top of it.
    BYTE unbanded[3] = {};
    BYTE banded[3] = {};
    if (!surface.pixel_at(right + 20.0f, kHeaderHeight + 10.0f, unbanded)
        || !surface.pixel_at(right + 20.0f, kHeaderHeight + kRowHeight + 10.0f, banded)) {
        return failed("reading the banding", E_FAIL);
    }
    check(banded[2] < unbanded[2], "every other row is banded");

    // On the boundary, and the pixel beside it, because a separator drawn at the
    // wrong x is still a separator somewhere.
    BYTE rule[3] = {};
    BYTE beside[3] = {};
    if (!surface.pixel_at(columns[0].width, kHeaderHeight + 10.0f, rule)
        || !surface.pixel_at(columns[0].width + 3.0f, kHeaderHeight + 10.0f, beside)) {
        return failed("reading a column separator", E_FAIL);
    }
    check(rule[2] < beside[2], "a separator stands on the column boundary");
    UINT header_ink = 0;
    if (!surface.ink_in(0.0f, 0.0f, right, kHeaderHeight, &header_ink)) {
        return failed("reading the header band", E_FAIL);
    }
    check(header_ink > 0, "the headings reached the bitmap");

    // Per row, not for the block: every row drawn on top of the first puts ink
    // in the block and leaves all the rest of these at zero, which is the
    // mistake a first grid actually makes.
    //
    // Over the rows the view can hold rather than over the result, which is
    // longer than the bitmap. Asking about a row that was never on screen would
    // fail a grid that is drawing exactly what it should. The last of them is
    // left out too: the span deliberately reaches one row past the bottom edge,
    // and a row with only its first few pixels on the bitmap is a question
    // about the clamp in `ink_in` rather than about the grid.
    const RowSpan visible = visible_rows(view);
    bool every_row = true;
    for (size_t r = visible.first; r + 1 < visible.last; ++r) {
        const float top = kHeaderHeight + static_cast<float>(r) * kRowHeight;
        UINT row_ink = 0;
        if (!surface.ink_in(0.0f, top, right, top + kRowHeight, &row_ink) || row_ink == 0) {
            every_row = false;
        }
    }
    check(every_row, "every row reached its own band");

    // Same question per column. A grid that ignored `column.x` would draw all
    // three on top of each other at the left edge and pass every check above.
    bool every_column = true;
    for (const Column& column : columns) {
        UINT column_ink = 0;
        if (!surface.ink_in(column.x, kHeaderHeight, column.x + column.width,
                            kHeaderHeight + 3.0f * kRowHeight, &column_ink)
            || column_ink == 0) {
            every_column = false;
        }
    }
    check(every_column, "every column reached its own band");

    UINT null_ink = 0;
    const float null_top = kHeaderHeight + kRowHeight;
    if (!surface.ink_in(columns[2].x, null_top, columns[2].x + columns[2].width,
                        null_top + kRowHeight, &null_ink)) {
        return failed("reading the null cell", E_FAIL);
    }
    check(null_ink > 0, "the word NULL was drawn rather than left blank");

    // And in the right tone. Drawing NULL with the value's brush puts the same
    // pixels in the same cell, so the check above cannot see it; what separates
    // them is that `Grid.nullText` never gets as dark as `Grid.text`.
    BYTE null_darkest = 0xFF;
    BYTE value_darkest = 0xFF;
    if (!surface.darkest_in(columns[2].x, null_top, columns[2].x + columns[2].width,
                            null_top + kRowHeight, &null_darkest)
        || !surface.darkest_in(columns[2].x, kHeaderHeight, columns[2].x + columns[2].width,
                               kHeaderHeight + kRowHeight, &value_darkest)) {
        return failed("comparing the null cell with a value", E_FAIL);
    }
    check(null_darkest > value_darkest, "NULL is dimmer than the value above it");

    // A number is read down its column by comparing magnitudes, which only works
    // if the units line up — so the digits sit at the trailing edge and nothing
    // is drawn in the half of the cell they left behind. The text column beside
    // it answers the other way, which is what makes this about alignment rather
    // than about one cell happening to be short.
    UINT number_left = 0;
    UINT number_right = 0;
    UINT word_left = 0;
    const float first_row = kHeaderHeight;
    const float middle = columns[0].x + columns[0].width / 2.0f;
    if (!surface.ink_in(columns[0].x, first_row, middle, first_row + kRowHeight, &number_left)
        || !surface.ink_in(middle, first_row, columns[0].x + columns[0].width,
                           first_row + kRowHeight, &number_right)
        || !surface.ink_in(columns[1].x, first_row, columns[1].x + columns[1].width / 2.0f,
                           first_row + kRowHeight, &word_left)) {
        return failed("reading the halves of a cell", E_FAIL);
    }
    check(number_left == 0 && number_right > 0, "a number is drawn against the trailing edge");
    check(word_left > 0, "a word is not");

    // A value the column cannot hold. Asked of the layout the grid draws with
    // rather than of the bitmap, because neither question has a pixel answer:
    // a second line is clipped by the row rather than reported, and a line that
    // stopped early looks the same as one that was trimmed.
    const Column& wide = columns.back();
    check(wide.width == kMaxColumnWidth, "a wide column is held to the maximum");
    ComPtr<IDWriteTextLayout> overrun;
    if (!make_layout(surface.dwrite.Get(), font, wide.cells[0],
                     wide.width - kCellPadding * 2.0f, false, &overrun)) {
        return false;
    }
    DWRITE_LINE_METRICS line{};
    UINT32 lines = 0;
    // E_NOT_SUFFICIENT_BUFFER when there is more than one line, and it fills in
    // the count either way — which is the case being ruled out.
    hr = overrun->GetLineMetrics(&line, 1, &lines);
    check(lines == 1, "a value too wide for its column stays on one line");
    check(SUCCEEDED(hr) && line.isTrimmed, "and is cut rather than carried over");

    // And the cut reaches the edge. Wrapping breaks at the last space that
    // fitted, which leaves the tail of the cell empty; trimming fills it and
    // puts the ellipsis there. That is the difference between a value the user
    // can see was shortened and one that just ends.
    UINT tail_ink = 0;
    const float tail = wide.x + wide.width - kCellPadding;
    if (!surface.ink_in(tail - 3.0f * font.advance, kHeaderHeight, tail,
                        kHeaderHeight + kRowHeight, &tail_ink)) {
        return failed("reading the trailing edge of a trimmed cell", E_FAIL);
    }
    check(tail_ink > 0, "the cut value reaches the cell's trailing edge");

    // And stops there. `DrawTextLayout` does not clip to the box it was given —
    // a line that no longer wraps and is not trimmed is simply drawn past the
    // end of it, over whatever column comes next. Here that is empty canvas,
    // which is why this is asked of the last column: the damage is visible
    // without a neighbour having to be sacrificed to show it.
    UINT spill = 0;
    if (!surface.ink_in(tail + 2.0f, kHeaderHeight, static_cast<float>(kWidth),
                        kHeaderHeight + kRowHeight, &spill)) {
        return failed("reading past a trimmed cell", E_FAIL);
    }
    check(spill == 0, "and nothing is drawn past it");

    // ------------------------------------------------------------------
    // The selection: where a click lands, and what it looks like once it has
    // ------------------------------------------------------------------

    Selection under;
    check(cell_at(columns[2].x + 4.0f, kHeaderHeight + kRowHeight + 4.0f, view, columns, &under)
              && under.row == 1 && under.column == 2,
          "a point inside a cell finds that cell");
    check(!cell_at(columns[2].x + 4.0f, 4.0f, view, columns, &under),
          "a point on the header finds none");
    check(!cell_at(right + 4.0f, kHeaderHeight + 4.0f, view, columns, &under),
          "a point past the last column finds none");
    check(!cell_at(4.0f, kHeaderHeight + static_cast<float>(rows) * kRowHeight + 4.0f, view,
                   columns, &under),
          "a point below the last row finds none");

    // The same pixel, from the drawing that had no selection in it. Row 1 is an
    // odd row and therefore already banded, so this is the only honest control
    // for the tint: a reading taken from an unbanded row would say the selection
    // was visible when all that had been measured was the banding.
    const float selected_y = kHeaderHeight + kRowHeight + 10.0f;
    BYTE unselected[3] = {};
    if (!surface.pixel_at(right + 20.0f, selected_y, unselected)) {
        return failed("reading the row before it is selected", E_FAIL);
    }

    Selection selection;
    selection.row = 1;
    selection.column = 2;
    surface.target->BeginDraw();
    draw_grid(surface.target.Get(), surface.dwrite.Get(), font, columns, palette, &selection,
              nullptr, 0.0f, 0.0f, Bars{});
    hr = surface.target->EndDraw();
    if (FAILED(hr)) {
        return failed("ID2D1RenderTarget::EndDraw with a selection", hr);
    }

    const float unselected_y = kHeaderHeight + 2.0f * kRowHeight + 10.0f;
    BYTE row_band[3] = {};
    BYTE elsewhere[3] = {};
    BYTE in_cell[3] = {};
    // Past the last column for the two row samples, where no glyph can reach,
    // and inside the cursor cell but clear of the four letters of NULL.
    if (!surface.pixel_at(right + 20.0f, selected_y, row_band)
        || !surface.pixel_at(right + 20.0f, unselected_y, elsewhere)
        || !surface.pixel_at(columns[2].x + columns[2].width - 4.0f, selected_y, in_cell)) {
        return failed("reading the selection", E_FAIL);
    }
    // Two questions, because either alone is answered by the banding that is
    // already on this row. It darkened: red fell from 247 to 216, and it can
    // only fall, since every fill here is translucent over white. And what
    // darkened it was the accent rather than more of the same grey: the accent
    // spends far more of the red than of the blue, so the gap between those two
    // channels opened from 1 to 28, while the rule tone keeps them level.
    check(row_band[2] < unselected[2]
              && row_band[0] - row_band[2] > unselected[0] - unselected[2],
          "the selected row is tinted with the accent");
    check(elsewhere[0] == elsewhere[2], "and the row below it is not");
    check(in_cell[2] < row_band[2], "the cursor cell is stronger than the rest of its row");

    // Half a DIP in, which is the middle of the one-DIP line rather than its
    // left edge. A column is a whole number of characters wide and a character
    // is 6.59765625 DIPs, so a column boundary almost never lands on a pixel
    // boundary: this one is at 136.16, and the pixel `columns[2].x` names holds
    // sixteen percent of the line and eighty-four percent of the column before
    // it.
    // The middle of the line is the only point that is inside it whatever the
    // fraction turns out to be.
    BYTE edge[3] = {};
    if (!surface.pixel_at(columns[2].x + 0.5f, selected_y, edge)) {
        return failed("reading the cursor edge", E_FAIL);
    }
    // Measured there, the pixel is 74/67/214 — the light accent at 79/70/229 with
    // the eight percent of the separator drawn over it, the order
    // `GridRenderer.swift` uses. The cursor cell beside it is 164/161/238 and
    // the rest of the selected row is 216/216/244.
    //
    // Asked as that descent rather than against the accent's own value, because
    // what the edge is for is being stronger than the wash around it: a check
    // written against a constant would go on passing if both fills were made
    // twice as heavy and the edge stopped standing out at all.
    check(edge[2] < in_cell[2] && edge[0] > edge[2],
          "a full-strength edge marks the cursor's leading side");

    // The value survives being selected. Filling over the text instead of under
    // it leaves the cell tinted and empty, and every check above would still
    // pass — the fills are what they are asking about.
    UINT selected_ink = 0;
    if (!surface.ink_in(columns[2].x, kHeaderHeight + kRowHeight, columns[2].x + columns[2].width,
                        kHeaderHeight + 2.0f * kRowHeight, &selected_ink)) {
        return failed("reading a selected cell", E_FAIL);
    }
    check(selected_ink > 0, "and the value under it is still drawn");

    // ------------------------------------------------------------------
    // The keyboard: how far each key moves, and what becomes of the band
    // ------------------------------------------------------------------

    // Thirty whole rows fit in this bitmap, so a page is twenty-nine of them.
    check(page_rows(view) == 29, "a page is every whole row the view holds, less one");
    View cramped = view;
    cramped.height = kHeaderHeight + kRowHeight;
    check(page_rows(cramped) == 1, "and is a row even in a view that holds one");

    const int last_row = static_cast<int>(rows) - 1;
    const int last_column = static_cast<int>(columns.size()) - 1;
    // One press. A key the grid does not use leaves the default selection,
    // which is the top left cell and answers no case below with what it asked.
    const auto pressed = [&](WPARAM key, const Selection& from, bool extend) {
        Selection next;
        key_moves(key, from, view, extend, last_row, last_column, &next);
        return next;
    };

    const Selection home;
    check(pressed(VK_DOWN, home, false).row == 1, "an arrow moves one row");
    check(pressed(VK_NEXT, home, false).row == 29, "and a page key moves a page");
    // Twice down from the top is past the end of forty rows. The key stopping
    // at the last row rather than doing nothing is what makes a second press
    // the way to the end of a result.
    check(pressed(VK_NEXT, Selection{29, 0}, false).row == last_row,
          "a page past the end stops at the last row");
    check(pressed(VK_PRIOR, Selection{last_row, 0}, false).row == 10,
          "and a page back from there is a page back");
    check(pressed(VK_UP, home, false).row == 0, "a key at the edge does nothing");
    check(pressed(VK_RIGHT, Selection{0, last_column}, false).column == last_column,
          "sideways too");

    // Home and End are the two keys whose whole purpose is the row they name,
    // and the column has to survive them: a user who has arrowed across to the
    // fourth column and presses End is asking about that column at the end of
    // the result, not about the first one.
    const Selection third_column{20, 2};
    check(pressed(VK_HOME, third_column, false).row == 0
              && pressed(VK_HOME, third_column, false).column == 2,
          "Home is the first row of the column the cursor is in");
    check(pressed(VK_END, third_column, false).row == last_row
              && pressed(VK_END, third_column, false).column == 2,
          "and End is the last row of it");

    // The band, which is what the shift key is for. Anchored where the cursor
    // was rather than where it lands, so the rows between the two are the ones
    // that were crossed.
    const Selection extended = pressed(VK_END, Selection{3, 0}, true);
    check(extended.anchored && extended.first_row() == 3 && extended.last_row() == last_row,
          "a shift-held key opens a band behind the cursor");
    // A second shift-held key moves the cursor and leaves the anchor where it
    // was, so a band can be taken back as well as out: a page up from the end
    // of the result gives up the rows at that end, not the rows at row three.
    // Re-taking the anchor on every press would put the band at ten to thirty
    // nine instead — a band that is only ever one key long, which is the shape
    // this reads as working in until somebody presses twice.
    const Selection grown = pressed(VK_PRIOR, extended, true);
    check(grown.anchored && grown.first_row() == 3 && grown.last_row() == 10,
          "and a second one moves the cursor end rather than the anchor");
    check(!pressed(VK_HOME, extended, false).anchored, "an unshifted key drops it");
    check(!pressed(VK_RIGHT, extended, true).anchored, "and so does a sideways one, shift or not");

    // A key the grid has nothing to do with is refused rather than answered
    // with the cursor where it already was. The window has to be able to tell
    // the two apart: one of them is a keystroke it should leave alone.
    Selection untouched;
    check(!key_moves(VK_TAB, third_column, view, false, last_row, last_column, &untouched),
          "a key the grid does not use gets no answer");

    // Ctrl+A. Every row, of the one column the cursor is in — the band is a
    // range of rows whatever else is true, so there is no "all columns" for it
    // to mean.
    const Selection everything = all_rows(third_column, last_row);
    check(everything.first_row() == 0 && everything.last_row() == last_row,
          "selecting all takes in every row of the result");
    check(everything.column == 2, "and stays in the column the cursor was in");
    // The cursor at the far end rather than at the near one. Both cover the
    // same rows; only this one leaves shift-up somewhere to go.
    check(everything.row == last_row && everything.anchor == 0,
          "with the cursor at the end it can be walked back from");

    // ------------------------------------------------------------------
    // The clipboard: what a copy would paste
    // ------------------------------------------------------------------

    check(clipboard_text(columns, Selection{2, 1}) == L"driver-2",
          "one cell copies as the value in it");
    // The word is how the grid says "there is nothing here"; the clipboard has
    // its own way of saying that, and pasting the word would put a
    // four-character string into a column of numbers.
    check(clipboard_text(columns, Selection{1, 2}).empty(), "and a null cell copies as nothing");

    // Rows one and two, which is the band a shift-arrow from row two leaves.
    const std::wstring rows_text = clipboard_text(columns, Selection{2, 0, true, 1});
    std::wstring names = columns[0].heading;
    for (size_t c = 1; c < columns.size(); ++c) {
        names += L'\t';
        names += columns[c].heading;
    }
    check(rows_text.rfind(names + L"\r\n", 0) == 0, "several rows copy under a line of column names");
    // Two line endings for two rows: the names are the first line rather than a
    // line of their own with a blank one after it.
    size_t endings = 0;
    for (size_t at = rows_text.find(L"\r\n"); at != std::wstring::npos; at = rows_text.find(L"\r\n", at + 1)) {
        endings += 1;
    }
    check(endings == 2, "one line each and no line for the names to sit above");
    // The null field is empty between its two tabs rather than absent: a row
    // that dropped it would paste every column after it one place to the left.
    check(rows_text.find(L"\r\n1\tdriver-1\t\ta value") != std::wstring::npos,
          "and a null inside a row is an empty field rather than a missing one");

    // The two characters the format is made of, in the one place they cannot be
    // left alone. Asked of a table of literals, because the query above has no
    // value with a tab in it and giving it one would change every width the
    // checks above measure.
    std::vector<Column> unruly(2);
    unruly[0].heading = L"a";
    unruly[0].cells = {L"one\ttwo", L"three\r\nfour"};
    unruly[0].nulls = {false, false};
    unruly[1].heading = L"b";
    unruly[1].cells = {L"x", L"y"};
    unruly[1].nulls = {false, false};
    check(clipboard_text(unruly, Selection{1, 0, true, 0})
              == L"a\tb\r\none two\tx\r\nthree  four\ty",
          "a tab or a newline inside a value becomes a space");
    // Not in a single-cell copy, where there is no format to break. A value is
    // pasted as the value; it is the rows around it that make the tab mean
    // something else.
    check(clipboard_text(unruly, Selection{0, 0}) == L"one\ttwo",
          "and is left alone where the value is the whole of what was copied");

    // The Win32 half, round-tripped. Nothing else can say that the block was
    // allocated moveable, that the terminator went into it, or that the
    // clipboard still held it after the handle was handed over — every one of
    // those is invisible until somebody pastes.
    const std::wstring posted = clipboard_text(columns, Selection{2, 0, true, 1});
    check(put_on_clipboard(nullptr, posted), "the clipboard takes the copy");
    std::wstring pasted;
    if (OpenClipboard(nullptr) != 0) {
        HANDLE held = GetClipboardData(CF_UNICODETEXT);
        const void* read = held != nullptr ? GlobalLock(held) : nullptr;
        if (read != nullptr) {
            pasted.assign(static_cast<const wchar_t*>(read));
            GlobalUnlock(held);
        }
        CloseClipboard();
    }
    check(pasted == posted, "and reads back as the text that was put there");

    // And that a copy takes the clipboard rather than joining what is on it.
    // The set succeeds either way — that is why this is asked rather than
    // assumed — but a format the last owner left stays behind, and a program
    // that prefers that format pastes the previous copy.
    //
    // Asked with a format of this check's own: Windows synthesises `CF_TEXT` and
    // `CF_OEMTEXT` from the Unicode above, so either of those would answer yes
    // whatever happened here.
    const UINT marker = RegisterClipboardFormatW(L"dbclient.verify-grid");
    if (marker != 0 && OpenClipboard(nullptr) != 0) {
        EmptyClipboard();
        HGLOBAL mark = GlobalAlloc(GMEM_MOVEABLE, sizeof(wchar_t));
        void* into = mark != nullptr ? GlobalLock(mark) : nullptr;
        if (into != nullptr) {
            *static_cast<wchar_t*>(into) = L'\0';
            GlobalUnlock(mark);
            if (SetClipboardData(marker, mark) == nullptr) {
                GlobalFree(mark);
            }
        }
        CloseClipboard();
    }
    check(marker != 0 && IsClipboardFormatAvailable(marker) != 0,
          "a format left by somebody else is on the clipboard");
    check(put_on_clipboard(nullptr, posted) && IsClipboardFormatAvailable(marker) == 0,
          "and a copy empties it rather than settling in beside it");

    // ------------------------------------------------------------------
    // The menu: where it opens, and the renderings it offers by name
    // ------------------------------------------------------------------

    check(row_count_phrase(1) == L"1 row" && row_count_phrase(3) == L"3 rows",
          "the items count the rows they would copy, in the plural they need");

    // Named items have no guessing to do, so this one says rows even where
    // there is one of them: the item that copies a value is the one above it.
    check(tab_separated(columns, 2, 2) == names + L"\r\n2\tdriver-2\tread 200\t"
                                              + columns[3].cells[2],
          "copying one row still copies it under the names");

    std::vector<Column> quoting(2);
    quoting[0].heading = L"a,b";
    quoting[0].cells = {L"x, y", L"he said \"no\""};
    quoting[0].nulls = {false, false};
    quoting[1].heading = L"plain";
    quoting[1].cells = {L"1", L""};
    quoting[1].nulls = {false, true};
    check(csv_rows(quoting, 0, 1) == L"\"a,b\",plain\r\n\"x, y\",1\r\n\"he said \"\"no\"\"\",",
          "CSV quotes a field that holds a comma or a quote, and doubles the quote");
    // The other half of that rule, and the difference from the rendering above:
    // a tab is an ordinary character in CSV and is left where it is, while a
    // line ending is quoted rather than replaced. Nothing is lost either way,
    // which is the point of offering both.
    check(csv_rows(unruly, 0, 1) == L"a,b\r\none\ttwo,x\r\n\"three\r\nfour\",y",
          "and quotes a line ending rather than flattening it");

    // Where the menu may open. The gutter refuses because both bars are drawn
    // over the data; the header refuses through `cell_at`.
    Selection opened;
    check(pointed_cell(20.0f, kHeaderHeight + 4.0f, view, columns, nullptr, false, &opened)
              && opened.row == 0 && opened.column == 0,
          "a right-click on a cell opens the menu on that cell");
    // Asked of the horizontal bar, and that is the whole of why this view is
    // narrowed: this result is narrower than the bitmap, so the vertical
    // gutter sits past the last column where `cell_at` refuses anyway, and a
    // check written there passes with the guard taken out. The bar along the
    // foot is the one drawn over a cell.
    View crossing = view;
    crossing.width = 200.0f;
    check(!pointed_cell(20.0f, crossing.height - 2.0f, crossing, columns, nullptr, false, &opened),
          "one in the scrollbar's gutter opens none");
    check(!pointed_cell(20.0f, 4.0f, view, columns, nullptr, false, &opened),
          "and one on a heading opens none");

    const Selection standing{3, 1, true, 1};
    check(pointed_cell(20.0f, kHeaderHeight + 5.0f * kRowHeight + 4.0f, view, columns, &standing,
                       true, &opened)
              && opened.first_row() == 1 && opened.last_row() == 5,
          "shift extends the band from where it already starts");
    check(pointed_cell(20.0f, kHeaderHeight + 5.0f * kRowHeight + 4.0f, view, columns, &standing,
                       false, &opened)
              && !opened.anchored && opened.row == 5,
          "and without it the menu acts on the one cell under the pointer");

    // ------------------------------------------------------------------
    // Editing: what a box opens with, what a commit leaves, and its mark
    // ------------------------------------------------------------------

    check(edit_seed(columns, Selection{2, 1}) == L"driver-2",
          "a box opens with the value that was in the cell");
    // Row 1's note is the null one. The word is what the grid draws over an
    // absent value, not the value: seeding the box with it would have the next
    // keystroke appended to a label.
    check(edit_seed(columns, Selection{1, 2}).empty(),
          "and opens empty over a null rather than on the word");

    std::vector<Column> staged = columns;
    stage_edit(&staged, Selection{1, 2}, L"read 0");
    check(staged[2].cells[1] == L"read 0", "a commit writes the value into the grid's own copy");
    check(!staged[2].nulls[1], "over the null that was there, which stops being one");
    check(staged[2].edited[1], "and marks that cell");
    check(!staged[2].edited[0] && !staged[1].edited[1],
          "leaving the cells around it unmarked and unchanged");
    // An empty box is an empty string. There is no key here that means "absent",
    // and making the absence of typing mean it would leave no way to enter the
    // empty string at all.
    stage_edit(&staged, Selection{2, 1}, std::wstring());
    check(staged[1].cells[2].empty() && !staged[1].nulls[2],
          "typing nothing leaves an empty value rather than a null");

    int onwards = 0;
    check(next_edit_column(1, 1, last_column, &onwards) && onwards == 2,
          "Tab opens the next column along");
    check(next_edit_column(1, -1, last_column, &onwards) && onwards == 0,
          "and shift-Tab the one before");
    // Neither edge wraps. A Tab that came back on the next row would put the
    // next thing typed into a row nobody was looking at.
    check(!next_edit_column(last_column, 1, last_column, &onwards),
          "Tab out of the last column opens nothing");
    check(!next_edit_column(0, -1, last_column, &onwards),
          "and neither does shift-Tab out of the first");

    // The mark, drawn. Row 2 of the note column: an even row, so there is no
    // banding under it to be mistaken for the mark, and the sample is taken at
    // the far edge of the column where no glyph reaches.
    std::vector<Column> with_mark = columns;
    stage_edit(&with_mark, Selection{2, 2}, L"read 0");
    const float marked_x = columns[2].x + columns[2].width - 4.0f;
    const float marked_y = kHeaderHeight + 2.0f * kRowHeight + 10.0f;
    const float beside_x = columns[1].x + columns[1].width - 4.0f;
    BYTE before_mark[3] = {};
    BYTE after_mark[3] = {};
    BYTE beside_mark[3] = {};
    surface.target->BeginDraw();
    draw_grid(surface.target.Get(), surface.dwrite.Get(), font, columns, palette, nullptr, nullptr,
              0.0f, 0.0f, Bars{});
    hr = surface.target->EndDraw();
    if (FAILED(hr) || !surface.pixel_at(marked_x, marked_y, before_mark)) {
        return failed("reading a cell before it is marked", E_FAIL);
    }
    surface.target->BeginDraw();
    draw_grid(surface.target.Get(), surface.dwrite.Get(), font, with_mark, palette, nullptr,
              nullptr, 0.0f, 0.0f, Bars{});
    hr = surface.target->EndDraw();
    if (FAILED(hr) || !surface.pixel_at(marked_x, marked_y, after_mark)
        || !surface.pixel_at(beside_x, marked_y, beside_mark)) {
        return failed("reading a marked cell", E_FAIL);
    }
    // Two questions again. It darkened — every fill here is translucent over
    // white, so it can only darken — and what darkened it was the warning tone
    // rather than the accent or the rule: warning is the one colour in the table
    // that spends more blue than red, so red ends up above blue where those two
    // leave them level or the other way round.
    check(after_mark[2] < before_mark[2], "a cell somebody has typed into is filled behind");
    check(after_mark[2] > after_mark[0] + 30,
          "in the tone that reports rather than the one that selects");
    // The cell, not the row. A mark that spread across the row would say four
    // values had been changed when one had.
    check(beside_mark[0] == beside_mark[2], "and only that cell carries it");

    // And that it survives the selection, which is where it will nearly always
    // be seen: the cell somebody has just typed into is the cell the cursor is
    // on. Drawn over the row's wash and under the cursor's own fill, so both
    // still read as what they are.
    const Selection on_mark{2, 2};
    BYTE plain_cursor[3] = {};
    BYTE marked_cursor[3] = {};
    surface.target->BeginDraw();
    draw_grid(surface.target.Get(), surface.dwrite.Get(), font, columns, palette, &on_mark, nullptr,
              0.0f, 0.0f, Bars{});
    hr = surface.target->EndDraw();
    if (FAILED(hr) || !surface.pixel_at(marked_x, marked_y, plain_cursor)) {
        return failed("reading the cursor cell before it is marked", E_FAIL);
    }
    surface.target->BeginDraw();
    draw_grid(surface.target.Get(), surface.dwrite.Get(), font, with_mark, palette, &on_mark,
              nullptr, 0.0f, 0.0f, Bars{});
    hr = surface.target->EndDraw();
    if (FAILED(hr) || !surface.pixel_at(marked_x, marked_y, marked_cursor)) {
        return failed("reading the cursor cell once marked", E_FAIL);
    }
    // The accent leaves blue above red; the mark moves that gap the other way.
    // Asked as the move rather than as a value, because what matters is that the
    // mark is still doing something under a fill drawn after it.
    check(marked_cursor[2] - marked_cursor[0] > plain_cursor[2] - plain_cursor[0],
          "and the mark still shows under the cursor sitting on it");
    // The value is still readable through both of them.
    UINT marked_ink = 0;
    if (!surface.ink_in(columns[2].x, kHeaderHeight + 2.0f * kRowHeight,
                        columns[2].x + columns[2].width, kHeaderHeight + 3.0f * kRowHeight,
                        &marked_ink)) {
        return failed("reading a marked cell's value", E_FAIL);
    }
    check(marked_ink > 0, "with the value under the two of them still drawn");

    // ------------------------------------------------------------------
    // The filter submenu: the SQL each item means, and what it leaves
    // ------------------------------------------------------------------

    check(cell_predicate(columns[1], 2, Filter::Equals) == "\"name\" = 'driver-2'",
          "Equals This Value names the column and quotes the value");
    check(cell_predicate(columns[0], 3, Filter::NotEquals) == "\"id\" <> '3'",
          "and its opposite is the same clause the other way round");
    // Row 1's note is the null one. `= NULL` is never true, so the item would
    // empty the grid every time it was chosen on an absent value — a fact about
    // three-valued logic reported as a filter.
    check(cell_predicate(columns[2], 1, Filter::Equals) == "\"note\" IS NULL",
          "on an absent value it asks IS NULL rather than = NULL");
    check(cell_predicate(columns[2], 1, Filter::NotEquals) == "\"note\" IS NOT NULL",
          "and the opposite item mirrors that");
    // The two NULL items ask about the column whatever the cell holds, which is
    // what tells them apart from the two above.
    check(cell_predicate(columns[2], 2, Filter::IsNull) == "\"note\" IS NULL"
              && cell_predicate(columns[2], 2, Filter::IsNotNull) == "\"note\" IS NOT NULL",
          "the two NULL items ignore the value under the pointer");
    // This fixture's fourth column is the one that makes quoting compulsory
    // rather than careful: its name is a sentence with spaces in it.
    check(cell_predicate(columns[3], 0, Filter::IsNull)
              == "\"a heading long enough to be cut by the column it names\" IS NULL",
          "a column named with spaces in it survives as one name");
    // And the other quote, which is the one a value can carry. Asked of a column
    // made here, because nothing the fixture query returns has an apostrophe in
    // it and giving one a value that does would change the widths every check
    // above measures.
    Column apostrophe;
    apostrophe.name = "it's";
    apostrophe.cells = {L"O'Hara"};
    apostrophe.nulls = {false};
    check(cell_predicate(apostrophe, 0, Filter::Equals) == "\"it's\" = 'O''Hara'",
          "a quote inside a value is doubled rather than ending the literal");

    check(where_clause(std::vector<std::string>()).empty(), "no filters is no clause at all");
    check(where_clause({"a", "b"}) == "a AND b",
          "and several are ANDed in the order they were chosen");

    Filter chose = Filter::IsNull;
    bool extending = true;
    check(filter_chosen(static_cast<int>(kFilterFirst), &chose, &extending)
              && chose == Filter::Equals && !extending,
          "the first item of the first submenu is Equals, replacing");
    check(filter_chosen(static_cast<int>(kFilterFirst + kFilterOps + 1), &chose, &extending)
              && chose == Filter::NotEquals && extending,
          "and the second of the second is Does Not Equal, adding");
    // The copy items share the menu with these, and nought is what a dismissed
    // menu answers: either taken for a filter would run one nobody chose.
    check(!filter_chosen(static_cast<int>(kCopyValue), &chose, &extending)
              && !filter_chosen(0, &chose, &extending),
          "a copy item and a dismissed menu are neither of them filters");

    // The ids the submenus really carry, read back off the menus and decoded.
    // The two checks above test the halves separately and would both go on
    // passing if the building and the decoding stopped agreeing: two submenus
    // built with one set of ids read as one menu, and *Add to Filter* would
    // quietly replace instead of narrowing. A menu needs no window, so this can
    // be asked here.
    for (int adding = 0; adding < 2; ++adding) {
        HMENU built = filter_submenu(adding == 1);
        bool agrees = built != nullptr
                      && GetMenuItemCount(built) == static_cast<int>(kFilterOps);
        for (int item = 0; built != nullptr && item < static_cast<int>(kFilterOps); ++item) {
            Filter carried = Filter::IsNull;
            bool extends = adding == 0;
            agrees = agrees
                     && filter_chosen(static_cast<int>(GetMenuItemID(built, item)), &carried,
                                      &extends)
                     && carried == static_cast<Filter>(item) && extends == (adding == 1);
        }
        DestroyMenu(built);
        check(agrees, adding == 1 ? "the adding submenu's four ids decode to its four items"
                                  : "and the replacing submenu's to its own");
    }

    // Against the server, because everything above is about text and none of it
    // says DuckDB will take it.
    std::vector<Column> filtered;
    if (!load_grid(std::string(), cell_predicate(columns[2], 1, Filter::IsNull), &filtered)) {
        return false;
    }
    check(filtered.size() == 4 && filtered[0].cells.size() == 1 && filtered[0].cells[0] == L"1",
          "the clause the menu builds runs, and leaves the row it was built from");
    // The wrap is the risk: the predicates name projections, so the statement
    // puts the whole select inside another one, and a declared type that did not
    // come out through that would take the second header line with it.
    check(filtered[0].type_name == columns[0].type_name
              && filtered[3].type_name == columns[3].type_name,
          "and the declared types come out through the wrap");

    std::vector<Column> narrowed;
    if (!load_grid(std::string(),
                   where_clause({cell_predicate(columns[1], 2, Filter::Equals),
                                 cell_predicate(columns[0], 2, Filter::Equals)}),
                   &narrowed)) {
        return false;
    }
    check(narrowed[0].cells.size() == 1 && narrowed[0].cells[0] == L"2",
          "two of them leave what both are true of");

    // Nothing matched is an answer, not a failure. It is also the one state this
    // menu can put the grid into that it cannot get out of by itself, which is
    // why the window offers Clear Filter on an empty result.
    std::vector<Column> nothing;
    if (!load_grid(std::string(), cell_predicate(columns[0], 0, Filter::IsNull), &nothing)) {
        return false;
    }
    check(nothing.size() == 4 && nothing[0].cells.empty(),
          "a filter that matches nothing keeps the columns and loses the rows");

    // ------------------------------------------------------------------
    // Scrolling: which rows are on screen, and which row each one is
    // ------------------------------------------------------------------

    check(visible_rows(view).first == 0 && visible_rows(scrolled(7.0f, 0.0f)).first == 7,
          "the top row is the one the scroll is counted in");
    // One past the bottom edge, not one short of it. 640 minus the header is
    // 608, which is 30.4 rows: a grid that drew 30 would leave the last two
    // fifths of a row empty at every position that is not a whole number.
    check(visible_rows(view).last == 32,
          "and the span reaches past the bottom edge rather than short of it");
    check(visible_rows(scrolled(38.0f, 0.0f)).last == rows,
          "the span stops at the last row rather than past it");

    // 40 rows less the 30.4 that fit. Scrolling further would put blank canvas
    // under the last row, which is the thing that makes a grid feel like it has
    // lost the result.
    const float most = max_scroll_row(view);
    check(most > 9.5f && most < 9.7f, "the scroll stops where the last row reaches the bottom");

    // The keyboard's half of scrolling, and the reason it is a separate
    // quantity: a row is visible only if all of it is, so the fold is at 30
    // whole rows rather than at 30.4.
    check(scroll_to_visible(view, 5) == 0.0f, "a row already on screen does not move the view");
    check(scroll_to_visible(view, 39) == most, "the last row brings the view to the end");
    check(scroll_to_visible(scrolled(20.0f, 0.0f), 3) == 3.0f,
          "a row above the fold becomes the top row");

    // And the hit test counts in the same rows the drawing does. A grid that
    // scrolled its pixels and not its arithmetic answers every click with the
    // row that used to be there, and the selection lands somewhere the user can
    // see they did not point at.
    check(cell_at(4.0f, kHeaderHeight + 4.0f, scrolled(7.0f, 0.0f), columns, &under)
              && under.row == 7,
          "a click is measured from the scrolled position");

    // Drawn twice more, because the rest of this is about pixels. Row 1 is odd
    // and banded, row 2 is not; scrolling by one row puts the banded row at the
    // top of the view and scrolling by two puts the unbanded one there. Striping
    // by position on screen instead of by row number passes at rest and swaps
    // the stripes on the first notch of the wheel.
    BYTE top_at_one[3] = {};
    BYTE top_at_two[3] = {};
    for (int step = 1; step <= 2; ++step) {
        surface.target->BeginDraw();
        draw_grid(surface.target.Get(), surface.dwrite.Get(), font, columns, palette, nullptr,
                  nullptr, static_cast<float>(step), 0.0f, Bars{});
        hr = surface.target->EndDraw();
        if (FAILED(hr)) {
            return failed("ID2D1RenderTarget::EndDraw scrolled", hr);
        }
        // Past the last column, where no glyph can reach.
        if (!surface.pixel_at(right + 20.0f, kHeaderHeight + 10.0f,
                              step == 1 ? top_at_one : top_at_two)) {
            return failed("reading the top row of a scrolled grid", E_FAIL);
        }
    }
    check(top_at_one[2] < top_at_two[2] && top_at_two[2] == 0xFF,
          "the banding belongs to the row rather than to the place on screen");

    // Half a row up, so the top row is cut by the header band. The band is
    // opaque and drawn over the fills, but the values go on after it, so the
    // row on its way out from under the header is the one thing that can still
    // reach the column names.
    surface.target->BeginDraw();
    draw_grid(surface.target.Get(), surface.dwrite.Get(), font, columns, palette, nullptr, nullptr,
              0.5f, 0.0f, Bars{});
    hr = surface.target->EndDraw();
    if (FAILED(hr)) {
        return failed("ID2D1RenderTarget::EndDraw part-scrolled", hr);
    }
    UINT band_ink = 0;
    if (!surface.ink_in(0.0f, 0.0f, right, kHeaderHeight, &band_ink)) {
        return failed("reading the header band of a part-scrolled grid", E_FAIL);
    }
    // The same count, not merely a small one: the headings are drawn in the same
    // place at every scroll position, so anything the moving rows added to this
    // strip shows up as a difference.
    check(band_ink == header_ink, "a row sliding under the header does not reach the headings");

    // ------------------------------------------------------------------
    // The scrollbar: how much there is, and where in it the view sits
    // ------------------------------------------------------------------

    Scrollbar bar;
    check(scrollbar_of(false, view, &bar), "a result taller than the view gets a bar");
    // Four rows in a view that holds thirty. Nothing is below the fold, and a
    // bar pinned to its full length would be a control that cannot be used
    // saying there is somewhere to go.
    View brief = view;
    brief.rows = 4;
    Scrollbar unneeded;
    check(!scrollbar_of(false, brief, &unneeded), "and a result that fits gets none");

    // 608 of track for 30.4 rows out of 40, which is 462. The thumb is how much
    // of the result is on screen, and reading it is how anyone knows whether
    // they are looking at most of a table or at the first screen of a million.
    check(bar.track_start == kHeaderHeight && bar.track_length == view.height - kHeaderHeight,
          "the track runs from under the header to the foot of the view");
    check(bar.thumb_length > 461.0f && bar.thumb_length < 463.0f,
          "the thumb is as long a part of the track as the view is of the result");
    // A million rows would ask for a thumb a thousandth of the track, which is
    // half a DIP: too small to see and far too small to hit.
    View million = view;
    million.rows = 1000000;
    Scrollbar huge;
    check(scrollbar_of(false, million, &huge) && huge.thumb_length == kMinThumbLength,
          "and never shorter than something that can be grabbed");

    check(bar.thumb_start == bar.track_start, "at rest the thumb is at the top of the track");
    Scrollbar ended;
    check(scrollbar_of(false, scrolled(most, 0.0f), &ended)
              && ended.thumb_start + ended.thumb_length == ended.track_start + ended.track_length,
          "at the end of the scroll its trailing edge is on the track's");

    // The drag is the inverse of the line above, so the two have to agree: a
    // thumb picked up and put down without moving must leave the scroll where
    // it was. They are separate expressions, and it is the round trip that says
    // the second one is the first one backwards.
    Scrollbar midway;
    check(scrollbar_of(false, scrolled(4.0f, 0.0f), &midway)
              && scroll_to_thumb(false, midway.thumb_start, view) > 3.99f
              && scroll_to_thumb(false, midway.thumb_start, view) < 4.01f,
          "dragging the thumb back to where it was leaves the scroll there");
    check(scroll_to_thumb(false, -100.0f, view) == 0.0f
              && scroll_to_thumb(false, 10000.0f, view) == most,
          "and a drag past either end of the track stops at the end of the result");

    // Drawn at both ends, and read where the thumb is not: the track is four
    // percent and the thumb eighteen, so the question is which of the two is at
    // the top of the gutter.
    const float gutter_x = static_cast<float>(kWidth) - kScrollbarGutter / 2.0f;
    BYTE thumb_top[3] = {};
    BYTE track_top[3] = {};
    surface.target->BeginDraw();
    draw_grid(surface.target.Get(), surface.dwrite.Get(), font, columns, palette, nullptr, nullptr,
              0.0f, 0.0f, Bars{});
    hr = surface.target->EndDraw();
    if (FAILED(hr) || !surface.pixel_at(gutter_x, kHeaderHeight + 10.0f, thumb_top)) {
        return failed("reading the thumb at rest", FAILED(hr) ? hr : E_FAIL);
    }
    surface.target->BeginDraw();
    draw_grid(surface.target.Get(), surface.dwrite.Get(), font, columns, palette, nullptr, nullptr,
              most, 0.0f, Bars{});
    hr = surface.target->EndDraw();
    if (FAILED(hr) || !surface.pixel_at(gutter_x, kHeaderHeight + 10.0f, track_top)) {
        return failed("reading the track at the end of the scroll", FAILED(hr) ? hr : E_FAIL);
    }
    check(thumb_top[2] < track_top[2], "the thumb is drawn where the scroll says it is");

    // And it darkens under the pointer. Without this the only sign that a drag
    // has taken hold is the grid moving, which is also what a wheel does — and
    // a thumb that never acknowledges the press reads as a bar that was missed.
    BYTE held[3] = {};
    surface.target->BeginDraw();
    draw_grid(surface.target.Get(), surface.dwrite.Get(), font, columns, palette, nullptr, nullptr,
              0.0f, 0.0f, Bars{true, false});
    hr = surface.target->EndDraw();
    if (FAILED(hr) || !surface.pixel_at(gutter_x, kHeaderHeight + 10.0f, held)) {
        return failed("reading a thumb being dragged", FAILED(hr) ? hr : E_FAIL);
    }
    check(held[2] < thumb_top[2], "and darkens while it is being dragged");

    // ------------------------------------------------------------------
    // Sideways: the second axis, and the room the first one has to give it
    // ------------------------------------------------------------------

    // 552 DIPs of columns in a 640-DIP view, so nothing here is off the side
    // until the view is narrowed to less than the result — which is a window
    // dragged in, and the state the grid was in until this brick.
    View narrow = view;
    narrow.width = 400.0f;
    check(!bars_of(view).horizontal, "a result that fits across the view gets no bar");
    check(bars_of(narrow).horizontal, "and one that does not gets one");

    // The two bars are not independent, and that is the whole reason the
    // geometry takes one view rather than a pair of numbers per axis. The
    // horizontal bar is drawn over the last row, so having it costs the vertical
    // scroll a gutter's worth of rows...
    check(visible_row_span(narrow) < visible_row_span(view),
          "the bar along the foot takes a row's worth of room off the vertical scroll");
    Scrollbar shortened;
    check(scrollbar_of(false, narrow, &shortened)
              && shortened.track_length == bar.track_length - kScrollbarGutter,
          "and the vertical track stops above it rather than running into it");
    // ...and the vertical bar costs the horizontal one the same, at the corner
    // where the two would otherwise be drawn on top of each other.
    Scrollbar across;
    check(scrollbar_of(true, narrow, &across) && across.track_start == 0.0f
              && across.track_length == narrow.width - kScrollbarGutter,
          "while the horizontal track stops short of the vertical bar for the same reason");

    check(max_scroll_x(view) == 0.0f, "a result that fits cannot be scrolled sideways");
    check(max_scroll_x(narrow) == content_width_of(columns) - narrow.width,
          "and one that does not stops when its last column reaches the trailing edge");

    // The keyboard's half of it. The last column is 340 wide in a 400 view, so
    // it comes in by its trailing edge with room to spare; a column wider than
    // the view would not fit either way, and the leading edge is the one kept,
    // because that is where the value starts.
    check(scroll_x_to_visible(narrow, columns, 0) == 0.0f,
          "a column already on screen does not move the view sideways");
    check(scroll_x_to_visible(narrow, columns, 3)
              == columns[3].x + columns[3].width - narrow.width,
          "a column past the trailing edge is brought in by that edge");
    // Asked again of a middle column in a view too narrow to hold it, because
    // the last column's answer is also what the clamp would give: brought in by
    // its leading edge instead, that one lands on the same number and this one
    // lands 78 DIPs further along, with the column back off the edge it was
    // supposed to be brought inside.
    View slim = narrow;
    slim.width = 150.0f;
    check(scroll_x_to_visible(slim, columns, 2) == columns[2].x + columns[2].width - slim.width,
          "by that edge rather than by its leading one, where the two differ");
    View scrolled_out = narrow;
    scrolled_out.scroll_x = 200.0f;
    check(scroll_x_to_visible(scrolled_out, columns, 0) == 0.0f,
          "and one off the leading edge by its own");

    // And the hit test counts across in the same DIPs the drawing does. Twenty
    // in is the first column at rest and the second once the grid has moved
    // forty, which is the sideways version of clicking the row that used to be
    // where the pointer is.
    check(cell_at(20.0f, kHeaderHeight + 4.0f, view, columns, &under) && under.column == 0
              && cell_at(20.0f, kHeaderHeight + 4.0f, scrolled(0.0f, 40.0f), columns, &under)
              && under.column == 1,
          "a click across the grid is measured from the sideways scroll");

    // The same round trip the vertical thumb gets, on the axis where the track
    // is a different length and the travel is measured in DIPs rather than rows.
    View sideways = narrow;
    sideways.scroll_x = 100.0f;
    Scrollbar grabbed;
    check(scrollbar_of(true, sideways, &grabbed)
              && scroll_to_thumb(true, grabbed.thumb_start, narrow) > 99.9f
              && scroll_to_thumb(true, grabbed.thumb_start, narrow) < 100.1f,
          "dragging the horizontal thumb back to where it was leaves the scroll there");

    // Which bar a press belongs to. In the corner the gutters share, the
    // vertical one: the horizontal track stops short of that corner, so a press
    // there that scrolled sideways would be a press on track that is not drawn.
    bool sideways_press = true;
    check(scrollbar_axis_at(narrow.width - 2.0f, narrow.height - 2.0f, narrow, &sideways_press)
              && !sideways_press,
          "the corner where the two gutters meet belongs to the vertical bar");
    check(scrollbar_axis_at(10.0f, narrow.height - 2.0f, narrow, &sideways_press)
              && sideways_press,
          "the rest of the gutter along the foot is the horizontal bar's");
    check(!scrollbar_axis_at(10.0f, kHeaderHeight + 10.0f, narrow, &sideways_press),
          "and a press on the data itself belongs to neither");

    // And the drawing moves. Forty DIPs is most of a column here: the second
    // heading leaves the place it was drawn at and arrives forty to the left of
    // it, which is the difference between a grid that scrolls and one that
    // scrolls its arithmetic and not its pixels.
    //
    // Read on the name's line only. The type underneath runs the full width of
    // the column rather than stopping where the marker would, so a strip taken
    // over the whole band would find `VARCHAR` where the name is not.
    const float heading_x = columns[1].x + kCellPadding;
    // Where the first boundary arrives, which is empty canvas before the scroll:
    // the column to its left is numeric, so its digits are forty DIPs further
    // over. Read as the same pixel in two states rather than as two pixels in
    // one, because every other x down here has a value drawn across it.
    const float rule_x = columns[0].width - 40.0f;
    // And where the banding is read: against the trailing edge of the view
    // rather than past the last column. A band offset with the content would
    // still cover a point in the middle of the view, and what it would leave
    // bare is exactly this strip — the part of the row the result no longer
    // reaches, which is where the eye notices the stripes have come loose.
    const float band_x = static_cast<float>(kWidth) - kScrollbarGutter - 8.0f;
    const float band_y = kHeaderHeight + kRowHeight + 10.0f;
    UINT before_scroll = 0;
    UINT after_scroll = 0;
    UINT arrived_left = 0;
    BYTE rule_before[3] = {};
    BYTE rule_after[3] = {};
    BYTE band_before[3] = {};
    BYTE band_after[3] = {};
    surface.target->BeginDraw();
    draw_grid(surface.target.Get(), surface.dwrite.Get(), font, columns, palette, nullptr, nullptr,
              0.0f, 0.0f, Bars{});
    hr = surface.target->EndDraw();
    if (FAILED(hr)
        || !surface.ink_in(heading_x, 0.0f, heading_x + font.advance, kHeaderTypeY,
                           &before_scroll)
        || !surface.pixel_at(rule_x, kHeaderHeight + 10.0f, rule_before)
        || !surface.pixel_at(band_x, band_y, band_before)) {
        return failed("reading a heading before a sideways scroll", FAILED(hr) ? hr : E_FAIL);
    }
    surface.target->BeginDraw();
    draw_grid(surface.target.Get(), surface.dwrite.Get(), font, columns, palette, nullptr, nullptr,
              0.0f, 40.0f, Bars{});
    hr = surface.target->EndDraw();
    if (FAILED(hr)
        || !surface.ink_in(heading_x, 0.0f, heading_x + font.advance, kHeaderTypeY, &after_scroll)
        || !surface.ink_in(heading_x - 40.0f, 0.0f, heading_x - 40.0f + font.advance, kHeaderTypeY,
                           &arrived_left)
        || !surface.pixel_at(rule_x, kHeaderHeight + 10.0f, rule_after)
        || !surface.pixel_at(band_x, band_y, band_after)) {
        return failed("reading a heading after a sideways scroll", FAILED(hr) ? hr : E_FAIL);
    }
    check(before_scroll > 0 && after_scroll == 0, "a sideways scroll takes the headings with it");
    check(arrived_left > 0, "and puts them the distance to the left that was scrolled");
    // The separators are a second line of code and go the same way. One left
    // behind would draw the boundaries through the middle of the values.
    check(rule_after[2] < rule_before[2], "and the separators with them");

    // The banding does not move, and it is the one thing here that must not: it
    // runs the width of the view rather than the width of the result, so a grid
    // that offset the fills too would slide the stripes off the trailing edge
    // and leave a bare strip where the view ran out of result. Paired with the
    // reading being banded at all, or two pixels of canvas would agree.
    check(band_before[2] < 0xFF && band_after[2] == band_before[2],
          "while the banding stays with the view");

    // The bar itself, which needs a result wider than the bitmap: the first
    // column dragged to the maximum puts 836 DIPs of columns in a 640 view.
    std::vector<Column> spread = columns;
    set_column_width(&spread, 0, kMaxColumnWidth);
    const float foot_y = static_cast<float>(kHeight) - kScrollbarGutter / 2.0f;
    const float most_x = content_width_of(spread) - static_cast<float>(kWidth);
    BYTE no_bar[3] = {};
    BYTE foot_thumb[3] = {};
    BYTE foot_track[3] = {};
    BYTE foot_held[3] = {};
    if (!surface.pixel_at(10.0f, foot_y, no_bar)) {
        return failed("reading the foot of a grid that fits", E_FAIL);
    }
    surface.target->BeginDraw();
    draw_grid(surface.target.Get(), surface.dwrite.Get(), font, spread, palette, nullptr, nullptr,
              0.0f, 0.0f, Bars{});
    hr = surface.target->EndDraw();
    if (FAILED(hr) || !surface.pixel_at(10.0f, foot_y, foot_thumb)) {
        return failed("reading the bar along the foot", FAILED(hr) ? hr : E_FAIL);
    }
    surface.target->BeginDraw();
    draw_grid(surface.target.Get(), surface.dwrite.Get(), font, spread, palette, nullptr, nullptr,
              0.0f, most_x, Bars{});
    hr = surface.target->EndDraw();
    if (FAILED(hr) || !surface.pixel_at(10.0f, foot_y, foot_track)) {
        return failed("reading the foot at the end of a sideways scroll", FAILED(hr) ? hr : E_FAIL);
    }
    surface.target->BeginDraw();
    draw_grid(surface.target.Get(), surface.dwrite.Get(), font, spread, palette, nullptr, nullptr,
              0.0f, 0.0f, Bars{false, true});
    hr = surface.target->EndDraw();
    if (FAILED(hr) || !surface.pixel_at(10.0f, foot_y, foot_held)) {
        return failed("reading the foot thumb being dragged", FAILED(hr) ? hr : E_FAIL);
    }
    check(foot_thumb[2] < no_bar[2], "a result wider than the view draws a bar along its foot");
    // Both halves at one pixel: the thumb left it, and the track did not. A bar
    // whose track was only as long as its thumb would pass the first of those
    // and leave the gutter empty wherever the scroll is not.
    check(foot_track[2] > foot_thumb[2] && foot_track[2] < no_bar[2],
          "with the thumb where the sideways scroll says and the track running past it");
    // Which also says the two bars read their own halves of the pair. Both
    // reading `dragging.vertical` is a swap the vertical checks cannot see.
    check(foot_held[2] < foot_thumb[2], "and it darkens while it is being dragged");

    // ------------------------------------------------------------------
    // The sort: what a heading click asks the server for, and what marks it
    // ------------------------------------------------------------------

    // The three states, in the order a repeated click walks through them.
    Sort ascending;
    Sort descending;
    Sort cleared;
    check(next_sort(nullptr, 1, &ascending) && ascending.column == 1 && !ascending.descending,
          "a heading that carried no sort becomes ascending");
    check(next_sort(&ascending, 1, &descending) && descending.descending,
          "the same heading again reverses it");
    check(!next_sort(&descending, 1, &cleared),
          "and a third click clears it rather than starting over");
    // The state that a two-state toggle cannot offer: a user who sorted by
    // mistake can put the result back the way the server sent it.
    Sort another;
    check(next_sort(&descending, 2, &another) && another.column == 2 && !another.descending,
          "a different heading starts ascending whichever way the last one pointed");

    check(order_clause(nullptr, columns).empty(), "an unsorted grid asks for no order at all");
    check(order_clause(&ascending, columns) == "\"name\"", "a sort asks for its column by name");
    check(order_clause(&descending, columns) == "\"name\" DESC", "and says which way");
    // Quoted rather than pasted, and a quote inside the name doubled. Neither
    // matters for `name`; both matter for the first column somebody selects
    // called `order`, or the first one with a quote in it — and an identifier
    // that broke the statement would arrive as a failed query rather than as a
    // grid that looks wrong.
    std::vector<Column> awkward(1);
    awkward[0].name = "a\"b";
    const Sort odd;
    check(order_clause(&odd, awkward) == "\"a\"\"b\"",
          "a quote inside a name is doubled rather than ending the identifier");

    int heading = -1;
    check(header_column_at(columns[2].x + 4.0f, 4.0f, 0.0f, columns, &heading) && heading == 2,
          "a point on a heading finds that column");
    check(!header_column_at(columns[2].x + 4.0f, kHeaderHeight + 4.0f, 0.0f, columns, &heading),
          "a point below the band finds none");
    check(!header_column_at(right + 4.0f, 4.0f, 0.0f, columns, &heading),
          "a point past the last heading finds none");
    // Twenty in is the first heading at rest and the second once the grid has
    // been scrolled forty. Without this a click sorts by whichever column was
    // under the pointer before the user scrolled it away.
    check(header_column_at(20.0f, 4.0f, 40.0f, columns, &heading) && heading == 1,
          "and a heading is found where it is drawn rather than where it was laid out");

    // The server sorts. Descending by `name` is a text order, so it answers
    // `driver-9` rather than `driver-39` — which is the whole point of asking
    // it: a grid that reordered its own page by the number it can see in the
    // string would answer the other way, and so would one that sorted the
    // column instead of the row.
    check(columns[0].cells[0] == L"0" && columns[1].cells[0] == L"driver-0",
          "the unsorted result starts where the base order does");
    std::vector<Column> reordered;
    if (!load_grid(order_clause(&descending, columns), std::string(), &reordered)) {
        return false;
    }
    check(reordered.size() == columns.size() && reordered[0].cells.size() == rows,
          "sorting returns the same result rather than a different one");
    check(reordered[1].cells[0] == L"driver-9", "and in the order the sort asked for");
    check(reordered[0].cells[0] == L"9", "with the whole row moved, not one column of it");

    // What the sorted column looks like. Asked of the hue rather than of
    // `ink_in`, and not by choice: `ink_in` counts a pixel only when all three
    // of its channels have left the canvas, and the accent's blue is 229 — 26
    // from white, well inside the tolerance. It cannot see this mark at all.
    // Written the other way round the check would have been a green line over a
    // header with no marker in it, which is how it first came out.
    //
    // So one measure answers both halves: in this band the marker is the only
    // blue thing there is, and "was anything drawn" and "was it the accent" are
    // the same question. Splitting them would be two checks with one answer.
    const Column& marked = columns[1];
    const float marker_x = marked.x + marked.width - kCellPadding - font.advance;
    const float name_x = marked.x + kCellPadding;
    int plain_tint = 0;
    BYTE plain_heading = 0xFF;
    surface.target->BeginDraw();
    draw_grid(surface.target.Get(), surface.dwrite.Get(), font, columns, palette, nullptr, nullptr,
              0.0f, 0.0f, Bars{});
    hr = surface.target->EndDraw();
    if (FAILED(hr)
        || !surface.bluest_in(marker_x, 0.0f, marker_x + font.advance, kHeaderHeight, &plain_tint)
        || !surface.darkest_in(name_x, 0.0f, marker_x, kHeaderHeight, &plain_heading)) {
        return failed("reading an unsorted heading", FAILED(hr) ? hr : E_FAIL);
    }
    check(plain_tint < 20, "an unsorted heading carries no marker");

    int marker_tint = 0;
    BYTE sorted_heading = 0xFF;
    surface.target->BeginDraw();
    draw_grid(surface.target.Get(), surface.dwrite.Get(), font, columns, palette, nullptr,
              &descending, 0.0f, 0.0f, Bars{});
    hr = surface.target->EndDraw();
    if (FAILED(hr)
        || !surface.bluest_in(marker_x, 0.0f, marker_x + font.advance, kHeaderHeight, &marker_tint)
        || !surface.darkest_in(name_x, 0.0f, marker_x, kHeaderHeight, &sorted_heading)) {
        return failed("reading a sorted heading", FAILED(hr) ? hr : E_FAIL);
    }
    // Blue runs 150 ahead of red in the accent, 34 ahead in `Grid.headerText`
    // and 8 ahead in the band itself, so a partly covered pixel still lands on
    // the right side of this — which matters for a triangle six DIPs across,
    // where few pixels are entirely its own.
    check(marker_tint > 60, "a sorted one carries the accent-coloured marker");
    // And the name beside it darkens. A six-DIP triangle is a small thing to
    // have to find; the column reads as sorted from across the window because
    // its name is the one heading in `Text.primary`.
    check(sorted_heading < plain_heading, "and the column's name darkens with it");

    // Nothing else in the band moved. The marker is drawn per column, and the
    // failure that costs is one drawn for every column — which the checks above
    // cannot see, because they only ever look at the column that is supposed to
    // have it.
    int neighbour_tint = 0;
    const Column& unmarked = columns[2];
    const float neighbour_x = unmarked.x + unmarked.width - kCellPadding - font.advance;
    if (!surface.bluest_in(neighbour_x, 0.0f, neighbour_x + font.advance, kHeaderHeight,
                           &neighbour_tint)) {
        return failed("reading an unsorted heading beside a sorted one", E_FAIL);
    }
    check(neighbour_tint < 20, "and the headings beside it carry none");

    // And the marker's box holds the marker rather than the last letters of the
    // name. This is the only column where the two can collide: `lay_out` gives
    // every column a character more than its widest content, so a heading only
    // reaches its own trailing edge where the width was clamped.
    //
    // Read as darkness, not as hue. What would be there is `Grid.headerText`,
    // whose lightest channel is 105, against an accent whose lightest is 229
    // over a band at 249 — the two answer the hue question the same way and the
    // darkness question 120 apart.
    const Column& clamped = columns.back();
    const float clamped_marker = clamped.x + clamped.width - kCellPadding - font.advance;
    Sort clamped_sort;
    clamped_sort.column = static_cast<int>(columns.size()) - 1;
    BYTE marker_alone = 0;
    surface.target->BeginDraw();
    draw_grid(surface.target.Get(), surface.dwrite.Get(), font, columns, palette, nullptr,
              &clamped_sort, 0.0f, 0.0f, Bars{});
    hr = surface.target->EndDraw();
    if (FAILED(hr) || !surface.darkest_in(clamped_marker, 0.0f, clamped_marker + font.advance,
                                          kHeaderHeight, &marker_alone)) {
        return failed("reading the marker on a clamped column", FAILED(hr) ? hr : E_FAIL);
    }
    check(marker_alone > 180, "and the marker's box is kept clear of the name it marks");

    // ------------------------------------------------------------------
    // The resize handles: where a boundary can be taken hold of, and what
    // happens to the layout when it moves
    // ------------------------------------------------------------------

    int handle = -1;
    const float boundary = columns[0].x + columns[0].width;
    check(column_edge_at(boundary, 4.0f, 0.0f, columns, &handle) && handle == 0,
          "a point on a boundary finds the column to its left");
    check(column_edge_at(boundary - kEdgeTolerance + 0.5f, 4.0f, 0.0f, columns, &handle)
              && handle == 0
              && column_edge_at(boundary + kEdgeTolerance - 0.5f, 4.0f, 0.0f, columns, &handle)
              && handle == 0,
          "and the handle reaches both sides of it");
    check(!column_edge_at(columns[0].x + columns[0].width / 2.0f, 4.0f, 0.0f, columns, &handle),
          "the middle of a heading is not a handle");
    // Otherwise the top row of the result would be eight DIPs of resize handle
    // rather than eight DIPs of row, at four places across every column.
    check(!column_edge_at(boundary, kHeaderHeight + 4.0f, 0.0f, columns, &handle),
          "and a boundary below the band is not one either");
    check(column_edge_at(right, 4.0f, 0.0f, columns, &handle)
              && handle == static_cast<int>(columns.size()) - 1,
          "the last column has a handle of its own");
    // And the handles move with the grid. This x is the middle of the first
    // heading at rest and the boundary after it once the view has scrolled
    // forty, so a handle that ignored the scroll would be four DIPs of resize
    // in the middle of a name.
    check(column_edge_at(boundary - 40.0f, 4.0f, 40.0f, columns, &handle) && handle == 0,
          "and a handle is where the boundary is drawn rather than where it was laid out");

    std::vector<Column> resized = columns;
    const float dragged = columns[0].width + 40.0f;
    set_column_width(&resized, 0, dragged);
    check(resized[0].width == dragged, "a dragged width is taken as it was given");
    check(resized[1].x == columns[1].x + 40.0f && resized[2].x == columns[2].x + 40.0f,
          "and every column after it moves by the same distance");
    check(resized[1].width == columns[1].width && resized[2].width == columns[2].width,
          "while their own widths are left alone");

    // The same clamp the measured widths get. Dragged to nothing a column has
    // no edge left to take hold of, and there is no other way to bring it back.
    set_column_width(&resized, 0, 1.0f);
    check(resized[0].width == kMinColumnWidth, "a column cannot be dragged away entirely");
    set_column_width(&resized, 0, 10000.0f);
    check(resized[0].width == kMaxColumnWidth, "nor past the width the layout allows");

    // A drag has to survive the next statement, and every heading click is one.
    set_column_width(&resized, 0, 120.0f);
    std::vector<Column> again;
    if (!load_grid(std::string(), std::string(), &again)) {
        return false;
    }
    lay_out(&again, font.advance);
    std::vector<Column> carried = again;
    check(carry_widths(resized, &carried) && carried[0].width == 120.0f
              && carried[1].x == 120.0f,
          "the same columns keep the widths they were dragged to");
    // And a result that is not those columns does not inherit them — including
    // not inheriting the first few, which is what a loop that copied as it
    // compared would leave behind.
    std::vector<Column> renamed = again;
    renamed[1].name = "elsewhere";
    check(!carry_widths(resized, &renamed) && renamed[0].width == again[0].width,
          "a different result is measured afresh instead");

    // And the drawing follows the width. Moving the offsets without moving the
    // pixels is a resize the checks believe and the user cannot see, so this
    // asks where the second heading is: at the start of its own column before
    // the drag, and forty DIPs along afterwards.
    std::vector<Column> wider = columns;
    set_column_width(&wider, 0, columns[0].width + 40.0f);
    const float probe = columns[1].x + kCellPadding;
    UINT at_rest = 0;
    UINT left_behind = 0;
    UINT arrived = 0;
    surface.target->BeginDraw();
    draw_grid(surface.target.Get(), surface.dwrite.Get(), font, columns, palette, nullptr, nullptr,
              0.0f, 0.0f, Bars{});
    hr = surface.target->EndDraw();
    if (FAILED(hr)
        || !surface.ink_in(probe, 0.0f, probe + font.advance, kHeaderHeight, &at_rest)) {
        return failed("reading a heading before a resize", FAILED(hr) ? hr : E_FAIL);
    }
    surface.target->BeginDraw();
    draw_grid(surface.target.Get(), surface.dwrite.Get(), font, wider, palette, nullptr, nullptr,
              0.0f, 0.0f, Bars{});
    hr = surface.target->EndDraw();
    if (FAILED(hr)
        || !surface.ink_in(probe, 0.0f, probe + font.advance, kHeaderHeight, &left_behind)
        || !surface.ink_in(probe + 40.0f, 0.0f, probe + 40.0f + font.advance, kHeaderHeight,
                           &arrived)) {
        return failed("reading a heading after a resize", FAILED(hr) ? hr : E_FAIL);
    }
    check(at_rest > 0 && left_behind == 0, "a widened column takes its neighbour's name off");
    check(arrived > 0, "and puts it where the new width says it goes");

    // ------------------------------------------------------------------
    // The type line: what the server called the column, under its name
    // ------------------------------------------------------------------

    // DuckDB's spelling, not Arrow's. The first column arrives as `l` and the
    // second as `u`, and a grid that read the type off the format string would
    // say `int64` and `utf8` — true about the buffers, silent about the columns,
    // and unable to tell this `VARCHAR` from a `JSON` or an `ENUM`, which arrive
    // as the same `u`.
    check(columns[0].type_name == L"BIGINT" && columns[1].type_name == L"VARCHAR",
          "every column carries the type the server declared it with");

    // The same columns with nothing declared, which is the state a driver that
    // cannot answer leaves them in — and the control for everything below.
    std::vector<Column> untyped = columns;
    for (Column& column : untyped) {
        column.type_name.clear();
    }
    lay_out(&untyped, font.advance);

    // Two DIPs of heading and six characters of `BIGINT`: without a say in the
    // width this column is the minimum, and the type under it would be cut to
    // `BIGI…`. A type shortened past recognition is worse than the space it
    // saves — `TIMESTAMP` cut to `TIME` is a different type, not a shorter word.
    check(untyped[0].width == kMinColumnWidth, "a narrow column is held to the minimum");
    check(columns[0].width > untyped[0].width,
          "and one too narrow for its own type name is widened to hold it");
    // But only so far. A type is routinely longer than every value beneath it,
    // and a column sized to spell one out is a column of data pushed off screen.
    std::vector<Column> shouted = untyped;
    std::vector<Column> bounded = untyped;
    shouted[0].type_name = std::wstring(kMaxTypeChars * 2, L'X');
    bounded[0].type_name = std::wstring(kMaxTypeChars, L'X');
    lay_out(&shouted, font.advance);
    lay_out(&bounded, font.advance);
    check(bounded[0].width > columns[0].width && shouted[0].width == bounded[0].width,
          "and a longer one stops widening it at thirteen characters");

    // Drawn on its own line rather than instead of the name. Read over one
    // column, and over the second: `name` and `note` have no descender between
    // them, where the last heading is full of them and would put ink below the
    // fold that came from the line above.
    //
    // The control keeps the widths it was measured with and loses only the
    // types, so the two drawings differ in the one thing being asked about. Laid
    // out afresh the columns would sit two DIPs to the left, the glyphs would
    // land on different fractions of a pixel, and the name's ink would differ
    // between them for a reason that has nothing to do with the type line.
    std::vector<Column> undeclared = columns;
    for (Column& column : undeclared) {
        column.type_name.clear();
    }
    const Column& typed = columns[1];
    UINT name_ink = 0;
    UINT type_ink = 0;
    UINT untyped_name_ink = 0;
    UINT untyped_type_ink = 0;
    BYTE name_darkest = 0xFF;
    BYTE type_darkest = 0xFF;
    surface.target->BeginDraw();
    draw_grid(surface.target.Get(), surface.dwrite.Get(), font, columns, palette, nullptr, nullptr,
              0.0f, 0.0f, Bars{});
    hr = surface.target->EndDraw();
    if (FAILED(hr)
        || !surface.ink_in(typed.x, 0.0f, typed.x + typed.width, kHeaderTypeY, &name_ink)
        || !surface.ink_in(typed.x, kHeaderTypeY, typed.x + typed.width, kHeaderHeight - 1.0f,
                           &type_ink)
        || !surface.darkest_in(typed.x, 0.0f, typed.x + typed.width, kHeaderTypeY, &name_darkest)
        || !surface.darkest_in(typed.x, kHeaderTypeY, typed.x + typed.width, kHeaderHeight - 1.0f,
                               &type_darkest)) {
        return failed("reading a header with a type under it", FAILED(hr) ? hr : E_FAIL);
    }
    surface.target->BeginDraw();
    draw_grid(surface.target.Get(), surface.dwrite.Get(), font, undeclared, palette, nullptr,
              nullptr, 0.0f, 0.0f, Bars{});
    hr = surface.target->EndDraw();
    if (FAILED(hr)
        || !surface.ink_in(typed.x, 0.0f, typed.x + typed.width, kHeaderTypeY, &untyped_name_ink)
        || !surface.ink_in(typed.x, kHeaderTypeY, typed.x + typed.width, kHeaderHeight - 1.0f,
                           &untyped_type_ink)) {
        return failed("reading a header with nothing declared", FAILED(hr) ? hr : E_FAIL);
    }
    check(type_ink > 0 && untyped_type_ink == 0, "the declared type is drawn under the name");
    // Paired with the line above staying put, because "under" is the whole
    // claim: a type drawn at the name's y would satisfy the first half and
    // overwrite the thing the grid is navigated by.
    check(name_ink > 0 && name_ink == untyped_name_ink, "and the name above it is untouched");
    // In `Text.dataMuted`, the rung the word NULL is on. A type in the name's
    // own tone reads as a second name and doubles what the eye has to sort
    // through in a band that is mostly names.
    check(type_darkest > name_darkest, "in a dimmer tone than the name");

    // ------------------------------------------------------------------
    // The other appearance: the same drawing against the other column
    // ------------------------------------------------------------------

    // Every tone moved. This is the failure the two tables invite — a name added
    // to one column and left out of the other reads as a grid that is nearly in
    // dark mode, which is worse than one that is not in it at all, because the
    // one wrong tone is the one the eye goes to.
    check(kLightTones.canvas != kDarkTones.canvas
              && kLightTones.header_band != kDarkTones.header_band
              && kLightTones.ink != kDarkTones.ink
              && kLightTones.header_ink != kDarkTones.header_ink
              && kLightTones.sorted_header_ink != kDarkTones.sorted_header_ink
              && kLightTones.muted_ink != kDarkTones.muted_ink
              && kLightTones.rule != kDarkTones.rule
              && kLightTones.accent != kDarkTones.accent
              && kLightTones.pending != kDarkTones.pending,
          "every colour in the table has an answer for both appearances");
    // And every alpha, which is the part that looks safe to copy across. Black
    // at 0.022 over white is a step nobody can see, so the light side is a shade
    // stronger than its dark counterpart rather than the same number.
    check(kLightTones.banding_alpha > kDarkTones.banding_alpha
              && kLightTones.separator_alpha > kDarkTones.separator_alpha
              && kLightTones.scroll_track_alpha > kDarkTones.scroll_track_alpha
              && kLightTones.scroll_thumb_alpha < kDarkTones.scroll_thumb_alpha,
          "and the strengths are the appearance's own rather than copied across");

    Palette dark;
    if (!dark.open(surface.target.Get(), kDarkTones)) {
        return false;
    }
    surface.canvas = kDarkTones.canvas;
    surface.target->BeginDraw();
    draw_grid(surface.target.Get(), surface.dwrite.Get(), font, columns, dark, &selection, nullptr,
              0.0f, 0.0f, Bars{});
    hr = surface.target->EndDraw();
    if (FAILED(hr)) {
        return failed("ID2D1RenderTarget::EndDraw in dark", hr);
    }

    BYTE dark_canvas[3] = {};
    BYTE dark_band[3] = {};
    BYTE dark_row[3] = {};
    if (!surface.pixel_at(right + 20.0f, kHeaderHeight + 10.0f, dark_canvas)
        || !surface.pixel_at(right + 20.0f, 10.0f, dark_band)
        || !surface.pixel_at(right + 20.0f, kHeaderHeight + kRowHeight + 10.0f, dark_row)) {
        return failed("reading a dark grid", E_FAIL);
    }
    check(dark_canvas[2] < 0x40 && dark_canvas[1] < 0x40 && dark_canvas[0] < 0x60,
          "the dark canvas is the near-black Surface.canvas names");
    // Above the canvas, not below it. `Surface.raised` is the surface a thing
    // sits on top of, and on a dark background "raised" means lighter — the one
    // relation that inverts, and the reason a second table beats a filter over
    // the first.
    check(dark_band[2] > dark_canvas[2] && dark_band[1] > dark_canvas[1]
              && dark_band[0] > dark_canvas[0],
          "and the header band is above it rather than below it");
    // Row 1 is banded. On white the banding darkens; here it has to lighten, or
    // it is the light table's rule colour laid on the wrong end of the ramp,
    // which draws nothing at all.
    check(dark_row[2] > dark_canvas[2] && dark_row[0] > dark_canvas[0],
          "and the banding lightens the row instead of darkening it");

    // The measure follows the canvas, so this asks the same question the light
    // checks ask: did the glyphs reach the bitmap.
    UINT dark_ink = 0;
    UINT dark_blank = 0;
    if (!surface.ink_in(0.0f, kHeaderHeight, right, kHeaderHeight + kRowHeight, &dark_ink)
        || !surface.ink_in(right + 4.0f, kHeaderHeight, static_cast<float>(kWidth)
                                                           - kScrollbarGutter,
                           kHeaderHeight + 4.0f * kRowHeight, &dark_blank)) {
        return failed("reading dark text", E_FAIL);
    }
    check(dark_ink > 0, "the values are drawn against the dark canvas");
    // Paired with the emptiness beside them, because on this canvas the first
    // question alone is answered by the canvas. A measure that counted dark
    // pixels instead of distance reports a dark grid as painted edge to edge
    // and passes the line above for a grid with nothing written on it at all —
    // measured, by putting that measure back: every other check here stays
    // green and only this one turns.
    check(dark_blank == 0, "and the space past the last column is not");
    // The cursor cell still stands out, which is the thing a translucent accent
    // over a near-black surface is least likely to manage. Read as blue rising
    // rather than red falling: over black a tint can only add light, which is
    // the opposite of the direction the light-mode check reads.
    BYTE dark_cell[3] = {};
    if (!surface.pixel_at(columns[2].x + columns[2].width - 4.0f,
                          kHeaderHeight + kRowHeight + 10.0f, dark_cell)) {
        return failed("reading a dark cursor cell", E_FAIL);
    }
    check(dark_cell[0] > dark_row[0] && dark_cell[0] > dark_cell[2],
          "and the cursor cell is brighter than its row rather than darker");

    // Back to the appearance the rest of this file measures in, bitmap and all:
    // the count below is over whatever was drawn last, and read against the
    // wrong canvas a dark grid answers with nearly every pixel it has.
    surface.canvas = kLightTones.canvas;
    surface.target->BeginDraw();
    draw_grid(surface.target.Get(), surface.dwrite.Get(), font, columns, palette, &selection,
              nullptr, 0.0f, 0.0f, Bars{});
    hr = surface.target->EndDraw();
    if (FAILED(hr)) {
        return failed("ID2D1RenderTarget::EndDraw back in light", hr);
    }

    UINT total = 0;
    surface.ink_in(0.0f, 0.0f, static_cast<float>(kWidth), static_cast<float>(kHeight), &total);
    std::string widths;
    for (const Column& column : columns) {
        widths += " " + std::to_string(static_cast<int>(column.width));
    }
    std::printf("      %u pixels painted, %.2f advance, columns%s\n", total, font.advance,
                widths.c_str());
    return failures == 0;
}

// -------------------------------------------------------------------------
// The driver list, which was the first thing this could draw
// -------------------------------------------------------------------------

// The `label` of every driver, scanned out rather than parsed. A JSON parser
// would be a second thing to be wrong in a check about drawing, and the shape
// here is fixed by `db_drivers_json` rather than by a user.
std::vector<std::string> labels_in(const char* json) {
    std::vector<std::string> found;
    const std::string text(json);
    const std::string key = "\"label\":\"";
    size_t at = 0;
    while ((at = text.find(key, at)) != std::string::npos) {
        const size_t from = at + key.size();
        const size_t to = text.find('"', from);
        if (to == std::string::npos) {
            break;
        }
        found.push_back(text.substr(from, to - from));
        at = to;
    }
    return found;
}

bool the_driver_list_draws() {
    char* err = nullptr;
    char* json = db_drivers_json(&err);
    if (json == nullptr) {
        return core_failed("db_drivers_json", err);
    }
    const std::vector<std::string> labels = labels_in(json);
    db_string_free(json);

    // Ten rather than one: a scan that found a single label would pass a "not
    // empty" check while having gone wrong, and this build has fifteen drivers.
    check(labels.size() >= 10, "the catalog names at least ten drivers");
    if (labels.empty()) {
        return false;
    }

    std::wstring text;
    for (const std::string& label : labels) {
        text += widen(label);
        text += L'\n';
    }

    Surface surface;
    if (!surface.open()) {
        return false;
    }
    Monospace font;
    if (!font.open(surface.dwrite)) {
        return false;
    }

    ComPtr<IDWriteTextLayout> layout;
    HRESULT hr = surface.dwrite->CreateTextLayout(text.c_str(), static_cast<UINT32>(text.size()),
                                                  font.format.Get(), static_cast<FLOAT>(kWidth),
                                                  static_cast<FLOAT>(kHeight), &layout);
    if (FAILED(hr)) {
        return failed("CreateTextLayout", hr);
    }
    DWRITE_TEXT_METRICS metrics{};
    hr = layout->GetMetrics(&metrics);
    if (FAILED(hr)) {
        return failed("IDWriteTextLayout::GetMetrics", hr);
    }
    check(metrics.width > 0.0f && metrics.height > 0.0f, "the layout has measurable extent");
    check(static_cast<size_t>(metrics.lineCount) >= labels.size(),
          "the layout holds a line per driver");
    // Not clipped: a layout wider or taller than the box it was given still
    // reports its own extent, and would then draw only the part that fitted.
    check(metrics.width <= static_cast<FLOAT>(kWidth)
              && metrics.height <= static_cast<FLOAT>(kHeight),
          "the layout fits the surface it was measured against");

    ComPtr<ID2D1SolidColorBrush> brush;
    hr = surface.target->CreateSolidColorBrush(D2D1::ColorF(D2D1::ColorF::Black), &brush);
    if (FAILED(hr)) {
        return failed("CreateSolidColorBrush", hr);
    }

    surface.target->BeginDraw();
    surface.target->Clear(D2D1::ColorF(D2D1::ColorF::White));
    surface.target->DrawTextLayout(D2D1::Point2F(0.0f, 0.0f), layout.Get(), brush.Get());
    hr = surface.target->EndDraw();
    if (FAILED(hr)) {
        return failed("ID2D1RenderTarget::EndDraw", hr);
    }

    UINT ink = 0;
    if (!surface.ink_in(0.0f, 0.0f, static_cast<float>(kWidth), static_cast<float>(kHeight),
                        &ink)) {
        return failed("reading the bitmap back", E_FAIL);
    }
    std::printf("      %u pixels painted, %u lines, %.1f x %.1f\n", ink, metrics.lineCount,
                metrics.width, metrics.height);
    check(ink > 0, "the text reached the bitmap");

    return failures == 0;
}

// -------------------------------------------------------------------------
// The window, drawing what the check above draws
// -------------------------------------------------------------------------

// The `EDIT` class's own procedure, kept so the one below can hand back
// everything it does not answer. One copy for the process: every field this grid
// opens is an `EDIT`, and they all share it.
WNDPROC edit_base_proc = nullptr;

LRESULT CALLBACK editor_proc(HWND field, UINT message, WPARAM wparam, LPARAM lparam);

struct Window {
    HWND hwnd = nullptr;
    ComPtr<ID2D1Factory> d2d;
    ComPtr<IDWriteFactory> dwrite;
    ComPtr<ID2D1HwndRenderTarget> target;
    Palette palette;
    Monospace font;
    std::vector<Column> columns;
    Selection selection;
    bool selected = false;
    Sort sort;
    bool sorted = false;
    float scroll_row = 0.0f;
    float scroll_x = 0.0f;
    // Where on the thumb the drag took hold, so the thumb stays under the
    // pointer instead of jumping its own leading edge there on the first move.
    Bars dragging;
    float grab_offset = 0.0f;
    // A header drag, from where it started rather than as a delta per move. The
    // width follows the total distance from the press: accumulating each move
    // instead would let the column drift away from the pointer over a long drag,
    // once the clamp had swallowed part of one.
    bool resizing = false;
    size_t resize_column = 0;
    float resize_from = 0.0f;
    float resize_width = 0.0f;
    bool is_light = true;
    // The box somebody is typing in, the cell it is over, and whether it has
    // already reported. A field taken away while it has the focus loses it on
    // the way out, which arrives as one more end-of-edit — so ending has to be
    // idempotent or clicking from one cell to another would stage twice.
    // `InlineCellEditor.swift` carries the same flag for the same reason.
    HWND editor = nullptr;
    Selection editing;
    bool ending = false;
    HFONT editor_font = nullptr;
    HBRUSH editor_brush = nullptr;
    // What the filter menu has narrowed the result to, in the order the items
    // were chosen. Kept as the predicates rather than as the clause so that
    // *Filter on* can throw away what came before and *Add to Filter* cannot.
    std::vector<std::string> filters;

    const Tones& tones() const { return is_light ? kLightTones : kDarkTones; }

    // The frame as well as the client area. Windows draws the title bar itself,
    // and a dark grid under a white caption is the half-done version of this
    // that reads as a bug rather than as a choice.
    //
    // Attribute 20, which is what Windows 11 and Windows 10 20H1 onwards use.
    // Earlier builds took 19 for the same thing; the call simply fails there,
    // and a light caption on a build that old is the correct outcome anyway.
    void follow_frame() {
        const BOOL dark = is_light ? FALSE : TRUE;
        DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &dark, sizeof(dark));
    }

    // Windows changed appearance under a running window. The brushes hold their
    // colours, so they are rebuilt rather than re-tinted — the palette is the
    // only place these numbers exist once drawing has started.
    void follow_appearance() {
        const bool light = windows_is_light();
        if (light == is_light) {
            return;
        }
        is_light = light;
        follow_frame();
        if (target) {
            palette = Palette();
            palette.open(target.Get(), tones());
        }
        // The box's brush holds a colour too, and it is a GDI object rather than
        // a Direct2D one, so it is dropped here and made again on the next paint
        // that asks for it.
        if (editor_brush != nullptr) {
            DeleteObject(editor_brush);
            editor_brush = nullptr;
        }
        InvalidateRect(hwnd, nullptr, FALSE);
    }

    ~Window() {
        if (editor_font != nullptr) {
            DeleteObject(editor_font);
        }
        if (editor_brush != nullptr) {
            DeleteObject(editor_brush);
        }
    }

    size_t rows() const { return columns.empty() ? 0 : columns[0].cells.size(); }

    // A heading was clicked. The result is asked for again in the new order,
    // which is what makes the marker true: a grid that reordered the page it
    // already had would put an ascending triangle over the forty rows the
    // server happened to send first.
    //
    // Read into a second vector and swapped in only once it arrives, so a
    // statement that fails leaves the grid showing what it was showing rather
    // than emptying it. `load_grid` has already said what went wrong.
    void sort_by(int column) {
        Sort next;
        const bool wanted = next_sort(sorted ? &sort : nullptr, column, &next);
        std::vector<Column> fresh;
        // Under whatever the filter menu has narrowed the result to. A sort that
        // dropped the filter would answer a click on a heading by putting rows
        // back that somebody had asked to be rid of.
        if (!load_grid(order_clause(wanted ? &next : nullptr, columns), where_clause(filters),
                       &fresh)) {
            return;
        }
        // The widths come across when the columns are the same columns, which
        // they are for every sort: re-measuring here would undo a header drag
        // on the next click, and a width that will not stay put is a width
        // nobody will drag twice.
        if (!carry_widths(columns, &fresh)) {
            lay_out(&fresh, font.advance);
        }
        columns = std::move(fresh);
        sort = next;
        sorted = wanted;
        // Back to the top, for `MetalGridView.swift`'s reason: row four of the
        // result that has just arrived is a different row from row four of the
        // one that was on screen, so an offset kept across the change points at
        // an arbitrary window of unrelated data.
        scroll_row = 0.0f;
        // Sideways it stays, because the columns are the same columns in the
        // same order and the place across the result still means what it meant.
        // Only held to their total, in case the fallback above measured afresh.
        hold_scroll_x();
        InvalidateRect(hwnd, nullptr, FALSE);
    }

    // A filter the menu asked for, or the end of all of them.
    //
    // `extend` is the difference between the two parent items: *Filter on* is a
    // fresh question about this column and *Add to Filter* narrows what is
    // already there. Replacing is safe because the stack is the only thing that
    // holds a filter — there is no field here for somebody to have typed
    // something into that this would overwrite, which is what
    // `AppModel.filterByCell` had to be careful about.
    void filter_by(Filter op, bool extend, const Selection& on) {
        const size_t at = static_cast<size_t>(on.column);
        if (on.column < 0 || at >= columns.size()) {
            return;
        }
        std::vector<std::string> wanted = extend ? filters : std::vector<std::string>();
        wanted.push_back(cell_predicate(columns[at], on.row, op));
        apply_filters(wanted);
    }

    void clear_filters() {
        if (filters.empty()) {
            return;
        }
        apply_filters(std::vector<std::string>());
    }

    // The result, read again under a different set of predicates.
    //
    // Into a second vector and swapped in only once it arrives, for `sort_by`'s
    // reason: a statement that fails leaves the grid showing what it was
    // showing. A filter is the more likely of the two to fail — it carries a
    // value back to the server — and an empty grid is exactly what a working
    // filter can also look like, so the two must not be confusable.
    void apply_filters(const std::vector<std::string>& wanted) {
        std::vector<Column> fresh;
        if (!load_grid(order_clause(sorted ? &sort : nullptr, columns), where_clause(wanted),
                       &fresh)) {
            return;
        }
        if (!carry_widths(columns, &fresh)) {
            lay_out(&fresh, font.advance);
        }
        columns = std::move(fresh);
        filters = wanted;
        // The cursor goes, rather than being clamped into range. These are
        // different rows: row four of a filtered result is not row four of the
        // one before it, and a selection kept across the change points at a row
        // nobody chose. The edit marks go with them for the same reason — they
        // were recorded against where a row sat.
        selected = false;
        scroll_row = 0.0f;
        hold_scroll_x();
        InvalidateRect(hwnd, nullptr, FALSE);
    }

    // Everything the geometry needs, assembled the one way. In DIPs, which is
    // what every scroll calculation is in.
    View view() const {
        const D2D1_SIZE_F size = target ? target->GetSize() : D2D1::SizeF(0.0f, 0.0f);
        return View{size.width,  size.height, content_width_of(columns),
                    rows(),      scroll_row,  scroll_x};
    }

    // Moves the scroll so the thumb sits where the pointer has taken it. The
    // grab offset is what keeps the point of the thumb that was grabbed under
    // the pointer for the whole drag rather than only at the moment of the
    // press.
    void drag_to(float along) {
        const bool horizontal = dragging.horizontal;
        const float to = scroll_to_thumb(horizontal, along - grab_offset, view());
        if (horizontal) {
            scroll_x = to;
        } else {
            scroll_row = to;
        }
        InvalidateRect(hwnd, nullptr, FALSE);
    }

    // The sideways scroll, held to the width the columns now add up to. Called
    // by everything that changes those widths: the scroll was clamped against a
    // total that no longer exists, and a grid left scrolled past its own content
    // shows a strip of canvas where the last column used to be.
    void hold_scroll_x() {
        const float most = max_scroll_x(view());
        if (scroll_x > most) {
            scroll_x = most;
        }
    }

    void resize_to(float x) {
        set_column_width(&columns, resize_column, resize_width + (x - resize_from));
        hold_scroll_x();
        InvalidateRect(hwnd, nullptr, FALSE);
    }

    void scroll_by(float rows_by, float dips_by) {
        const View at = view();
        scroll_row += rows_by;
        const float most_row = max_scroll_row(at);
        scroll_row = scroll_row > most_row ? most_row : (scroll_row < 0.0f ? 0.0f : scroll_row);
        scroll_x += dips_by;
        const float most_x = max_scroll_x(at);
        scroll_x = scroll_x > most_x ? most_x : (scroll_x < 0.0f ? 0.0f : scroll_x);
        InvalidateRect(hwnd, nullptr, FALSE);
    }

    // Physical pixels, which is what a click carries once the process is
    // per-monitor aware, divided into the DIPs everything else here is in. The
    // same number `SetDpi` was given, and the reason the hit test can be written
    // in the units the layout was.
    float dips() const {
        const UINT dpi = GetDpiForWindow(hwnd);
        return dpi == 0 ? 1.0f : 96.0f / static_cast<float>(dpi);
    }

    // The selection, on the clipboard. Nothing selected is not a failure: there
    // is no cursor until something has been pointed at or arrowed to, and a
    // grid that reported an error for Ctrl+C pressed on a fresh result would be
    // answering a question nobody asked.
    void copy() {
        if (!selected || columns.empty()) {
            return;
        }
        put_on_clipboard(hwnd, clipboard_text(columns, selection));
    }

    // Opens a box over the cursor's cell.
    //
    // `typed` is the character that opened it, where a character did. It
    // replaces the value rather than joining it — which is what typing over a
    // selected value means everywhere else — and the caret goes after it, or the
    // second keystroke would delete the first. Opened by Return or a double
    // click instead, the whole value is selected, so the first thing typed
    // replaces it either way.
    void begin_edit(const std::wstring* typed) {
        if (editor != nullptr || !selected || columns.empty()) {
            return;
        }
        const size_t c = static_cast<size_t>(selection.column);
        if (selection.column < 0 || c >= columns.size() || selection.row < 0
            || static_cast<size_t>(selection.row) >= rows()) {
            return;
        }
        // Brought into view first, and painted, because the cursor can be on a
        // row the last scroll left behind — Ctrl+A and End both put it there —
        // and the answer wanted then is the cell, not a box off the bottom edge.
        const View at = view();
        scroll_row = scroll_to_visible(at, selection.row);
        scroll_x = scroll_x_to_visible(at, columns, selection.column);
        InvalidateRect(hwnd, nullptr, FALSE);
        UpdateWindow(hwnd);

        const RECT cell = cell_rect(selection);
        // Right-aligned over a column of numbers, for the reason the drawing is:
        // magnitudes only line up when the digits do, and a box that left-
        // aligned what the cell right-aligns would move the value as it opened.
        //
        // One line that scrolls rather than wraps: a value longer than its
        // column is a value being scrolled through, not a cell that grew a
        // second line.
        const DWORD align = columns[c].numeric ? ES_RIGHT : ES_LEFT;
        editor = CreateWindowExW(0, L"EDIT", nullptr,
                                 WS_CHILD | WS_VISIBLE | ES_AUTOHSCROLL | align, cell.left,
                                 cell.top, cell.right - cell.left, cell.bottom - cell.top, hwnd,
                                 nullptr, GetModuleHandleW(nullptr), nullptr);
        if (editor == nullptr) {
            failed("CreateWindowExW for the cell editor", HRESULT_FROM_WIN32(GetLastError()));
            return;
        }
        editing = selection;
        ending = false;

        open_editor_font();
        SendMessageW(editor, WM_SETFONT, reinterpret_cast<WPARAM>(editor_font), TRUE);
        // The text laid out where the grid draws it. `EDIT` starts its own a
        // couple of pixels in from the left and centres it vertically; the grid
        // starts at `kCellPadding` and `kCellTextY`. Without this the characters
        // jump the moment the box opens, which is the one thing a box over a
        // drawn cell must not do — the whole illusion is that the cell became
        // typeable. `InsetTextFieldCell` is the same answer on the other side.
        const float scale = dips();
        RECT text{static_cast<LONG>(kCellPadding / scale), static_cast<LONG>(kCellTextY / scale),
                  cell.right - cell.left - static_cast<LONG>(kCellPadding / scale),
                  cell.bottom - cell.top};
        SendMessageW(editor, EM_SETRECT, 0, reinterpret_cast<LPARAM>(&text));

        const std::wstring seed = typed != nullptr ? *typed : edit_seed(columns, selection);
        SetWindowTextW(editor, seed.c_str());
        SetWindowLongPtrW(editor, GWLP_USERDATA, reinterpret_cast<LONG_PTR>(this));
        edit_base_proc = reinterpret_cast<WNDPROC>(
            SetWindowLongPtrW(editor, GWLP_WNDPROC, reinterpret_cast<LONG_PTR>(editor_proc)));
        SetFocus(editor);
        if (typed != nullptr) {
            SendMessageW(editor, EM_SETSEL, seed.size(), seed.size());
        } else {
            // The whole value selected with the caret at its start rather than
            // its end, which is what the arguments are the other way round for.
            // A value wider than its column is scrolled to the caret, and a box
            // that opened on the tail of a string the grid was showing the head
            // of moves the text the moment it appears — the same jump the inset
            // above exists to prevent, only further.
            // A value wider than its column opens showing its end rather than
            // its start, and nothing here changes that: an `EDIT` puts the caret
            // at the far end of a selection whichever order the two positions
            // are given in, and scrolls to it. Reversing the arguments,
            // `EM_SCROLLCARET` and `WM_HSCROLL` with `SB_LEFT` were each tried
            // and each left the view at the tail. Home brings it back.
            // `InlineCellEditor.swift` does not have this to deal with, because
            // AppKit scrolls to the start of a selection rather than to the
            // caret — so the two sides differ here, on the long values only.
            SendMessageW(editor, EM_SETSEL, 0, -1);
        }
    }

    // Keeps what was typed. `step` is where the cursor goes afterwards: nowhere
    // for Return and for a click elsewhere — somebody who has just fixed a value
    // is as likely to look at it as to move on — and one column either way for
    // Tab.
    void commit_edit(int step, bool take_focus) {
        if (editor == nullptr || ending) {
            return;
        }
        ending = true;
        // Room for the terminator as well as the text, because that is what
        // `GetWindowTextW` is counting, and then cut back to the characters.
        const int length = GetWindowTextLengthW(editor);
        std::wstring text(static_cast<size_t>(length) + 1, L'\0');
        if (length > 0) {
            GetWindowTextW(editor, &text[0], length + 1);
        }
        text.resize(static_cast<size_t>(length));
        const Selection at = editing;
        close_editor(take_focus);
        stage_edit(&columns, at, text);
        // The column keeps the width it had. Re-measuring here would move every
        // column after this one while somebody is typing down a column, and a
        // grid that reflowed under the caret would be unusable for the one task
        // this exists for.
        selection = at;
        selected = true;
        int column = 0;
        if (step != 0 && next_edit_column(at.column, step, static_cast<int>(columns.size()) - 1,
                                          &column)) {
            selection.column = column;
            // The band goes, the way an unshifted arrow drops it: a Tab is a
            // move, and a move collapses whatever was extended.
            selection.anchored = false;
            const View to = view();
            scroll_x = scroll_x_to_visible(to, columns, selection.column);
            InvalidateRect(hwnd, nullptr, FALSE);
            begin_edit(nullptr);
            return;
        }
        InvalidateRect(hwnd, nullptr, FALSE);
    }

    // Throws away whatever was being typed. A different answer from the one
    // above rather than a quieter version of it: one is a value, the other is
    // "forget I typed that".
    void cancel_edit() {
        if (editor == nullptr || ending) {
            return;
        }
        ending = true;
        close_editor(true);
        InvalidateRect(hwnd, nullptr, FALSE);
    }

    // Takes the box away, and puts the keyboard back on the grid where the grid
    // is where it should go.
    //
    // It should not when the box is closing *because* the focus left: something
    // else has it by then — another window, or this one after a click on another
    // cell — and taking it back would be this grid arguing with the click that
    // just happened.
    void close_editor(bool take_focus) {
        HWND field = editor;
        editor = nullptr;
        if (field != nullptr) {
            DestroyWindow(field);
        }
        if (take_focus) {
            SetFocus(hwnd);
        }
    }

    // One font for the boxes, at this window's scale. Built on demand rather
    // than with the window because it depends on the DPI, which can change while
    // the window is open.
    void open_editor_font() {
        if (editor_font != nullptr) {
            DeleteObject(editor_font);
        }
        const UINT dpi = GetDpiForWindow(hwnd);
        editor_font = CreateFontW(-MulDiv(static_cast<int>(kFontSize),
                                          static_cast<int>(dpi == 0 ? 96 : dpi), 96),
                                  0, 0, 0, FW_NORMAL, FALSE, FALSE, FALSE, DEFAULT_CHARSET,
                                  OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS, CLEARTYPE_QUALITY,
                                  FIXED_PITCH | FF_MODERN, L"Consolas");
    }

    // What the box paints itself with. The grid's own canvas colour, so the box
    // is invisible as a box: what says the cell is live is the caret in it, and a
    // rectangle in some other colour would read as a second kind of selection
    // sitting inside the first.
    //
    // Kept rather than made per paint, and dropped by `follow_appearance` when
    // the colour it was made from stops being the colour.
    HBRUSH editor_background() {
        if (editor_brush == nullptr) {
            editor_brush = CreateSolidBrush(colorref(tones().canvas));
        }
        return editor_brush;
    }

    RECT cell_rect(const Selection& at) const {
        const Column& column = columns[static_cast<size_t>(at.column)];
        const float x = column.x - scroll_x;
        const float y = kHeaderHeight + (static_cast<float>(at.row) - scroll_row) * kRowHeight;
        const float scale = dips();
        return RECT{static_cast<LONG>(x / scale), static_cast<LONG>(y / scale),
                    static_cast<LONG>((x + column.width) / scale),
                    static_cast<LONG>((y + kRowHeight) / scale)};
    }

    // Every row of the column the cursor is in.
    //
    // The view is left where it is, unlike every key that moves the cursor:
    // selecting everything is not going anywhere, and a grid that jumped to the
    // last row would take the reader off the rows they were looking at to show
    // them the end of a band they can already see they have.
    void select_all() {
        const int last_row = static_cast<int>(rows()) - 1;
        if (last_row < 0) {
            return;
        }
        selection = all_rows(selected ? selection : Selection{}, last_row);
        selected = true;
        InvalidateRect(hwnd, nullptr, FALSE);
    }

    // The three renderings, offered by name at a point on the screen.
    //
    // `TPM_RETURNCMD` hands the choice back here rather than posting
    // `WM_COMMAND`: this window has no command table, and adding one would put
    // the three items and the three answers in two places that have to agree.
    void show_menu(POINT screen, const Selection& on) {
        HMENU menu = CreatePopupMenu();
        if (menu == nullptr) {
            failed("CreatePopupMenu", HRESULT_FROM_WIN32(GetLastError()));
            return;
        }
        const std::wstring rows = row_count_phrase(on.last_row() - on.first_row() + 1);
        AppendMenuW(menu, MF_STRING, kCopyValue, L"Copy Value");
        AppendMenuW(menu, MF_STRING, kCopyRows, (L"Copy " + rows).c_str());
        AppendMenuW(menu, MF_STRING, kCopyRowsAsCsv, (L"Copy " + rows + L" as CSV").c_str());

        // Under a separator, because the three above copy what is on screen and
        // these change what is on screen. Named after the column rather than
        // after the value: the value is under the pointer, and the column is the
        // thing the item is about that the click does not already say.
        const size_t at = static_cast<size_t>(on.column);
        if (at < columns.size()) {
            AppendMenuW(menu, MF_SEPARATOR, 0, nullptr);
            AppendMenuW(menu, MF_POPUP, reinterpret_cast<UINT_PTR>(filter_submenu(false)),
                        (L"Filter on " + columns[at].heading).c_str());
            AppendMenuW(menu, MF_POPUP, reinterpret_cast<UINT_PTR>(filter_submenu(true)),
                        L"Add to Filter");
        }
        // Only where there is one to take off. An item that does nothing is a
        // question the reader has to answer for themselves every time they open
        // the menu.
        if (!filters.empty()) {
            AppendMenuW(menu, MF_STRING, kClearFilter, L"Clear Filter");
        }

        const int chosen = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, screen.x, screen.y,
                                          0, hwnd, nullptr);
        // Which takes the two submenus with it: they were attached to this one.
        DestroyMenu(menu);

        Filter op = Filter::Equals;
        bool extend = false;
        if (filter_chosen(chosen, &op, &extend)) {
            filter_by(op, extend, on);
            return;
        }
        switch (chosen) {
        case kCopyValue:
            // The cursor's own row rather than the band's first, because this
            // item names one cell and the cursor is which one.
            if (at < columns.size()) {
                put_on_clipboard(hwnd, copied_value(columns[at], on.row));
            }
            break;
        case kCopyRows:
            put_on_clipboard(hwnd, tab_separated(columns, on.first_row(), on.last_row()));
            break;
        case kCopyRowsAsCsv:
            put_on_clipboard(hwnd, csv_rows(columns, on.first_row(), on.last_row()));
            break;
        case kClearFilter:
            clear_filters();
            break;
        default:
            // Dismissed. Not an error and not a copy — the clipboard keeps
            // whatever was on it.
            break;
        }
    }

    // The way back out of a filter that matched nothing.
    //
    // Every other menu here opens on a cell, and a result with no rows has none:
    // the filter that emptied the grid would be the one thing that could not be
    // taken off, which is a trap this feature would otherwise build. So an empty
    // result answers the right button with the single item that ends it.
    void show_filter_exit(POINT screen) {
        HMENU menu = CreatePopupMenu();
        if (menu == nullptr) {
            failed("CreatePopupMenu", HRESULT_FROM_WIN32(GetLastError()));
            return;
        }
        AppendMenuW(menu, MF_STRING, kClearFilter, L"Clear Filter");
        const int chosen = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, screen.x, screen.y,
                                          0, hwnd, nullptr);
        DestroyMenu(menu);
        if (chosen == static_cast<int>(kClearFilter)) {
            clear_filters();
        }
    }

    bool is_empty_and_filtered() const { return rows() == 0 && !filters.empty(); }

    // Where the cursor's cell is on screen, for a menu asked for with the
    // keyboard. It opens at the cell rather than at the pointer, which may be
    // anywhere on the desktop or on another display entirely.
    //
    // The cell's lower leading corner, which is where Windows drops a menu from
    // a control: a menu whose top-left is the cell's top-left covers the cell it
    // was opened on.
    POINT menu_point_of(const Selection& on) const {
        const size_t at = static_cast<size_t>(on.column);
        const float x = at < columns.size() ? columns[at].x - scroll_x : 0.0f;
        const float y = kHeaderHeight + (static_cast<float>(on.row) - scroll_row + 1.0f) * kRowHeight;
        const float scale = dips();
        POINT corner{static_cast<LONG>(x / scale), static_cast<LONG>(y / scale)};
        // Held inside the window, because the cursor need not be: Ctrl+A puts it
        // on the last row without moving the view, and a menu opened at a point
        // below the window has come loose from the thing it acts on.
        RECT client{};
        GetClientRect(hwnd, &client);
        corner.x = corner.x < client.left ? client.left
                                          : (corner.x > client.right ? client.right : corner.x);
        corner.y = corner.y < client.top ? client.top
                                        : (corner.y > client.bottom ? client.bottom : corner.y);
        ClientToScreen(hwnd, &corner);
        return corner;
    }

    // A key arrived with the grid focused. `key_moves` is where the keys differ
    // from one another; this is the part that is the same for all of them.
    void key(WPARAM pressed, bool extend) {
        const int last_row = static_cast<int>(rows()) - 1;
        const int last_column = static_cast<int>(columns.size()) - 1;
        if (last_row < 0 || last_column < 0) {
            return;
        }
        // A key with nothing selected acts from the first cell rather than
        // doing nothing, which is what `AppController.swift` does when the
        // renderer's selection is nil. Ignoring it instead would leave a click
        // as the only way into the grid, and a result opened from the keyboard
        // would have eight keys that appeared not to work.
        Selection next;
        if (!key_moves(pressed, selected ? selection : Selection{}, view(), extend, last_row,
                       last_column, &next)) {
            return;
        }
        selection = next;
        selected = true;
        // The cursor takes the view with it, on whichever axis it moved. A grid
        // that let the cursor leave the screen would answer every further key
        // by moving something the user cannot see, and the only way back would
        // be to guess how far.
        const View at = view();
        scroll_row = scroll_to_visible(at, selection.row);
        scroll_x = scroll_x_to_visible(at, columns, selection.column);
        InvalidateRect(hwnd, nullptr, FALSE);
    }

    // Built here rather than once at startup, because Direct2D can lose the
    // device underneath a living window — a driver update, a remote session, a
    // GPU reset — and says so by answering `EndDraw` with D2DERR_RECREATE_TARGET
    // instead of by failing anything. Dropping the target there and coming back
    // through here is the whole of the recovery. A window without it goes on
    // running and stops drawing, after an event the user did not cause and
    // cannot connect to what they see.
    //
    // The brushes are rebuilt alongside it and not before: a brush belongs to
    // the target it was made from, and keeping one across a recreation is a use
    // of a dead resource that Direct2D reports as a blank window.
    bool ready() {
        if (target) {
            return true;
        }
        RECT client{};
        GetClientRect(hwnd, &client);
        HRESULT hr = d2d->CreateHwndRenderTarget(
            D2D1::RenderTargetProperties(),
            D2D1::HwndRenderTargetProperties(
                hwnd, D2D1::SizeU(static_cast<UINT32>(client.right - client.left),
                                  static_cast<UINT32>(client.bottom - client.top))),
            &target);
        if (FAILED(hr)) {
            return failed("CreateHwndRenderTarget", hr);
        }

        // The size above is in physical pixels — that is what a client rect is
        // once the process is per-monitor aware — and this is what tells Direct2D
        // how many of them a DIP is worth. Everything drawn afterwards is in
        // DIPs, which is the unit every constant at the top of this file is in,
        // so this one call is what makes a row twenty of anything at all.
        const float dpi = static_cast<float>(GetDpiForWindow(hwnd));
        target->SetDpi(dpi, dpi);

        return palette.open(target.Get(), tones());
    }

    void paint() {
        if (!ready()) {
            return;
        }
        target->BeginDraw();
        draw_grid(target.Get(), dwrite.Get(), font, columns, palette,
                  selected ? &selection : nullptr, sorted ? &sort : nullptr, scroll_row, scroll_x,
                  dragging);
        const HRESULT hr = target->EndDraw();
        if (hr == D2DERR_RECREATE_TARGET) {
            target.Reset();
            palette = Palette();
            InvalidateRect(hwnd, nullptr, FALSE);
        } else if (FAILED(hr)) {
            failed("ID2D1HwndRenderTarget::EndDraw", hr);
        }
    }

    void resize() {
        if (!target) {
            return;
        }
        RECT client{};
        GetClientRect(hwnd, &client);
        target->Resize(D2D1::SizeU(static_cast<UINT32>(client.right - client.left),
                                   static_cast<UINT32>(client.bottom - client.top)));
    }
};

// The three keys that end an edit, in front of an `EDIT` that would otherwise
// treat two of them as text and beep at the third. Everything else — the
// arrows, Home, End, the selection, the undo an `EDIT` already has — is handed
// straight back, which is the point of subclassing one rather than drawing a
// caret by hand.
LRESULT CALLBACK editor_proc(HWND field, UINT message, WPARAM wparam, LPARAM lparam) {
    auto* window = reinterpret_cast<Window*>(GetWindowLongPtrW(field, GWLP_USERDATA));
    if (window != nullptr) {
        switch (message) {
        case WM_KEYDOWN:
            switch (wparam) {
            case VK_RETURN:
                window->commit_edit(0, true);
                return 0;
            case VK_TAB:
                // Along the row and on into the next cell's box, so a row can be
                // corrected without reaching for the mouse between fields.
                window->commit_edit((GetKeyState(VK_SHIFT) & 0x8000) != 0 ? -1 : 1, true);
                return 0;
            case VK_ESCAPE:
                window->cancel_edit();
                return 0;
            default:
                break;
            }
            break;

        // The same three again as characters, because `TranslateMessage` makes
        // one of each and a single-line `EDIT` answers Return and Escape with
        // `MessageBeep`. The keystroke has already done what it was for; letting
        // the character through as well would be a noise at every commit.
        case WM_CHAR:
            if (wparam == VK_RETURN || wparam == VK_TAB || wparam == VK_ESCAPE) {
                return 0;
            }
            break;

        // The focus left some other way: a click on another cell, on another
        // window, or Alt+Tab. Kept rather than thrown away — nothing is sent
        // anywhere from here, so keeping it costs a mark on one cell and losing
        // it costs whatever was typed. `InlineCellEditor.swift` ends the same
        // way on the same event.
        case WM_KILLFOCUS:
            window->commit_edit(0, false);
            return 0;

        default:
            break;
        }
    }
    return CallWindowProcW(edit_base_proc, field, message, wparam, lparam);
}

LRESULT CALLBACK window_proc(HWND hwnd, UINT message, WPARAM wparam, LPARAM lparam) {
    if (message == WM_NCCREATE) {
        auto* created = reinterpret_cast<CREATESTRUCTW*>(lparam);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA,
                          reinterpret_cast<LONG_PTR>(created->lpCreateParams));
        return DefWindowProcW(hwnd, message, wparam, lparam);
    }

    auto* window = reinterpret_cast<Window*>(GetWindowLongPtrW(hwnd, GWLP_USERDATA));
    if (window == nullptr) {
        return DefWindowProcW(hwnd, message, wparam, lparam);
    }

    switch (message) {
    case WM_PAINT: {
        PAINTSTRUCT paint{};
        BeginPaint(hwnd, &paint);
        window->paint();
        EndPaint(hwnd, &paint);
        return 0;
    }

    // Claimed and ignored. The default handler fills the client area with the
    // class brush first, and every frame would then be a flash of that colour
    // before Direct2D clears over it — visible as flicker while resizing, which
    // is the moment the redraws come fastest.
    case WM_ERASEBKGND:
        return 1;

    case WM_SIZE:
        window->resize();
        return 0;

    case WM_LBUTTONDOWN: {
        // The window takes focus on the way past. A grid that could be clicked
        // but not then typed into would look broken in a way that has nothing to
        // do with either the click or the key.
        SetFocus(hwnd);
        const float scale = window->dips();
        const float x = static_cast<float>(GET_X_LPARAM(lparam)) * scale;
        const float y = static_cast<float>(GET_Y_LPARAM(lparam)) * scale;

        // Before the cell, and returning: a press in a gutter is for that bar,
        // and must not also land on whatever row is underneath it. Both bars
        // are asked at once, because in the corner where the two gutters meet
        // only one of them can have the press.
        const View at = window->view();
        bool horizontal = false;
        Scrollbar bar;
        if (scrollbar_axis_at(x, y, at, &horizontal) && scrollbar_of(horizontal, at, &bar)) {
            const float along = horizontal ? x : y;
            const bool on_thumb =
                along >= bar.thumb_start && along <= bar.thumb_start + bar.thumb_length;
            // A press on the track goes where it points rather than paging
            // towards it. On four hundred thousand rows, paging there is an
            // afternoon's work with the mouse button held down.
            window->grab_offset = on_thumb ? along - bar.thumb_start : bar.thumb_length / 2.0f;
            window->dragging = horizontal ? Bars{false, true} : Bars{true, false};
            // Captured, so a drag that wanders off the side of the window keeps
            // scrolling instead of stopping at the edge and letting go silently.
            SetCapture(hwnd);
            window->drag_to(along);
            return 0;
        }

        // A boundary before the band it sits in, because here the two answers
        // really do overlap and one of them has to win. The edge wins:
        // `AppController.swift` asks in this order, and the handle is eight
        // DIPs of a heading that is seventy-seven wide — a press on it is much
        // more likely to be aimed at the line than at the name.
        int edge = 0;
        if (column_edge_at(x, y, window->scroll_x, window->columns, &edge)) {
            window->resizing = true;
            window->resize_column = static_cast<size_t>(edge);
            window->resize_from = x;
            window->resize_width = window->columns[static_cast<size_t>(edge)].width;
            SetCapture(hwnd);
            return 0;
        }

        // The band sorts rather than selects, and it is asked first because the
        // two areas do not overlap: `cell_at` refuses everything above the
        // fold, so the order here is about which answer is looked for, not
        // about which one wins.
        int heading = 0;
        if (header_column_at(x, y, window->scroll_x, window->columns, &heading)) {
            window->sort_by(heading);
            return 0;
        }

        Selection hit;
        if (cell_at(x, y, at, window->columns, &hit)) {
            window->selection = hit;
            window->selected = true;
            InvalidateRect(hwnd, nullptr, FALSE);
        }
        return 0;
    }

    // Opens the cell under the pointer. The press before it has already put the
    // cursor there, and this asks again anyway: the same two clicks over a
    // heading are a sort and then nothing, and a box opened on the strength of
    // the second would appear over whichever cell the cursor was last left on.
    case WM_LBUTTONDBLCLK: {
        const float scale = window->dips();
        Selection hit;
        if (pointed_cell(static_cast<float>(GET_X_LPARAM(lparam)) * scale,
                         static_cast<float>(GET_Y_LPARAM(lparam)) * scale, window->view(),
                         window->columns, nullptr, false, &hit)) {
            window->selection = hit;
            window->selected = true;
            window->begin_edit(nullptr);
        }
        return 0;
    }

    // The box paints itself with the grid's own colours rather than the system's
    // window colours, which are white text on white here as often as not: an
    // `EDIT` asks its parent for them, and this window is the only thing that
    // knows which appearance the grid is drawn in.
    case WM_CTLCOLOREDIT: {
        auto dc = reinterpret_cast<HDC>(wparam);
        SetTextColor(dc, colorref(window->tones().ink));
        SetBkColor(dc, colorref(window->tones().canvas));
        return reinterpret_cast<LRESULT>(window->editor_background());
    }

    // Three rows a notch, `AppController.swift`'s multiplier. One row would be
    // an accurate wheel and a useless one: a result is read in pages, and a
    // notch that moved a single line would need forty of them to cross a screen.
    //
    // The distance is kept as a fraction rather than rounded to rows, so that a
    // trackpad — which sends many small deltas rather than a few whole notches —
    // scrolls smoothly instead of standing still until the deltas add up to one.
    case WM_MOUSEMOVE:
        if (window->dragging.vertical) {
            window->drag_to(static_cast<float>(GET_Y_LPARAM(lparam)) * window->dips());
        } else if (window->dragging.horizontal) {
            window->drag_to(static_cast<float>(GET_X_LPARAM(lparam)) * window->dips());
        } else if (window->resizing) {
            window->resize_to(static_cast<float>(GET_X_LPARAM(lparam)) * window->dips());
        }
        return 0;

    // The pointer says what the press would do before it is made. Answered here
    // rather than by the window class, because the class cursor is one cursor
    // for the whole client area and this one changes with where it is.
    //
    // Only over the client area: `LOWORD(lparam)` is the hit-test code, and
    // claiming the others would take the arrow off the window's own borders.
    case WM_SETCURSOR:
        if (LOWORD(lparam) == HTCLIENT) {
            POINT at{};
            int over = 0;
            if (GetCursorPos(&at) && ScreenToClient(hwnd, &at)
                && column_edge_at(static_cast<float>(at.x) * window->dips(),
                                  static_cast<float>(at.y) * window->dips(), window->scroll_x,
                                  window->columns, &over)) {
                SetCursor(LoadCursorW(nullptr, IDC_SIZEWE));
                return TRUE;
            }
        }
        return DefWindowProcW(hwnd, message, wparam, lparam);

    // The capture is released whichever way the button comes up, including the
    // one where another window takes it away — a drag left engaged would leave
    // the thumb dark and the grid following a pointer nobody is pressing.
    case WM_LBUTTONUP:
    case WM_CAPTURECHANGED:
        if (window->dragging.vertical || window->dragging.horizontal || window->resizing) {
            window->dragging = Bars{};
            window->resizing = false;
            if (message == WM_LBUTTONUP) {
                ReleaseCapture();
            }
            InvalidateRect(hwnd, nullptr, FALSE);
        }
        return 0;

    case WM_MOUSEWHEEL: {
        const float notches = static_cast<float>(GET_WHEEL_DELTA_WPARAM(wparam))
                              / static_cast<float>(WHEEL_DELTA);
        // Shift turns the wheel sideways, which is what a Windows control with
        // a horizontal bar does and the only way across for a mouse with one
        // wheel. `AppController.swift` has no equivalent because a trackpad
        // hands it both axes; here the tilt wheel that would is `WM_MOUSEHWHEEL`
        // below, and most mice do not have one.
        if ((GetKeyState(VK_SHIFT) & 0x8000) != 0) {
            window->scroll_by(0.0f, -notches * kWheelRows * kRowHeight);
        } else {
            window->scroll_by(-notches * kWheelRows, 0.0f);
        }
        return 0;
    }

    // A notch sideways covers the same distance a notch down does, so the two
    // axes feel like one gesture. There is no natural unit here the way there
    // is vertically — sideways a notch is not a column, because columns are not
    // all one width.
    case WM_MOUSEHWHEEL:
        window->scroll_by(0.0f, static_cast<float>(GET_WHEEL_DELTA_WPARAM(wparam))
                                    / static_cast<float>(WHEEL_DELTA) * kWheelRows * kRowHeight);
        return 0;

    // Claimed, so the arrows arrive here rather than being taken for dialog
    // navigation. There is nothing to tab between yet, and the grid is the only
    // thing in the window that an arrow key could mean anything to.
    case WM_GETDLGCODE:
        return DLGC_WANTARROWS;

    // In screen coordinates, and (-1, -1) where it was the keyboard — Shift+F10
    // or the menu key — rather than the pointer. Those act on the cell the
    // cursor is already on, which is the only cell they could mean.
    case WM_CONTEXTMENU: {
        POINT screen{GET_X_LPARAM(lparam), GET_Y_LPARAM(lparam)};
        if (screen.x == -1 && screen.y == -1) {
            if (window->selected) {
                window->show_menu(window->menu_point_of(window->selection), window->selection);
            } else if (window->is_empty_and_filtered()) {
                // From the top corner, there being no cell to open it on.
                POINT corner{0, 0};
                ClientToScreen(hwnd, &corner);
                window->show_filter_exit(corner);
            }
            return 0;
        }

        POINT client = screen;
        ScreenToClient(hwnd, &client);
        const float scale = window->dips();
        Selection on;
        if (!pointed_cell(static_cast<float>(client.x) * scale, static_cast<float>(client.y) * scale,
                       window->view(), window->columns,
                       window->selected ? &window->selection : nullptr,
                       (GetKeyState(VK_SHIFT) & 0x8000) != 0, &on)) {
            if (window->is_empty_and_filtered()) {
                window->show_filter_exit(screen);
            }
            return 0;
        }
        SetFocus(hwnd);
        window->selection = on;
        window->selected = true;
        // Painted before the menu opens rather than left for the next idle, so
        // the menu and the cursor agree about which cell is being acted on.
        // `TrackPopupMenu` runs a loop of its own, and a grid that showed the
        // move only after the menu closed would be offering to copy a cell
        // nothing on screen points at.
        InvalidateRect(hwnd, nullptr, FALSE);
        UpdateWindow(hwnd);
        window->show_menu(screen, on);
        return 0;
    }

    // Windows announces every setting the same way, so the string is the only
    // thing that says this one is about colours. Broadcast to every window on
    // the desktop, which is why the handler re-reads rather than assuming the
    // appearance is the one that changed.
    case WM_SETTINGCHANGE:
        if (lparam != 0
            && wcscmp(reinterpret_cast<const wchar_t*>(lparam), L"ImmersiveColorSet") == 0) {
            window->follow_appearance();
        }
        return 0;

    // One handler for every key, because every key this grid has either moves
    // the cursor or copies it, and `key_moves` is where the movers differ.
    // Claimed whether or not the key was one of them, which is what this did
    // when there were four: there is nothing else in the window for a keystroke
    // to reach.
    //
    // Control is asked first and answers for itself. A modified key is a
    // command rather than a direction — Ctrl+End means the end of the result in
    // some grids and nothing in this one, and letting it through to `key_moves`
    // would make it silently mean End. `AppController.swift` puts ⌘ in front of
    // its own switch for the same reason.
    case WM_KEYDOWN:
        if ((GetKeyState(VK_CONTROL) & 0x8000) != 0) {
            if (wparam == 'C') {
                window->copy();
            } else if (wparam == 'A') {
                window->select_all();
            }
            return 0;
        }
        // Return opens the cursor's cell with its value selected, which is the
        // key every grid with an editor uses for it and the one somebody who has
        // arrowed to a cell already has a finger near.
        if (wparam == VK_RETURN) {
            window->begin_edit(nullptr);
            return 0;
        }
        window->key(wparam, (GetKeyState(VK_SHIFT) & 0x8000) != 0);
        return 0;

    // Typing over a cell opens it and keeps the character, rather than needing
    // Return first: a value that has to be announced before it can be typed is a
    // value people mistype, having started before the box was there.
    //
    // Anything below a space is a command rather than a value — Return, Tab and
    // Escape all arrive here as characters too, as does every Ctrl+letter — and
    // 0x7F is Ctrl+Backspace, which is a deletion and not a value either.
    case WM_CHAR: {
        const auto typed = static_cast<wchar_t>(wparam);
        if (typed >= L' ' && typed != 0x7F) {
            const std::wstring one(1, typed);
            window->begin_edit(&one);
        }
        return 0;
    }

    // The window has moved to a display with a different scale. Windows offers a
    // rectangle for where it should now sit; taking it is what keeps the window
    // the same physical size across the move rather than the same pixel size.
    // The target is told the new scale so the DIPs below it keep their meaning.
    case WM_DPICHANGED: {
        const auto* suggested = reinterpret_cast<const RECT*>(lparam);
        SetWindowPos(hwnd, nullptr, suggested->left, suggested->top,
                     suggested->right - suggested->left, suggested->bottom - suggested->top,
                     SWP_NOZORDER | SWP_NOACTIVATE);
        if (window->target) {
            const float dpi = static_cast<float>(HIWORD(wparam));
            window->target->SetDpi(dpi, dpi);
        }
        return 0;
    }

    case WM_DESTROY:
        PostQuitMessage(0);
        return 0;

    default:
        return DefWindowProcW(hwnd, message, wparam, lparam);
    }
}

int show_the_grid_in_a_window() {
    // Before a window exists and before anything is asked how big it is. Without
    // this Windows scales the whole window for a high-DPI display, and text that
    // DirectWrite laid out from glyph metrics arrives through a bitmap scaler —
    // soft, and off by whatever the rounding was. That is the one thing a grid
    // whose entire argument is a measured advance must not have happen to it.
    SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

    // The console goes away before the window arrives. This is a console
    // subsystem binary because `--verify-grid` and the rest have to be able to
    // print, but the window mode has nothing to say on stdout, and leaving the
    // console open puts a black rectangle on the desktop beside the grid for as
    // long as the app runs.
    //
    // It also makes the app's window findable. `Start-Process ... MainWindowHandle`
    // returns whichever top-level window turned up first, so with a console in
    // the race a screenshot tool sometimes brings the console to the front and
    // types into it instead — which is how this was noticed.
    FreeConsole();

    Window window;
    HRESULT hr = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, __uuidof(ID2D1Factory),
                                   reinterpret_cast<void**>(window.d2d.GetAddressOf()));
    if (FAILED(hr)) {
        failed("D2D1CreateFactory", hr);
        return 1;
    }
    hr = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED, __uuidof(IDWriteFactory),
                             reinterpret_cast<IUnknown**>(window.dwrite.GetAddressOf()));
    if (FAILED(hr)) {
        failed("DWriteCreateFactory", hr);
        return 1;
    }
    if (!window.font.open(window.dwrite)) {
        return 1;
    }
    if (!load_grid(std::string(), std::string(), &window.columns)) {
        return 1;
    }
    lay_out(&window.columns, window.font.advance);

    WNDCLASSEXW window_class{};
    window_class.cbSize = sizeof(window_class);
    // Without `CS_DBLCLKS` Windows sends a second `WM_LBUTTONDOWN` instead of a
    // `WM_LBUTTONDBLCLK`, and the class has to say so: a window cannot ask for
    // double clicks after the fact.
    window_class.style = CS_DBLCLKS;
    window_class.lpfnWndProc = window_proc;
    window_class.hInstance = GetModuleHandleW(nullptr);
    window_class.hCursor = LoadCursorW(nullptr, IDC_ARROW);
    window_class.lpszClassName = L"DbClientGrid";
    if (RegisterClassExW(&window_class) == 0) {
        failed("RegisterClassExW", HRESULT_FROM_WIN32(GetLastError()));
        return 1;
    }

    // A size asked for in DIPs and converted, rather than in pixels, for the
    // reason everything else here is in DIPs: on a 200% display a window given
    // 900 pixels is half the window it was meant to be. `AdjustWindowRectEx`
    // then turns the client area that is wanted into the outer size that
    // produces it, which is what stops the title bar eating into the grid.
    //
    // Larger than the grid it holds, deliberately. This is not the shell — there
    // is no tab bar, no sidebar and no editor yet — and a window sized to fit
    // three columns exactly would look finished instead of looking like the one
    // piece that is.
    const UINT dpi = GetDpiForSystem();
    RECT bounds{0, 0, MulDiv(900, static_cast<int>(dpi), 96),
                MulDiv(600, static_cast<int>(dpi), 96)};
    AdjustWindowRectExForDpi(&bounds, WS_OVERLAPPEDWINDOW, FALSE, 0, dpi);

    window.hwnd = CreateWindowExW(0, window_class.lpszClassName, L"DBeaver", WS_OVERLAPPEDWINDOW,
                                  CW_USEDEFAULT, CW_USEDEFAULT, bounds.right - bounds.left,
                                  bounds.bottom - bounds.top, nullptr, nullptr,
                                  window_class.hInstance, &window);
    if (window.hwnd == nullptr) {
        failed("CreateWindowExW", HRESULT_FROM_WIN32(GetLastError()));
        return 1;
    }
    // Before it is shown, so the caption is the right colour the first time it
    // is drawn rather than repainting a moment after the window appears.
    window.is_light = windows_is_light();
    window.follow_frame();
    ShowWindow(window.hwnd, SW_SHOWNORMAL);

    MSG message{};
    while (GetMessageW(&message, nullptr, 0, 0) > 0) {
        TranslateMessage(&message);
        DispatchMessageW(&message);
    }
    return failures == 0 ? 0 : 1;
}

int report() {
    if (failures != 0) {
        std::printf("\n%d check(s) failed\n", failures);
        return 1;
    }
    std::printf("\nevery check passed\n");
    return 0;
}

}  // namespace

int main(int argc, char** argv) {
    const std::string flag = argc > 1 ? argv[1] : "";
    if (!flag.empty() && flag != "--verify-drivers" && flag != "--verify-grid") {
        std::printf("unknown option: %s\n", flag.c_str());
        std::printf("run with no arguments for the window, or --verify-drivers/--verify-grid\n");
        return 2;
    }

    // Apartment-threaded, which is what a window wants: COM delivers to it
    // through the message loop this thread is about to run. WIC is instantiated
    // on this thread too. Every call the core makes blocks, and none of them are
    // made here.
    const HRESULT hr = CoInitializeEx(nullptr, COINIT_APARTMENTTHREADED);
    if (FAILED(hr)) {
        std::printf("FAIL  CoInitializeEx: hr=0x%08lx\n", static_cast<unsigned long>(hr));
        return 1;
    }

    int status = 0;
    if (flag == "--verify-drivers") {
        the_driver_list_draws();
        status = report();
    } else if (flag == "--verify-grid") {
        the_grid_draws_a_result();
        status = report();
    } else {
        status = show_the_grid_in_a_window();
    }

    CoUninitialize();
    return status;
}

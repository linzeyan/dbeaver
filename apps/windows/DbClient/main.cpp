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
#include <dwrite.h>
#include <wincodec.h>
#include <wrl/client.h>

#include <cmath>
#include <cstdio>
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
// The header's second line, the declared type at 15, is not here. It needs the
// type the server declared, which travels beside the result rather than in the
// Arrow schema, and the Arrow kind is only its fallback. Anything invented to
// fill the gap would be this grid stating a type a column may not have, which
// `GridRenderer.swift` calls the one outcome worse than saying nothing. The band
// is 32 tall because that line is coming.
constexpr float kHeaderNameY = 2.0f;
constexpr float kCellTextY = 3.0f;

// The scrollbar's gutter is the target, not the paint. Twelve DIPs to hit and
// five to see: a thumb drawn at the width it can be grabbed at would be a bar
// down the side of the result rather than a mark on it, and one grabbable only
// where it is painted would be a five-DIP target.
//
// The floor on the thumb's length is the trade every platform makes. Sized
// purely by proportion it disappears on a million rows, and at that point it has
// stopped reporting how much there is and become something to take hold of.
constexpr float kScrollbarGutter = 12.0f;
constexpr float kScrollbarThumb = 5.0f;
constexpr float kMinThumbLength = 28.0f;

// `Theme.swift`'s light values, under the names it gives them. Dark mode is a
// separate question — it begins by asking Windows which one the user is in, not
// by writing a second set of numbers — and a front end that guessed at these
// would be a front end that looks nearly like the other one.
//
// `banding` and `separator` are one colour at two alphas rather than two
// colours: they are the ramp's direction at low strength, which is what keeps
// them correct over whatever they land on.
constexpr UINT32 kCanvas = 0xFFFFFF;      // Grid.background, Surface.canvas
constexpr UINT32 kHeaderBand = 0xF1F5F9;  // Grid.header, Surface.raised
constexpr UINT32 kInk = 0x1E293B;         // Grid.text
constexpr UINT32 kHeaderInk = 0x475569;   // Grid.headerText, Text.secondary
constexpr UINT32 kMutedInk = 0x51607A;    // Grid.nullText, Text.dataMuted
constexpr UINT32 kRule = 0x0F172A;
constexpr float kBandingAlpha = 0.030f;
constexpr float kSeparatorAlpha = 0.080f;
// Accent.selection, at the two strengths Grid.selectedRow and Grid.selectedCell
// use, and undiluted for Grid.cursor. Translucent so the value underneath stays
// readable — which is also why the text is drawn after them.
constexpr UINT32 kAccent = 0x4F46E5;
constexpr float kSelectedRowAlpha = 0.180f;
constexpr float kSelectedCellAlpha = 0.380f;
// Grid.scrollTrack, Grid.scrollThumb and Grid.scrollThumbActive — `kRule` again,
// at three strengths. The bar sits over the data rather than beside it, so the
// track is barely there and the thumb carries the whole signal.
constexpr float kScrollTrackAlpha = 0.040f;
constexpr float kScrollThumbAlpha = 0.180f;
constexpr float kScrollThumbActiveAlpha = 0.320f;

// What counts as a glyph when the bitmap is read back. See `Surface::ink_in`.
constexpr BYTE kInkThreshold = 0xB4;

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

// -------------------------------------------------------------------------
// The device, kept together because every check needs all of it
// -------------------------------------------------------------------------

struct Surface {
    ComPtr<ID2D1Factory> d2d;
    ComPtr<IDWriteFactory> dwrite;
    ComPtr<IWICImagingFactory> wic;
    ComPtr<IWICBitmap> bitmap;
    ComPtr<ID2D1RenderTarget> target;

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
    // Dark pixels rather than pixels that differ from the background, which is
    // what this counted before the grid had any chrome. A banded, ruled,
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
    // The threshold has room on both sides: the lightest thing this grid writes
    // is `Grid.nullText`, whose lightest channel is 0x7A, and the darkest thing
    // it fills is a separator over the header band, which lands near 0xE0.
    bool ink_in(float x0, float y0, float x1, float y1, UINT* out) const {
        UINT painted = 0;
        const bool read = each_pixel(x0, y0, x1, y1, [&painted](const BYTE* px) {
            // BGRA, premultiplied over an opaque clear, so the channels are the
            // colour. A glyph is the only thing here dark enough to put all
            // three under the threshold.
            if (px[0] < kInkThreshold && px[1] < kInkThreshold && px[2] < kInkThreshold) {
                painted += 1;
            }
        });
        *out = painted;
        return read;
    }

    // The darkest pixel in a rectangle, as its lightest channel.
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

struct Column {
    std::wstring heading;
    std::vector<std::wstring> cells;
    std::vector<bool> nulls;
    // A property of the type, not of any value in it. A column of numbers is
    // read by comparing magnitudes down it, and that only works when the units
    // line up; `GridRenderer.swift` asks the column's kind the same question.
    bool numeric = false;
    float x = 0.0f;
    float width = 0.0f;
};

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

// How far down the result the view has been taken, counted in rows.
//
// Fractional, like `GridRenderer.swift`'s `scrollRow`. A wheel notch is a
// distance rather than a row, and rounding one to whole rows turns a smooth
// gesture into a series of jumps — on a trackpad, where the notches are small
// and continuous, into a series of jumps that mostly do nothing.
//
// Rows that the grid draws at a given scroll position. One more than fits,
// because the first row on screen is usually only partly on it and so is the
// last: drawing exactly as many as fit leaves a gap along the bottom edge at
// every scroll position that is not a whole number of rows.
struct RowSpan {
    size_t first = 0;
    size_t last = 0;
};

RowSpan visible_rows(float scroll_row, float view_height, size_t rows) {
    RowSpan span;
    if (scroll_row > 0.0f) {
        span.first = static_cast<size_t>(scroll_row);
    }
    const float usable = view_height - kHeaderHeight;
    if (usable > 0.0f) {
        span.last = span.first + static_cast<size_t>(std::ceil(usable / kRowHeight)) + 1;
    }
    return span.last > rows ? RowSpan{span.first, rows} : span;
}

// Whole rows the view can show at once, which is not the same quantity as the
// fractional span below and is not interchangeable with it. This one answers
// "is that row on screen", so a row half under the bottom edge does not count.
// At least one, so that a view shorter than its own header still has somewhere
// to put the cursor.
float whole_visible_rows(float view_height) {
    const float span = std::floor((view_height - kHeaderHeight) / kRowHeight);
    return span < 1.0f ? 1.0f : span;
}

// The largest scroll that still fills the view. Past it the grid would show
// blank space under the last row, and a scrollbar built on it would reach the
// end of its track before the result ran out.
float max_scroll_row(float view_height, size_t rows) {
    const float usable = view_height - kHeaderHeight;
    const float span = usable > kRowHeight ? usable / kRowHeight : 1.0f;
    const float most = static_cast<float>(rows) - span;
    return most > 0.0f ? most : 0.0f;
}

// The smallest movement that brings a row into view: above the fold it becomes
// the top row, below it the last whole one. `GridRenderer.swift` again — a grid
// that centred the row instead would move the whole page for a one-row step,
// and the user would lose their place on every arrow key.
float scroll_to_visible(float scroll_row, int row, float view_height, size_t rows) {
    const float target = static_cast<float>(row);
    const float span = whole_visible_rows(view_height);
    float next = scroll_row;
    if (target < scroll_row) {
        next = target;
    } else if (target >= scroll_row + span - 1.0f) {
        next = target - span + 2.0f;
    }
    const float most = max_scroll_row(view_height, rows);
    if (next > most) {
        next = most;
    }
    return next < 0.0f ? 0.0f : next;
}

// Where the vertical scrollbar's track and thumb sit, in DIPs down the view.
//
// Only the vertical one. The horizontal bar in `GridRenderer.swift` shortens
// this one's track and is subtracted from the row span, and neither applies
// until this grid can scroll sideways — which it cannot, because every column
// it draws still fits. Written for one axis rather than for two with one of
// them permanently absent.
struct Scrollbar {
    float track_start = 0.0f;
    float track_length = 0.0f;
    float thumb_start = 0.0f;
    float thumb_length = 0.0f;
};

// Absent when the whole result already fits, which is the difference between a
// grid with nothing below the fold and a grid whose bar is pinned full-length
// and means nothing.
bool scrollbar_of(float scroll_row, float view_height, size_t rows, Scrollbar* out) {
    const float span = (view_height - kHeaderHeight) / kRowHeight;
    if (span <= 0.0f || static_cast<float>(rows) <= span) {
        return false;
    }
    out->track_start = kHeaderHeight;
    out->track_length = view_height - kHeaderHeight;
    // How much of the result is on screen, floored so it stays grabbable, and
    // capped at the track so a short result cannot ask for a thumb longer than
    // the space it runs in.
    const float proportional = out->track_length * span / static_cast<float>(rows);
    out->thumb_length = proportional < kMinThumbLength ? kMinThumbLength : proportional;
    if (out->thumb_length > out->track_length) {
        out->thumb_length = out->track_length;
    }
    // Along the travel the thumb has, not along the track: at the end of the
    // scroll the thumb's trailing edge is on the track's, and a fraction taken
    // of the track instead would leave it a thumb's length short.
    const float most = max_scroll_row(view_height, rows);
    float progress = most > 0.0f ? scroll_row / most : 0.0f;
    progress = progress < 0.0f ? 0.0f : (progress > 1.0f ? 1.0f : progress);
    out->thumb_start = out->track_start + (out->track_length - out->thumb_length) * progress;
    return true;
}

// The scroll that puts the thumb's leading edge at `thumb_start`. The inverse of
// the line above, and the whole of what a drag does.
float scroll_to_thumb(float thumb_start, float view_height, size_t rows) {
    Scrollbar bar;
    if (!scrollbar_of(0.0f, view_height, rows, &bar)) {
        return 0.0f;
    }
    const float travel = bar.track_length - bar.thumb_length;
    if (travel <= 0.0f) {
        return 0.0f;
    }
    float progress = (thumb_start - bar.track_start) / travel;
    progress = progress < 0.0f ? 0.0f : (progress > 1.0f ? 1.0f : progress);
    return progress * max_scroll_row(view_height, rows);
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
bool cell_at(float x, float y, float scroll_row, const std::vector<Column>& columns, size_t rows,
             Selection* out) {
    if (x < 0.0f || y < kHeaderHeight) {
        return false;
    }
    // Through the same scroll the drawing used, so the answer is the row the
    // user is looking at rather than the row that would be there unscrolled.
    const float at = scroll_row + (y - kHeaderHeight) / kRowHeight;
    if (at < 0.0f) {
        return false;
    }
    const auto row = static_cast<size_t>(at);
    if (row >= rows) {
        return false;
    }
    for (size_t c = 0; c < columns.size(); ++c) {
        if (x >= columns[c].x && x < columns[c].x + columns[c].width) {
            out->row = static_cast<int>(row);
            out->column = static_cast<int>(c);
            out->anchored = false;
            return true;
        }
    }
    return false;
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

// Int64 and utf8 only, which is what the query below produces.
//
// A real grid needs every type the drivers can return, and the macOS side has
// that; putting it here now would be writing a second copy of it against a
// surface with no window on it. This is the first brick, and what it has to
// prove is the geometry.
bool read_columns(DbHandle* handle, std::vector<Column>* out) {
    char* err = nullptr;
    int position = 0;
    DbQuery* query = db_query(handle,
                              "SELECT i AS id,"
                              "       'driver-' || i AS name,"
                              "       CASE WHEN i = 1 THEN NULL"
                              "            ELSE 'read ' || (i * 100) END AS note,"
                              // Long enough to be clamped to `kMaxColumnWidth`
                              // and then to overrun that, which is the only way
                              // to have anything to say about what happens to a
                              // value the column cannot hold. With spaces in it,
                              // because wrapping breaks at a space and trimming
                              // does not — a value without any would be cut in
                              // the same place either way and the check below
                              // would not be able to tell them apart.
                              "       'a value long enough to run past the widest"
                              " column this grid allows ' || i AS wide "
                              // More rows than the check's bitmap can show, so
                              // that scrolling has somewhere to go and so that
                              // "draws every row" and "draws the rows in view"
                              // stop being the same statement.
                              "FROM range(40) t(i) ORDER BY i",
                              1000, &err, &position);
    if (query == nullptr) {
        return core_failed("db_query", err);
    }

    ArrowSchema schema{};
    if (db_query_schema(query, &schema, &err) != 0) {
        db_query_free(query);
        return core_failed("db_query_schema", err);
    }
    ArrowArray batch{};
    if (db_query_next(query, &batch, &err) != 1) {
        schema.release(&schema);
        db_query_free(query);
        return core_failed("db_query_next", err);
    }

    for (int64_t c = 0; c < batch.n_children; ++c) {
        const ArrowSchema& field = *schema.children[c];
        const ArrowArray& values = *batch.children[c];
        const std::string format(field.format);
        Column column;
        column.heading = widen(field.name);
        column.numeric = format == "l";
        for (int64_t r = 0; r < batch.length; ++r) {
            const bool present = valid_at(values, r);
            column.nulls.push_back(!present);
            if (!present) {
                column.cells.push_back(kNullText);
            } else if (format == "u") {
                column.cells.push_back(widen(utf8_at(values, r)));
            } else if (format == "l") {
                const auto* numbers = static_cast<const int64_t*>(values.buffers[1]);
                column.cells.push_back(std::to_wstring(numbers[values.offset + r]));
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

    batch.release(&batch);
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

// `GridRenderer.swift`'s rule, transcribed: the widest of the heading and the
// cells, one character of slack so the longest value stays off the separator,
// padding either side, clamped. NULL counts as four because it renders as the
// word.
void lay_out(std::vector<Column>* columns, float advance) {
    float x = 0.0f;
    for (Column& column : *columns) {
        size_t chars = column.heading.size();
        for (const std::wstring& cell : column.cells) {
            chars = cell.size() > chars ? cell.size() : chars;
        }
        float width = kCellPadding * 2.0f + static_cast<float>(chars + 1) * advance;
        width = width < kMinColumnWidth ? kMinColumnWidth : width;
        width = width > kMaxColumnWidth ? kMaxColumnWidth : width;
        column.x = x;
        column.width = width;
        x += width;
    }
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
    ComPtr<ID2D1SolidColorBrush> header;
    ComPtr<ID2D1SolidColorBrush> banding;
    ComPtr<ID2D1SolidColorBrush> separator;
    ComPtr<ID2D1SolidColorBrush> selected_row;
    ComPtr<ID2D1SolidColorBrush> selected_cell;
    ComPtr<ID2D1SolidColorBrush> cursor;
    ComPtr<ID2D1SolidColorBrush> scroll_track;
    ComPtr<ID2D1SolidColorBrush> scroll_thumb;
    ComPtr<ID2D1SolidColorBrush> scroll_thumb_active;

    bool open(ID2D1RenderTarget* target) {
        struct Wanted {
            ComPtr<ID2D1SolidColorBrush>* into;
            UINT32 rgb;
            float alpha;
        };
        const Wanted wanted[] = {
            {&ink, kInk, 1.0f},
            {&muted, kMutedInk, 1.0f},
            {&header_ink, kHeaderInk, 1.0f},
            {&header, kHeaderBand, 1.0f},
            {&banding, kRule, kBandingAlpha},
            {&separator, kRule, kSeparatorAlpha},
            {&selected_row, kAccent, kSelectedRowAlpha},
            {&selected_cell, kAccent, kSelectedCellAlpha},
            {&cursor, kAccent, 1.0f},
            {&scroll_track, kRule, kScrollTrackAlpha},
            {&scroll_thumb, kRule, kScrollThumbAlpha},
            {&scroll_thumb_active, kRule, kScrollThumbActiveAlpha},
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
               const Selection* selection, float scroll_row, bool dragging_thumb) {
    const D2D1_SIZE_F view = target->GetSize();
    size_t rows = 0;
    for (const Column& column : columns) {
        rows = column.cells.size() > rows ? column.cells.size() : rows;
    }
    const RowSpan span = visible_rows(scroll_row, view.height, rows);

    // Every row's position comes from here, and it takes the row's own number
    // rather than its place on screen. Those are the same only at rest, and the
    // grid that confuses them looks right until it is scrolled: the banding
    // belongs to the row, so striping by screen position makes the stripes swap
    // places under a moving result.
    const auto row_y = [scroll_row](size_t r) {
        return kHeaderHeight + (static_cast<float>(r) - scroll_row) * kRowHeight;
    };

    target->Clear(D2D1::ColorF(kCanvas));

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
        if (selection->column >= 0 && static_cast<size_t>(selection->column) < columns.size()
            && selection->row >= 0 && static_cast<size_t>(selection->row) < rows) {
            const Column& column = columns[selection->column];
            const float y = row_y(static_cast<size_t>(selection->row));
            // Apart from the band because within a multi-row selection this is
            // the one cell the keyboard and the inspector act on, and it has to
            // stay distinguishable from the rows around it.
            target->FillRectangle(
                D2D1::RectF(column.x, y, column.x + column.width, y + kRowHeight),
                palette.selected_cell.Get());
            // A one-DIP edge on the leading side, at full strength. The fill
            // washes out over a dark value; this does not, so the cell stays
            // findable when it does.
            target->FillRectangle(D2D1::RectF(column.x, y, column.x + 1.0f, y + kRowHeight),
                                  palette.cursor.Get());
        }
    }

    // On the column's trailing edge, one DIP wide, and only between the columns
    // — the run starts below the header band and the last column has no rule
    // after it, because a line at the right of the last column would read as an
    // empty column beginning there.
    for (size_t c = 0; c + 1 < columns.size(); ++c) {
        const float x = columns[c].x + columns[c].width;
        target->FillRectangle(D2D1::RectF(x, kHeaderHeight, x + 1.0f, view.height),
                              palette.separator.Get());
    }

    target->FillRectangle(D2D1::RectF(0.0f, 0.0f, view.width, kHeaderHeight),
                          palette.header.Get());
    target->FillRectangle(D2D1::RectF(0.0f, kHeaderHeight - 1.0f, view.width, kHeaderHeight),
                          palette.separator.Get());

    // The text box is the cell inset by its padding on both sides, not the whole
    // column. Left-aligned that distinction never showed; right-aligned it is
    // the difference between a value on the padding and a value against the
    // separator.
    //
    // Headers are left-aligned whichever way their column is. A heading is a
    // name, and names read from the left even above a column of numbers.
    for (const Column& column : columns) {
        draw_text(target, dwrite, font, column.heading, column.x + kCellPadding, kHeaderNameY,
                  column.width - kCellPadding * 2.0f, palette.header_ink.Get(), false);
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
    target->PushAxisAlignedClip(D2D1::RectF(0.0f, kHeaderHeight, view.width, view.height),
                                D2D1_ANTIALIAS_MODE_ALIASED);
    for (const Column& column : columns) {
        const float x = column.x + kCellPadding;
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
    Scrollbar bar;
    if (scrollbar_of(scroll_row, view.height, rows, &bar)) {
        const float x = view.width - kScrollbarGutter;
        target->FillRectangle(
            D2D1::RectF(x, bar.track_start, view.width, bar.track_start + bar.track_length),
            palette.scroll_track.Get());
        // Centred in the gutter it is grabbed by, which is what makes the target
        // wider than the paint without making the paint look misplaced.
        const float inset = (kScrollbarGutter - kScrollbarThumb) / 2.0f;
        target->FillRectangle(
            D2D1::RectF(x + inset, bar.thumb_start, x + inset + kScrollbarThumb,
                        bar.thumb_start + bar.thumb_length),
            dragging_thumb ? palette.scroll_thumb_active.Get() : palette.scroll_thumb.Get());
    }
}

// A connection, one result, and nothing left open. `read_columns` copies every
// value it wants into `Column`, so the handle has no reason to outlive it, and a
// window that held a live DuckDB connection for as long as it held a frame is a
// shape worth not starting.
bool load_grid(std::vector<Column>* out) {
    char* err = nullptr;
    DbHandle* handle = db_connect("duckdb://:memory:", nullptr, 10, &err);
    if (handle == nullptr) {
        return core_failed("db_connect", err);
    }
    const bool read = read_columns(handle, out);
    db_free(handle);
    return read;
}

bool the_grid_draws_a_result() {
    std::vector<Column> columns;
    if (!load_grid(&columns)) {
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
    check(columns[0].width >= kMinColumnWidth, "a narrow column is held to the minimum");

    check(columns[0].numeric, "the id column is read as a number");
    check(!columns[1].numeric, "the name column is not");

    Palette palette;
    if (!palette.open(surface.target.Get())) {
        return false;
    }

    surface.target->BeginDraw();
    draw_grid(surface.target.Get(), surface.dwrite.Get(), font, columns, palette, nullptr, 0.0f,
              false);
    HRESULT hr = surface.target->EndDraw();
    if (FAILED(hr)) {
        return failed("ID2D1RenderTarget::EndDraw", hr);
    }

    const float right = columns.back().x + columns.back().width;

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
    check(band[2] == ((kHeaderBand >> 16) & 0xFF) && band[1] == ((kHeaderBand >> 8) & 0xFF)
              && band[0] == (kHeaderBand & 0xFF),
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
    const RowSpan visible = visible_rows(0.0f, static_cast<float>(kHeight), rows);
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
    check(cell_at(columns[2].x + 4.0f, kHeaderHeight + kRowHeight + 4.0f, 0.0f, columns, rows, &under)
              && under.row == 1 && under.column == 2,
          "a point inside a cell finds that cell");
    check(!cell_at(columns[2].x + 4.0f, 4.0f, 0.0f, columns, rows, &under),
          "a point on the header finds none");
    check(!cell_at(right + 4.0f, kHeaderHeight + 4.0f, 0.0f, columns, rows, &under),
          "a point past the last column finds none");
    check(!cell_at(4.0f, kHeaderHeight + static_cast<float>(rows) * kRowHeight + 4.0f, 0.0f,
                   columns, rows, &under),
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
    draw_grid(surface.target.Get(), surface.dwrite.Get(), font, columns, palette, &selection, 0.0f,
              false);
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
    // boundary: this one is at 133.977, and the pixel `columns[2].x` names holds
    // two percent of the line and ninety-eight percent of the column before it.
    // The middle of the line is the only point that is inside it whatever the
    // fraction turns out to be.
    BYTE edge[3] = {};
    if (!surface.pixel_at(columns[2].x + 0.5f, selected_y, edge)) {
        return failed("reading the cursor edge", E_FAIL);
    }
    // Measured there, the pixel is 74/67/214 — `kAccent` at 79/70/229 with the
    // eight percent of the separator that is drawn over it, the order
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
    // Scrolling: which rows are on screen, and which row each one is
    // ------------------------------------------------------------------

    const auto view_height = static_cast<float>(kHeight);
    check(visible_rows(0.0f, view_height, rows).first == 0
              && visible_rows(7.0f, view_height, rows).first == 7,
          "the top row is the one the scroll is counted in");
    // One past the bottom edge, not one short of it. 640 minus the header is
    // 608, which is 30.4 rows: a grid that drew 30 would leave the last two
    // fifths of a row empty at every position that is not a whole number.
    check(visible_rows(0.0f, view_height, rows).last == 32,
          "and the span reaches past the bottom edge rather than short of it");
    check(visible_rows(38.0f, view_height, rows).last == rows,
          "the span stops at the last row rather than past it");

    // 40 rows less the 30.4 that fit. Scrolling further would put blank canvas
    // under the last row, which is the thing that makes a grid feel like it has
    // lost the result.
    const float most = max_scroll_row(view_height, rows);
    check(most > 9.5f && most < 9.7f, "the scroll stops where the last row reaches the bottom");

    // The keyboard's half of scrolling, and the reason it is a separate
    // quantity: a row is visible only if all of it is, so the fold is at 30
    // whole rows rather than at 30.4.
    check(scroll_to_visible(0.0f, 5, view_height, rows) == 0.0f,
          "a row already on screen does not move the view");
    check(scroll_to_visible(0.0f, 39, view_height, rows) == most,
          "the last row brings the view to the end");
    check(scroll_to_visible(20.0f, 3, view_height, rows) == 3.0f,
          "a row above the fold becomes the top row");

    // And the hit test counts in the same rows the drawing does. A grid that
    // scrolled its pixels and not its arithmetic answers every click with the
    // row that used to be there, and the selection lands somewhere the user can
    // see they did not point at.
    check(cell_at(4.0f, kHeaderHeight + 4.0f, 7.0f, columns, rows, &under) && under.row == 7,
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
                  static_cast<float>(step), false);
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
    draw_grid(surface.target.Get(), surface.dwrite.Get(), font, columns, palette, nullptr, 0.5f,
              false);
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
    check(scrollbar_of(0.0f, view_height, rows, &bar), "a result taller than the view gets a bar");
    // Four rows in a view that holds thirty. Nothing is below the fold, and a
    // bar pinned to its full length would be a control that cannot be used
    // saying there is somewhere to go.
    Scrollbar unneeded;
    check(!scrollbar_of(0.0f, view_height, 4, &unneeded), "and a result that fits gets none");

    // 608 of track for 30.4 rows out of 40, which is 462. The thumb is how much
    // of the result is on screen, and reading it is how anyone knows whether
    // they are looking at most of a table or at the first screen of a million.
    check(bar.track_start == kHeaderHeight && bar.track_length == view_height - kHeaderHeight,
          "the track runs from under the header to the foot of the view");
    check(bar.thumb_length > 461.0f && bar.thumb_length < 463.0f,
          "the thumb is as long a part of the track as the view is of the result");
    // A million rows would ask for a thumb a thousandth of the track, which is
    // half a DIP: too small to see and far too small to hit.
    Scrollbar huge;
    check(scrollbar_of(0.0f, view_height, 1000000, &huge) && huge.thumb_length == kMinThumbLength,
          "and never shorter than something that can be grabbed");

    check(bar.thumb_start == bar.track_start, "at rest the thumb is at the top of the track");
    Scrollbar ended;
    check(scrollbar_of(most, view_height, rows, &ended)
              && ended.thumb_start + ended.thumb_length == ended.track_start + ended.track_length,
          "at the end of the scroll its trailing edge is on the track's");

    // The drag is the inverse of the line above, so the two have to agree: a
    // thumb picked up and put down without moving must leave the scroll where
    // it was. They are separate expressions, and it is the round trip that says
    // the second one is the first one backwards.
    Scrollbar midway;
    check(scrollbar_of(4.0f, view_height, rows, &midway)
              && scroll_to_thumb(midway.thumb_start, view_height, rows) > 3.99f
              && scroll_to_thumb(midway.thumb_start, view_height, rows) < 4.01f,
          "dragging the thumb back to where it was leaves the scroll there");
    check(scroll_to_thumb(-100.0f, view_height, rows) == 0.0f
              && scroll_to_thumb(10000.0f, view_height, rows) == most,
          "and a drag past either end of the track stops at the end of the result");

    // Drawn at both ends, and read where the thumb is not: the track is four
    // percent and the thumb eighteen, so the question is which of the two is at
    // the top of the gutter.
    const float gutter_x = static_cast<float>(kWidth) - kScrollbarGutter / 2.0f;
    BYTE thumb_top[3] = {};
    BYTE track_top[3] = {};
    surface.target->BeginDraw();
    draw_grid(surface.target.Get(), surface.dwrite.Get(), font, columns, palette, nullptr, 0.0f,
              false);
    hr = surface.target->EndDraw();
    if (FAILED(hr) || !surface.pixel_at(gutter_x, kHeaderHeight + 10.0f, thumb_top)) {
        return failed("reading the thumb at rest", FAILED(hr) ? hr : E_FAIL);
    }
    surface.target->BeginDraw();
    draw_grid(surface.target.Get(), surface.dwrite.Get(), font, columns, palette, nullptr, most,
              false);
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
    draw_grid(surface.target.Get(), surface.dwrite.Get(), font, columns, palette, nullptr, 0.0f,
              true);
    hr = surface.target->EndDraw();
    if (FAILED(hr) || !surface.pixel_at(gutter_x, kHeaderHeight + 10.0f, held)) {
        return failed("reading a thumb being dragged", FAILED(hr) ? hr : E_FAIL);
    }
    check(held[2] < thumb_top[2], "and darkens while it is being dragged");

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
    float scroll_row = 0.0f;
    // Where on the thumb the drag took hold, so the thumb stays under the
    // pointer instead of jumping its own leading edge there on the first move.
    bool dragging = false;
    float grab_offset = 0.0f;

    size_t rows() const { return columns.empty() ? 0 : columns[0].cells.size(); }

    // The view in DIPs, which is what every scroll calculation is in.
    float height() const { return target ? target->GetSize().height : 0.0f; }
    float width() const { return target ? target->GetSize().width : 0.0f; }

    // Moves the scroll so the thumb sits where the pointer has taken it. The
    // grab offset is what keeps the point of the thumb that was grabbed under
    // the pointer for the whole drag rather than only at the moment of the
    // press.
    void drag_to(float y) {
        scroll_row = scroll_to_thumb(y - grab_offset, height(), rows());
        InvalidateRect(hwnd, nullptr, FALSE);
    }

    void scroll_by(float rows_by) {
        const float most = max_scroll_row(height(), rows());
        scroll_row += rows_by;
        if (scroll_row > most) {
            scroll_row = most;
        }
        if (scroll_row < 0.0f) {
            scroll_row = 0.0f;
        }
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

    // Moved and clamped rather than wrapped: an arrow at the edge of a result
    // does nothing, which is what every grid does and what stops a keystroke
    // from teleporting the cursor to the far corner.
    void move(int rows_by, int columns_by, bool extend) {
        const int last_row = static_cast<int>(rows()) - 1;
        const int last_column = static_cast<int>(columns.size()) - 1;
        if (last_row < 0 || last_column < 0) {
            return;
        }
        // An arrow key with nothing selected acts from the first cell rather
        // than doing nothing, which is what `AppController.swift` does when the
        // renderer's selection is nil. Ignoring it instead would leave a click
        // as the only way into the grid, and a result opened from the keyboard
        // would have four keys that appeared not to work.
        if (!selected) {
            selection = Selection{};
            selected = true;
        }
        // Taken before the move, so a shift-arrow from an unextended selection
        // grows from where the cursor was rather than from where it lands.
        if (extend && !selection.anchored) {
            selection.anchored = true;
            selection.anchor = selection.row;
        } else if (!extend) {
            selection.anchored = false;
        }
        const int row = selection.row + rows_by;
        const int column = selection.column + columns_by;
        selection.row = row < 0 ? 0 : (row > last_row ? last_row : row);
        selection.column = column < 0 ? 0 : (column > last_column ? last_column : column);
        // The cursor takes the view with it. A grid that let the cursor leave
        // the screen would answer every further arrow key by moving something
        // the user cannot see, and the only way back would be to guess how far.
        scroll_row = scroll_to_visible(scroll_row, selection.row, height(), rows());
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

        return palette.open(target.Get());
    }

    void paint() {
        if (!ready()) {
            return;
        }
        target->BeginDraw();
        draw_grid(target.Get(), dwrite.Get(), font, columns, palette,
                  selected ? &selection : nullptr, scroll_row, dragging);
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

        // Before the cell, and returning: a press in the gutter is for the bar,
        // and must not also land on whatever row is underneath it.
        Scrollbar bar;
        if (x >= window->width() - kScrollbarGutter && y >= kHeaderHeight
            && scrollbar_of(window->scroll_row, window->height(), window->rows(), &bar)) {
            const bool on_thumb = y >= bar.thumb_start && y <= bar.thumb_start + bar.thumb_length;
            // A press on the track goes where it points rather than paging
            // towards it. On four hundred thousand rows, paging there is an
            // afternoon's work with the mouse button held down.
            window->grab_offset = on_thumb ? y - bar.thumb_start : bar.thumb_length / 2.0f;
            window->dragging = true;
            // Captured, so a drag that wanders off the side of the window keeps
            // scrolling instead of stopping at the edge and letting go silently.
            SetCapture(hwnd);
            window->drag_to(y);
            return 0;
        }

        Selection hit;
        if (cell_at(x, y, window->scroll_row, window->columns, window->rows(), &hit)) {
            window->selection = hit;
            window->selected = true;
            InvalidateRect(hwnd, nullptr, FALSE);
        }
        return 0;
    }

    // Three rows a notch, `AppController.swift`'s multiplier. One row would be
    // an accurate wheel and a useless one: a result is read in pages, and a
    // notch that moved a single line would need forty of them to cross a screen.
    //
    // The distance is kept as a fraction rather than rounded to rows, so that a
    // trackpad — which sends many small deltas rather than a few whole notches —
    // scrolls smoothly instead of standing still until the deltas add up to one.
    case WM_MOUSEMOVE:
        if (window->dragging) {
            window->drag_to(static_cast<float>(GET_Y_LPARAM(lparam)) * window->dips());
        }
        return 0;

    // The capture is released whichever way the button comes up, including the
    // one where another window takes it away — a drag left engaged would leave
    // the thumb dark and the grid following a pointer nobody is pressing.
    case WM_LBUTTONUP:
    case WM_CAPTURECHANGED:
        if (window->dragging) {
            window->dragging = false;
            if (message == WM_LBUTTONUP) {
                ReleaseCapture();
            }
            InvalidateRect(hwnd, nullptr, FALSE);
        }
        return 0;

    case WM_MOUSEWHEEL:
        window->scroll_by(-static_cast<float>(GET_WHEEL_DELTA_WPARAM(wparam))
                          / static_cast<float>(WHEEL_DELTA) * 3.0f);
        return 0;

    // Claimed, so the arrows arrive here rather than being taken for dialog
    // navigation. There is nothing to tab between yet, and the grid is the only
    // thing in the window that an arrow key could mean anything to.
    case WM_GETDLGCODE:
        return DLGC_WANTARROWS;

    case WM_KEYDOWN: {
        const bool extend = (GetKeyState(VK_SHIFT) & 0x8000) != 0;
        switch (wparam) {
        case VK_UP:
            window->move(-1, 0, extend);
            return 0;
        case VK_DOWN:
            window->move(1, 0, extend);
            return 0;
        // Sideways never extends: the band is a range of rows, and shift-left
        // has no range to grow. Passing `extend` here would collapse one that
        // was already open, which is the opposite of what the key says.
        case VK_LEFT:
            window->move(0, -1, false);
            return 0;
        case VK_RIGHT:
            window->move(0, 1, false);
            return 0;
        default:
            return 0;
        }
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
    if (!load_grid(&window.columns)) {
        return 1;
    }
    lay_out(&window.columns, window.font.advance);

    WNDCLASSEXW window_class{};
    window_class.cbSize = sizeof(window_class);
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

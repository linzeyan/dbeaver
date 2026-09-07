// The Windows front end, from the bottom.
//
// There is no window here yet, and that is the point of starting here. The plan
// is WinUI 3 with a Direct2D grid, and of those two the XAML shell is the part
// that is well understood and the part whose problems are somebody else's build
// system. The grid is Direct2D and DirectWrite, it is where every hard question
// lives — text metrics, DPI, how much can be drawn in a frame — and none of it
// needs an HWND to be wrong. So this asks the rendering stack first, headless,
// and the window comes next.
//
// The checks run inside the real binary behind a flag, the same arrangement the
// macOS app uses, rather than in a test target that would have to reproduce this
// link. They draw into a WIC bitmap instead of a swap chain, so they run on a
// machine with no display and no GPU, which is what a CI runner is.
//
// Built with `cl` directly, like `apps/windows/ffi-check` next door. No package
// manager and no project file, because everything here ships with the Windows
// SDK, and a first brick that needs a NuGet restore to say whether Direct2D
// works is a brick that answers two questions and tells you neither.

#define WIN32_LEAN_AND_MEAN
#include <windows.h>

#include <d2d1.h>
#include <dwrite.h>
#include <wincodec.h>
#include <wrl/client.h>

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

    // Pixels differing from the colour the target was cleared to, inside one
    // rectangle.
    //
    // Every call in a draw can succeed and leave a blank bitmap — a brush the
    // same colour as the background, a layout positioned off the edge, a font
    // that resolved to nothing — and `EndDraw` reports none of it. Asked of a
    // rectangle rather than of the whole surface because "something was drawn"
    // and "something was drawn *there*" are different questions, and only the
    // second one notices a grid that painted every row on top of the first.
    bool ink_in(float x0, float y0, float x1, float y1, UINT* out) const {
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

        UINT painted = 0;
        for (UINT y = from_y; y < to_y; ++y) {
            const BYTE* row = pixels + static_cast<size_t>(y) * stride;
            for (UINT x = from_x; x < to_x; ++x) {
                // BGRA, premultiplied. The target is cleared to opaque white, so
                // a pixel with any channel below full is one the text reached.
                const BYTE* px = row + static_cast<size_t>(x) * 4;
                if (px[0] != 0xFF || px[1] != 0xFF || px[2] != 0xFF) {
                    painted += 1;
                }
            }
        }
        *out = painted;
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
        // Asked of the face rather than guessed from the size. A row is 20 and
        // the text has to sit inside it; deriving the inset from a made-up ratio
        // is how text ends up a pixel high in one font and clipped in the next.
        line_height = static_cast<float>(face_metrics.ascent + face_metrics.descent
                                         + face_metrics.lineGap)
                      * kFontSize / per_em;

        hr = dwrite->CreateTextFormat(kFamily, nullptr, DWRITE_FONT_WEIGHT_NORMAL,
                                      DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_STRETCH_NORMAL,
                                      kFontSize, L"en-us", &format);
        if (FAILED(hr)) {
            return failed("CreateTextFormat", hr);
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
    float x = 0.0f;
    float width = 0.0f;
};

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
                              "            ELSE 'read ' || (i * 100) END AS note "
                              "FROM range(3) t(i) ORDER BY i",
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
        Column column;
        column.heading = widen(field.name);
        for (int64_t r = 0; r < batch.length; ++r) {
            const bool present = valid_at(values, r);
            column.nulls.push_back(!present);
            const std::string format(field.format);
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

bool draw_text(const Surface& surface, const Monospace& font, const std::wstring& text, float x,
               float y, float width, const ComPtr<ID2D1SolidColorBrush>& brush) {
    if (text.empty()) {
        return true;
    }
    ComPtr<IDWriteTextLayout> layout;
    // One layout per cell, for now. The macOS grid draws from a glyph atlas
    // instead, because a grid draws the same ninety-five shapes tens of
    // thousands of times a frame — but that is a decision made against a
    // measurement, and there is no frame to measure here yet.
    const HRESULT hr = font.format
                           ? surface.dwrite->CreateTextLayout(text.c_str(),
                                                              static_cast<UINT32>(text.size()),
                                                              font.format.Get(), width,
                                                              kRowHeight, &layout)
                           : E_FAIL;
    if (FAILED(hr)) {
        return failed("CreateTextLayout", hr);
    }
    surface.target->DrawTextLayout(D2D1::Point2F(x, y), layout.Get(), brush.Get());
    return true;
}

bool the_grid_draws_a_result() {
    char* err = nullptr;
    DbHandle* handle = db_connect("duckdb://:memory:", nullptr, 10, &err);
    if (handle == nullptr) {
        return core_failed("db_connect", err);
    }
    std::vector<Column> columns;
    const bool read = read_columns(handle, &columns);
    db_free(handle);
    if (!read) {
        return false;
    }

    check(columns.size() == 3, "the result has three columns");
    if (columns.size() != 3) {
        return false;
    }
    check(columns[2].nulls.size() == 3 && columns[2].nulls[1],
          "the middle row of the third column is null");
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

    ComPtr<ID2D1SolidColorBrush> ink;
    HRESULT hr = surface.target->CreateSolidColorBrush(D2D1::ColorF(D2D1::ColorF::Black), &ink);
    if (FAILED(hr)) {
        return failed("CreateSolidColorBrush", hr);
    }
    // Dimmer, the way `Theme.Grid.nullText` is: NULL is a fact about the value,
    // not one of its characters, and it should not read as data.
    ComPtr<ID2D1SolidColorBrush> faint;
    hr = surface.target->CreateSolidColorBrush(D2D1::ColorF(0.55f, 0.55f, 0.55f), &faint);
    if (FAILED(hr)) {
        return failed("CreateSolidColorBrush", hr);
    }

    surface.target->BeginDraw();
    surface.target->Clear(D2D1::ColorF(D2D1::ColorF::White));
    const float text_inset = (kRowHeight - font.line_height) / 2.0f;
    for (const Column& column : columns) {
        draw_text(surface, font, column.heading, column.x + kCellPadding,
                  kHeaderHeight - kRowHeight + text_inset, column.width, ink);
        for (size_t r = 0; r < column.cells.size(); ++r) {
            const float y = kHeaderHeight + static_cast<float>(r) * kRowHeight + text_inset;
            draw_text(surface, font, column.cells[r], column.x + kCellPadding, y, column.width,
                      column.nulls[r] ? faint : ink);
        }
    }
    hr = surface.target->EndDraw();
    if (FAILED(hr)) {
        return failed("ID2D1RenderTarget::EndDraw", hr);
    }

    const float right = columns.back().x + columns.back().width;
    UINT header_ink = 0;
    if (!surface.ink_in(0.0f, 0.0f, right, kHeaderHeight, &header_ink)) {
        return failed("reading the header band", E_FAIL);
    }
    check(header_ink > 0, "the headings reached the bitmap");

    // Per row, not for the block: three rows drawn on top of each other put ink
    // in the block and leave two of these at zero, which is the mistake a first
    // grid actually makes.
    bool every_row = true;
    for (size_t r = 0; r < 3; ++r) {
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

    UINT total = 0;
    surface.ink_in(0.0f, 0.0f, static_cast<float>(kWidth), static_cast<float>(kHeight), &total);
    std::printf("      %u pixels painted, %.2f advance, columns %.0f %.0f %.0f\n", total,
                font.advance, columns[0].width, columns[1].width, columns[2].width);
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

}  // namespace

int main(int argc, char** argv) {
    const std::string flag = argc > 1 ? argv[1] : "";
    if (flag != "--verify-drivers" && flag != "--verify-grid") {
        std::printf("nothing to run yet: pass --verify-drivers or --verify-grid\n");
        return 2;
    }

    // Apartment-threaded because WIC is instantiated here and the eventual
    // window will need one anyway. Every call the core makes blocks, and none of
    // them are made on this thread.
    const HRESULT hr = CoInitializeEx(nullptr, COINIT_APARTMENTTHREADED);
    if (FAILED(hr)) {
        std::printf("FAIL  CoInitializeEx: hr=0x%08lx\n", static_cast<unsigned long>(hr));
        return 1;
    }

    if (flag == "--verify-drivers") {
        the_driver_list_draws();
    } else {
        the_grid_draws_a_result();
    }

    CoUninitialize();

    if (failures != 0) {
        std::printf("\n%d check(s) failed\n", failures);
        return 1;
    }
    std::printf("\nevery check passed\n");
    return 0;
}

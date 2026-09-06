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
// `--verify-drivers` is the same arrangement the macOS app uses: checks that run
// inside the real binary behind a flag, rather than in a test target that would
// have to reproduce this link. It draws into a WIC bitmap instead of a swap
// chain, so it runs on a machine with no display at all, which is what a CI
// runner is.
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

// The bitmap the check draws into. Big enough to hold every driver on one line
// each at 16pt, with room left over — a layout that overflowed would otherwise
// be indistinguishable from one that fitted exactly.
constexpr UINT kWidth = 480;
constexpr UINT kHeight = 640;

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

// The `label` of every driver, scanned out rather than parsed.
//
// A JSON parser would be a second thing to be wrong in a check about drawing,
// and the shape here is fixed by `db_drivers_json` rather than by a user: the
// labels are the display names the eventual list shows, so drawing them is
// drawing the real thing rather than lorem ipsum with the right length.
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

// Whether anything was actually painted, asked of the pixels rather than of the
// return codes.
//
// Every call in the draw can succeed and leave a blank bitmap — a brush the same
// colour as the background, a layout positioned off the edge, a font that
// resolved to nothing. `EndDraw` reports none of that. Counting pixels that
// differ from the colour the target was cleared to is the one question whose
// answer cannot be produced by the drawing code being wrong in the usual ways.
bool anything_was_drawn(const ComPtr<IWICBitmap>& bitmap, UINT* ink) {
    WICRect all{0, 0, static_cast<INT>(kWidth), static_cast<INT>(kHeight)};
    ComPtr<IWICBitmapLock> locked;
    if (FAILED(bitmap->Lock(&all, WICBitmapLockRead, &locked))) {
        return false;
    }
    UINT size = 0;
    BYTE* pixels = nullptr;
    UINT stride = 0;
    if (FAILED(locked->GetStride(&stride)) || FAILED(locked->GetDataPointer(&size, &pixels))) {
        return false;
    }

    UINT painted = 0;
    for (UINT y = 0; y < kHeight; ++y) {
        const BYTE* row = pixels + static_cast<size_t>(y) * stride;
        for (UINT x = 0; x < kWidth; ++x) {
            // BGRA, premultiplied. The target is cleared to opaque white, so a
            // pixel with any channel below full is one the text reached.
            const BYTE* px = row + static_cast<size_t>(x) * 4;
            if (px[0] != 0xFF || px[1] != 0xFF || px[2] != 0xFF) {
                painted += 1;
            }
        }
    }
    *ink = painted;
    return true;
}

bool the_driver_list_draws() {
    char* err = nullptr;
    char* json = db_drivers_json(&err);
    if (json == nullptr) {
        std::printf("FAIL  db_drivers_json: %s\n", err ? err : "(no message)");
        if (err != nullptr) {
            db_string_free(err);
        }
        failures += 1;
        return false;
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

    ComPtr<ID2D1Factory> d2d;
    HRESULT hr = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, __uuidof(ID2D1Factory),
                                   reinterpret_cast<void**>(d2d.GetAddressOf()));
    if (FAILED(hr)) {
        return failed("D2D1CreateFactory", hr);
    }

    ComPtr<IDWriteFactory> dwrite;
    hr = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED, __uuidof(IDWriteFactory),
                             reinterpret_cast<IUnknown**>(dwrite.GetAddressOf()));
    if (FAILED(hr)) {
        return failed("DWriteCreateFactory", hr);
    }

    ComPtr<IWICImagingFactory> wic;
    hr = CoCreateInstance(CLSID_WICImagingFactory, nullptr, CLSCTX_INPROC_SERVER,
                          IID_PPV_ARGS(&wic));
    if (FAILED(hr)) {
        return failed("CLSID_WICImagingFactory", hr);
    }

    ComPtr<IWICBitmap> bitmap;
    hr = wic->CreateBitmap(kWidth, kHeight, GUID_WICPixelFormat32bppPBGRA,
                           WICBitmapCacheOnLoad, &bitmap);
    if (FAILED(hr)) {
        return failed("IWICImagingFactory::CreateBitmap", hr);
    }

    // Software rather than whatever the machine has: a runner has no GPU, and a
    // check that quietly needed one would fail here for a reason that has
    // nothing to do with the code being checked.
    const D2D1_RENDER_TARGET_PROPERTIES properties = D2D1::RenderTargetProperties(
        D2D1_RENDER_TARGET_TYPE_SOFTWARE,
        D2D1::PixelFormat(DXGI_FORMAT_B8G8R8A8_UNORM, D2D1_ALPHA_MODE_PREMULTIPLIED));
    ComPtr<ID2D1RenderTarget> target;
    hr = d2d->CreateWicBitmapRenderTarget(bitmap.Get(), properties, &target);
    if (FAILED(hr)) {
        return failed("CreateWicBitmapRenderTarget", hr);
    }

    // Segoe UI is the system face every supported Windows has, which is what the
    // shell should use anyway; the grid picking its own font is a later
    // decision, and not one to make by accident here.
    ComPtr<IDWriteTextFormat> format;
    hr = dwrite->CreateTextFormat(L"Segoe UI", nullptr, DWRITE_FONT_WEIGHT_NORMAL,
                                  DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_STRETCH_NORMAL, 16.0f,
                                  L"en-us", &format);
    if (FAILED(hr)) {
        return failed("CreateTextFormat", hr);
    }

    // A layout rather than DrawText, for two reasons. It is what a grid keeps
    // per cell, so this measures the thing that will actually exist; and
    // `DrawText` is a macro in windows.h, so `target->DrawText` would be
    // rewritten to a method that does not exist.
    ComPtr<IDWriteTextLayout> layout;
    hr = dwrite->CreateTextLayout(text.c_str(), static_cast<UINT32>(text.size()), format.Get(),
                                  static_cast<FLOAT>(kWidth), static_cast<FLOAT>(kHeight),
                                  &layout);
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
    hr = target->CreateSolidColorBrush(D2D1::ColorF(D2D1::ColorF::Black), &brush);
    if (FAILED(hr)) {
        return failed("CreateSolidColorBrush", hr);
    }

    target->BeginDraw();
    target->Clear(D2D1::ColorF(D2D1::ColorF::White));
    target->DrawTextLayout(D2D1::Point2F(0.0f, 0.0f), layout.Get(), brush.Get());
    hr = target->EndDraw();
    if (FAILED(hr)) {
        return failed("ID2D1RenderTarget::EndDraw", hr);
    }

    UINT ink = 0;
    if (!anything_was_drawn(bitmap, &ink)) {
        return failed("reading the bitmap back", E_FAIL);
    }
    std::printf("      %u pixels painted, %u lines, %.1f x %.1f\n", ink, metrics.lineCount,
                metrics.width, metrics.height);
    check(ink > 0, "the text reached the bitmap");

    return failures == 0;
}

}  // namespace

int main(int argc, char** argv) {
    const bool verifying = argc > 1 && std::string(argv[1]) == "--verify-drivers";
    if (!verifying) {
        std::printf("nothing to run yet: pass --verify-drivers\n");
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

    the_driver_list_draws();

    CoUninitialize();

    if (failures != 0) {
        std::printf("\n%d check(s) failed\n", failures);
        return 1;
    }
    std::printf("\nevery check passed\n");
    return 0;
}

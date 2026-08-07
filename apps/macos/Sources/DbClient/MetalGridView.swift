import MetalKit
import SwiftUI

/// The cell the grid's cursor is on.
struct GridSelection: Equatable {
    var row: Int
    var column: Int
}

/// Bridges the Metal grid into SwiftUI.
///
/// The grid stays AppKit + Metal rather than becoming a SwiftUI `Table`: no
/// view-based table survives a million rows with dynamic columns, and phase 0
/// exists to demonstrate exactly that. SwiftUI owns the chrome; this owns the
/// data surface.
struct MetalGridView: NSViewRepresentable {
    let table: ArrowTable
    /// Changes when the underlying result is replaced, which is the signal to
    /// reset scroll position and redraw.
    let generation: Int
    @Binding var selection: GridSelection?
    /// See `GridView.claimsInitialFocus`.
    var claimsInitialFocus = false

    final class Coordinator {
        var renderer: GridRenderer?
        var lastGeneration = -1
        var onSelect: ((GridSelection) -> Void)?
    }

    func makeCoordinator() -> Coordinator { Coordinator() }

    func makeNSView(context: Context) -> GridView {
        guard let device = MTLCreateSystemDefaultDevice() else {
            return GridView(frame: .zero, device: nil)
        }
        // The window is not attached yet, so take the scale from the screen.
        // updateNSView corrects it once the view is in a window.
        let scale = NSScreen.main?.backingScaleFactor ?? 2
        let view = GridView(frame: .zero, device: device)
        view.colorPixelFormat = .bgra8Unorm
        view.clearColor = Theme.Grid.background.mtlClear
        view.isPaused = true
        view.enableSetNeedsDisplay = true
        view.claimsInitialFocus = claimsInitialFocus
        view.onSelect = { [weak coordinator = context.coordinator] hit in
            coordinator?.onSelect?(hit)
        }

        if let renderer = GridRenderer(device: device, scale: scale) {
            renderer.table = table
            view.renderer = renderer
            view.delegate = context.coordinator.makeDelegate(renderer: renderer)
            context.coordinator.renderer = renderer
        }
        return view
    }

    func updateNSView(_ view: GridView, context: Context) {
        guard let renderer = context.coordinator.renderer else { return }
        renderer.table = table
        // Re-captured each update so the closure writes through the current
        // binding rather than the one that existed when the view was made.
        context.coordinator.onSelect = { selection = $0 }

        if context.coordinator.lastGeneration != generation {
            context.coordinator.lastGeneration = generation
            // A new result starts at the top; keeping the old offset would show
            // an arbitrary window of unrelated data.
            renderer.scrollRow = 0
            renderer.scrollX = 0
            // The stored widths describe the previous result's columns.
            renderer.invalidateColumnLayout()
        }
        // The model owns the selection, including the one it sets when a result
        // arrives, so the renderer follows the binding rather than being reset
        // here — resetting would discard that initial selection every time.
        if renderer.selection != selection { renderer.selection = selection }
        view.needsDisplay = true
    }
}

extension MetalGridView.Coordinator {
    /// Retains the delegate, which MTKView holds weakly.
    func makeDelegate(renderer: GridRenderer) -> MTKViewDelegate {
        let d = GridDrawDelegate(renderer: renderer)
        retainedDelegate = d
        return d
    }
}

private var retainedDelegateKey: UInt8 = 0
extension MetalGridView.Coordinator {
    var retainedDelegate: MTKViewDelegate? {
        get { objc_getAssociatedObject(self, &retainedDelegateKey) as? MTKViewDelegate }
        set {
            objc_setAssociatedObject(
                self, &retainedDelegateKey, newValue, .OBJC_ASSOCIATION_RETAIN)
        }
    }
}

/// Minimal delegate: the benchmark harness has its own, which also drives the
/// scripted scroll and frame statistics.
final class GridDrawDelegate: NSObject, MTKViewDelegate {
    private let renderer: GridRenderer

    init(renderer: GridRenderer) {
        self.renderer = renderer
    }

    func mtkView(_ view: MTKView, drawableSizeWillChange size: CGSize) {}

    func draw(in view: MTKView) {
        renderer.draw(in: view)
    }
}

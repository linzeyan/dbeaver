#!/usr/bin/env swift
//
// Generates apps/macos/Resources/AppIcon.icns.
//
// Drawn rather than exported from a design tool: the icon is then a source file
// that reviews as a diff, and every size is rendered from the same vectors
// instead of downsampled from one bitmap — which is what makes the 16pt version
// legible.
//
// Usage: swift tools/make-icon.swift [output.icns]

import AppKit
import CoreGraphics
import SwiftUI

// MARK: - Geometry
//
// Everything is expressed against a 1024 canvas and scaled per output size.
// Apple's macOS grid puts the icon's rounded square at 824 of 1024, leaving the
// margin the system expects for shadow and optical alignment against the other
// icons in the Dock.

let canvas: CGFloat = 1024
let content = CGRect(x: 100, y: 100, width: 824, height: 824)
let cornerRadius: CGFloat = 824 * 0.2237

func color(_ hex: UInt32, _ alpha: CGFloat = 1) -> CGColor {
    CGColor(
        srgbRed: CGFloat((hex >> 16) & 0xFF) / 255,
        green: CGFloat((hex >> 8) & 0xFF) / 255,
        blue: CGFloat(hex & 0xFF) / 255,
        alpha: alpha)
}

/// Apple's continuous corner, taken from SwiftUI rather than approximated. A
/// circular-arc rounded rect is visibly not the platform shape at icon sizes.
func squircle(_ rect: CGRect, radius: CGFloat) -> CGPath {
    Path(
        roundedRect: rect,
        cornerSize: CGSize(width: radius, height: radius),
        style: .continuous
    ).cgPath
}

func gradient(_ stops: [(UInt32, CGFloat, CGFloat)]) -> CGGradient? {
    CGGradient(
        colorsSpace: CGColorSpaceCreateDeviceRGB(),
        colors: stops.map { color($0.0, $0.1) } as CFArray,
        locations: stops.map { $0.2 })
}

// MARK: - Drawing

func drawIcon(in ctx: CGContext, size: CGFloat) {
    ctx.scaleBy(x: size / canvas, y: size / canvas)
    // Core Graphics is y-up here, so "top" is high y throughout.
    let top = content.maxY
    let bottom = content.minY
    let shape = squircle(content, radius: cornerRadius)

    drawPlate(ctx, shape: shape, top: top, bottom: bottom)
    ctx.saveGState()
    ctx.addPath(shape)
    ctx.clip()
    drawGlass(ctx, top: top, bottom: bottom)
    drawDatabase(ctx)
    ctx.restoreGState()
    drawEdges(ctx, shape: shape, top: top, bottom: bottom)
}

/// The dark plate the whole icon sits on, plus the shadow that seats it.
private func drawPlate(_ ctx: CGContext, shape: CGPath, top: CGFloat, bottom: CGFloat) {
    ctx.saveGState()
    ctx.setShadow(offset: CGSize(width: 0, height: -16), blur: 40, color: color(0x0000_00, 0.5))
    ctx.addPath(shape)
    ctx.setFillColor(color(0x1C1E_22))
    ctx.fillPath()
    ctx.restoreGState()

    // Neutral graphite rather than a blue slate. The plate is the backdrop; any
    // hue in it competes with the accent glow behind the mark.
    guard let g = gradient([(0x4245_4B, 1, 0), (0x1E20_25, 1, 0.5), (0x0A0B_0D, 1, 1)]) else {
        return
    }
    ctx.saveGState()
    ctx.addPath(shape)
    ctx.clip()
    ctx.drawLinearGradient(
        g, start: CGPoint(x: 0, y: top), end: CGPoint(x: 0, y: bottom), options: [])
    ctx.restoreGState()
}

/// What makes it read as glass: a bright pool near the top-left where a light
/// source would sit, a broad sheen falling off down the plate, and a faint
/// bounce along the bottom edge. Kept low-contrast on purpose — at 16pt these
/// layers must not compete with the glyph.
private func drawGlass(_ ctx: CGContext, top: CGFloat, bottom: CGFloat) {
    if let sheen = gradient([(0xFFFF_FF, 0.17, 0), (0xFFFF_FF, 0.05, 0.35), (0xFFFF_FF, 0, 1)]) {
        ctx.drawLinearGradient(
            sheen, start: CGPoint(x: 0, y: top),
            end: CGPoint(x: 0, y: bottom + content.height * 0.35), options: [])
    }

    if let pool = gradient([(0xFFFF_FF, 0.20, 0), (0xFFFF_FF, 0, 1)]) {
        let centre = CGPoint(x: content.minX + content.width * 0.28, y: top - content.height * 0.10)
        ctx.drawRadialGradient(
            pool, startCenter: centre, startRadius: 0,
            endCenter: centre, endRadius: content.width * 0.62, options: [])
    }

    if let bounce = gradient([(0xFFFF_FF, 0, 0), (0xA8BC_E0, 0.12, 1)]) {
        ctx.drawLinearGradient(
            bounce, start: CGPoint(x: 0, y: bottom + content.height * 0.22),
            end: CGPoint(x: 0, y: bottom), options: [])
    }
}

/// The database mark: three stacked bands under an elliptical top face.
///
/// Drawn as solid geometry rather than an outline. A stroked glyph loses its
/// interior at 16pt, where this icon is a 16-pixel square in a sidebar.
private func drawDatabase(_ ctx: CGContext) {
    // Sized to fill the plate the way platform icons do. An undersized mark
    // floating in a large field is the most common way a first icon reads as
    // unfinished.
    let width = content.width * 0.54
    let height = content.width * 0.56
    let ry = width * 0.155
    let cx = content.midX
    let cy = content.midY
    let x = cx - width / 2
    let topY = cy + height / 2 - ry      // centre of the top ellipse
    let bottomY = cy - height / 2 + ry   // centre of the bottom ellipse

    // Accent glow. Ties the icon to the app's selection colour and lifts the
    // glyph off a background that is otherwise the same value as it.
    if let glow = gradient([(0x6366_F1, 0.42, 0), (0x6366_F1, 0.10, 0.55), (0x6366_F1, 0, 1)]) {
        let centre = CGPoint(x: cx, y: cy)
        ctx.drawRadialGradient(
            glow, startCenter: centre, startRadius: 0,
            endCenter: centre, endRadius: width * 0.95, options: [])
    }

    let body = CGRect(x: x, y: bottomY, width: width, height: topY - bottomY)
    let bottomCap = CGRect(x: x, y: bottomY - ry, width: width, height: ry * 2)
    let topCap = CGRect(x: x, y: topY - ry, width: width, height: ry * 2)
    let bounds = CGRect(
        x: x, y: bottomY - ry, width: width, height: (topY + ry) - (bottomY - ry))

    // One gradient drawn through several clips rather than a single unioned
    // path: the shapes overlap, and unioning them by winding rule depends on
    // subpath direction in a way that is easy to get subtly wrong.
    if let g = gradient([(0xF8FA_FC, 1, 0), (0xC6D2_E6, 1, 0.62), (0x9AA9_C4, 1, 1)]) {
        for shape in [body, bottomCap] {
            ctx.saveGState()
            shape == body ? ctx.addRect(shape) : ctx.addEllipse(in: shape)
            ctx.clip()
            ctx.drawLinearGradient(
                g, start: CGPoint(x: 0, y: bounds.maxY),
                end: CGPoint(x: 0, y: bounds.minY), options: [])
            ctx.restoreGState()
        }
    }

    // Band separators. Only the front half of each ellipse is visible on a
    // cylinder, so each is clipped to the region at or below its own centre.
    //
    // Heavier and darker than looks right at 1024: at 32pt a band is barely one
    // pixel, and the mark stops reading as a database the moment they vanish.
    ctx.setStrokeColor(color(0x2B31_3D, 0.72))
    ctx.setLineWidth(canvas * 0.016)
    for t in [0.36, 0.72] as [CGFloat] {
        let yc = topY - (topY - bottomY) * t
        ctx.saveGState()
        ctx.clip(to: CGRect(x: x, y: yc - ry * 1.2, width: width, height: ry * 1.2))
        ctx.strokeEllipse(in: CGRect(x: x, y: yc - ry, width: width, height: ry * 2))
        ctx.restoreGState()
    }

    // Top face, brighter so the cylinder reads as lit from above.
    if let g = gradient([(0xFFFF_FF, 1, 0), (0xDCE5_F2, 1, 1)]) {
        ctx.saveGState()
        ctx.addEllipse(in: topCap)
        ctx.clip()
        ctx.drawLinearGradient(
            g, start: CGPoint(x: 0, y: topCap.maxY),
            end: CGPoint(x: 0, y: topCap.minY), options: [])
        ctx.restoreGState()
    }
}

/// Hairlines along the plate's rim: a bright one at the top where light catches
/// the edge, and a dark outer line so the icon still has a boundary against a
/// light Finder background.
private func drawEdges(_ ctx: CGContext, shape: CGPath, top: CGFloat, bottom: CGFloat) {
    ctx.saveGState()
    ctx.addPath(shape)
    ctx.clip()
    ctx.addPath(squircle(content.insetBy(dx: 3, dy: 3), radius: cornerRadius - 3))
    ctx.setLineWidth(6)
    if let g = gradient([(0xFFFF_FF, 0.34, 0), (0xFFFF_FF, 0, 0.45)]) {
        ctx.replacePathWithStrokedPath()
        ctx.clip()
        ctx.drawLinearGradient(
            g, start: CGPoint(x: 0, y: top), end: CGPoint(x: 0, y: bottom), options: [])
    }
    ctx.restoreGState()

    ctx.addPath(squircle(content.insetBy(dx: 0.5, dy: 0.5), radius: cornerRadius))
    ctx.setStrokeColor(color(0x0000_00, 0.35))
    ctx.setLineWidth(2)
    ctx.strokePath()
}

// MARK: - Output

func render(size: Int) -> Data? {
    guard let ctx = CGContext(
        data: nil, width: size, height: size, bitsPerComponent: 8, bytesPerRow: 0,
        space: CGColorSpaceCreateDeviceRGB(),
        bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)
    else { return nil }
    ctx.setShouldAntialias(true)
    ctx.interpolationQuality = .high
    drawIcon(in: ctx, size: CGFloat(size))
    guard let image = ctx.makeImage() else { return nil }
    return NSBitmapImageRep(cgImage: image).representation(using: .png, properties: [:])
}

let output = CommandLine.arguments.count > 1
    ? CommandLine.arguments[1]
    : "apps/macos/Resources/AppIcon.icns"

// Every entry is rendered from the vectors at its own size. Downsampling one
// 1024 bitmap is what turns a small icon into mush.
let entries: [(name: String, size: Int)] = [
    ("icon_16x16", 16), ("icon_16x16@2x", 32),
    ("icon_32x32", 32), ("icon_32x32@2x", 64),
    ("icon_128x128", 128), ("icon_128x128@2x", 256),
    ("icon_256x256", 256), ("icon_256x256@2x", 512),
    ("icon_512x512", 512), ("icon_512x512@2x", 1024),
]

let workDir = URL(fileURLWithPath: NSTemporaryDirectory())
    .appendingPathComponent("AppIcon-\(ProcessInfo.processInfo.processIdentifier).iconset")
try? FileManager.default.removeItem(at: workDir)
try FileManager.default.createDirectory(at: workDir, withIntermediateDirectories: true)

for entry in entries {
    guard let data = render(size: entry.size) else {
        FileHandle.standardError.write(Data("failed to render \(entry.name)\n".utf8))
        exit(1)
    }
    try data.write(to: workDir.appendingPathComponent("\(entry.name).png"))
}

let outputURL = URL(fileURLWithPath: output)
try? FileManager.default.createDirectory(
    at: outputURL.deletingLastPathComponent(), withIntermediateDirectories: true)

let iconutil = Process()
iconutil.executableURL = URL(fileURLWithPath: "/usr/bin/iconutil")
iconutil.arguments = ["-c", "icns", workDir.path, "-o", outputURL.path]
try iconutil.run()
iconutil.waitUntilExit()
guard iconutil.terminationStatus == 0 else {
    FileHandle.standardError.write(Data("iconutil failed\n".utf8))
    exit(1)
}
try? FileManager.default.removeItem(at: workDir)

let bytes = (try? FileManager.default.attributesOfItem(atPath: outputURL.path)[.size]) ?? 0
print("icon: \(outputURL.path) (\(bytes) bytes, \(entries.count) sizes)")

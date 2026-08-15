import AppKit
import Foundation

guard CommandLine.arguments.count == 3 else {
    fputs("usage: apply-rounded-mask.swift INPUT.png OUTPUT.png\n", stderr)
    exit(2)
}

let inputPath = CommandLine.arguments[1]
let outputPath = CommandLine.arguments[2]
let canvasSize = NSSize(width: 1024, height: 1024)
let canvasRect = NSRect(origin: .zero, size: canvasSize)

guard let source = NSImage(contentsOfFile: inputPath) else {
    fputs("failed to read source icon at \(inputPath)\n", stderr)
    exit(1)
}

guard let bitmap = NSBitmapImageRep(
    bitmapDataPlanes: nil,
    pixelsWide: 1024,
    pixelsHigh: 1024,
    bitsPerSample: 8,
    samplesPerPixel: 4,
    hasAlpha: true,
    isPlanar: false,
    colorSpaceName: .deviceRGB,
    bytesPerRow: 0,
    bitsPerPixel: 0
) else {
    fputs("failed to create icon bitmap\n", stderr)
    exit(1)
}

bitmap.size = canvasSize

guard let graphicsContext = NSGraphicsContext(bitmapImageRep: bitmap) else {
    fputs("failed to create icon graphics context\n", stderr)
    exit(1)
}

NSGraphicsContext.saveGraphicsState()
NSGraphicsContext.current = graphicsContext
graphicsContext.cgContext.clear(canvasRect)

let tile = NSBezierPath(
    roundedRect: NSRect(x: 64, y: 64, width: 896, height: 896),
    xRadius: 208,
    yRadius: 208
)
tile.addClip()
source.draw(in: canvasRect, from: .zero, operation: .copy, fraction: 1)

graphicsContext.flushGraphics()
NSGraphicsContext.restoreGraphicsState()

guard let png = bitmap.representation(using: .png, properties: [:]) else {
    fputs("failed to encode rounded icon\n", stderr)
    exit(1)
}

do {
    try png.write(to: URL(fileURLWithPath: outputPath), options: .atomic)
} catch {
    fputs("failed to write rounded icon at \(outputPath): \(error)\n", stderr)
    exit(1)
}

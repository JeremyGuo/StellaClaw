import AppKit
import CoreGraphics
import Foundation

struct IconImage {
    let filename: String
    let pixels: Int
}

let iconSetURL = URL(fileURLWithPath: CommandLine.arguments[1])

let images: [IconImage] = [
    IconImage(filename: "Icon-App-20x20@2x.png", pixels: 40),
    IconImage(filename: "Icon-App-20x20@3x.png", pixels: 60),
    IconImage(filename: "Icon-App-29x29@2x.png", pixels: 58),
    IconImage(filename: "Icon-App-29x29@3x.png", pixels: 87),
    IconImage(filename: "Icon-App-40x40@2x.png", pixels: 80),
    IconImage(filename: "Icon-App-40x40@3x.png", pixels: 120),
    IconImage(filename: "Icon-App-60x60@2x.png", pixels: 120),
    IconImage(filename: "Icon-App-60x60@3x.png", pixels: 180),
    IconImage(filename: "Icon-App-20x20@1x.png", pixels: 20),
    IconImage(filename: "Icon-App-29x29@1x.png", pixels: 29),
    IconImage(filename: "Icon-App-40x40@1x.png", pixels: 40),
    IconImage(filename: "Icon-App-76x76@1x.png", pixels: 76),
    IconImage(filename: "Icon-App-76x76@2x.png", pixels: 152),
    IconImage(filename: "Icon-App-83.5x83.5@2x.png", pixels: 167),
    IconImage(filename: "Icon-App-16x16@1x.png", pixels: 16),
    IconImage(filename: "Icon-App-16x16@2x.png", pixels: 32),
    IconImage(filename: "Icon-App-32x32@1x.png", pixels: 32),
    IconImage(filename: "Icon-App-32x32@2x.png", pixels: 64),
    IconImage(filename: "Icon-App-128x128@1x.png", pixels: 128),
    IconImage(filename: "Icon-App-128x128@2x.png", pixels: 256),
    IconImage(filename: "Icon-App-256x256@1x.png", pixels: 256),
    IconImage(filename: "Icon-App-256x256@2x.png", pixels: 512),
    IconImage(filename: "Icon-App-512x512@1x.png", pixels: 512),
    IconImage(filename: "Icon-App-512x512@2x.png", pixels: 1024),
    IconImage(filename: "Icon-App-1024x1024@1x.png", pixels: 1024)
]

func drawIcon(pixels: Int) throws -> CGImage {
    let width = pixels
    let height = pixels
    let bytesPerPixel = 4
    let bytesPerRow = width * bytesPerPixel
    var data = Data(repeating: 0, count: height * bytesPerRow)

    guard let context = data.withUnsafeMutableBytes({ rawBuffer -> CGContext? in
        guard let baseAddress = rawBuffer.baseAddress else {
            return nil
        }
        return CGContext(
            data: baseAddress,
            width: width,
            height: height,
            bitsPerComponent: 8,
            bytesPerRow: bytesPerRow,
            space: CGColorSpaceCreateDeviceRGB(),
            bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
        )
    }) else {
        throw NSError(domain: "IconGenerator", code: 1, userInfo: [NSLocalizedDescriptionKey: "Failed to create bitmap context"])
    }

    let size = CGFloat(pixels)
    context.setAllowsAntialiasing(true)
    context.setShouldAntialias(true)

    drawFlatBackground(in: context, size: size)
    drawFlatMark(in: context, size: size)

    guard let image = context.makeImage() else {
        throw NSError(domain: "IconGenerator", code: 2, userInfo: [NSLocalizedDescriptionKey: "Failed to create CGImage"])
    }
    return image
}

func drawFlatBackground(in context: CGContext, size: CGFloat) {
    context.setFillColor(NSColor(calibratedRed: 0.055, green: 0.067, blue: 0.09, alpha: 1).cgColor)
    context.fill(CGRect(x: 0, y: 0, width: size, height: size))

    let inset = size * 0.118
    let card = CGRect(x: inset, y: inset, width: size - inset * 2, height: size - inset * 2)
    let radius = size * 0.176
    let cardPath = CGPath(roundedRect: card, cornerWidth: radius, cornerHeight: radius, transform: nil)
    context.addPath(cardPath)
    context.setFillColor(NSColor(calibratedRed: 0.09, green: 0.108, blue: 0.145, alpha: 1).cgColor)
    context.fillPath()

    context.addPath(cardPath)
    context.setStrokeColor(NSColor(calibratedRed: 1, green: 1, blue: 1, alpha: 0.08).cgColor)
    context.setLineWidth(max(1, size * 0.012))
    context.strokePath()
}

func drawFlatMark(in context: CGContext, size: CGFloat) {
    let lineWidth = size * 0.145
    let center = CGPoint(x: size * 0.5, y: size * 0.5)
    let span = size * 0.47

    context.saveGState()
    context.setLineCap(.round)
    context.setLineJoin(.round)

    let whitePath = CGMutablePath()
    whitePath.move(to: CGPoint(x: center.x - span * 0.55, y: center.y - span * 0.55))
    whitePath.addLine(to: CGPoint(x: center.x + span * 0.55, y: center.y + span * 0.55))
    context.addPath(whitePath)
    context.setStrokeColor(NSColor.white.cgColor)
    context.setLineWidth(lineWidth)
    context.strokePath()

    let bluePath = CGMutablePath()
    bluePath.move(to: CGPoint(x: center.x - span * 0.55, y: center.y + span * 0.55))
    bluePath.addLine(to: CGPoint(x: center.x + span * 0.55, y: center.y - span * 0.55))
    context.addPath(bluePath)
    context.setStrokeColor(NSColor(calibratedRed: 0.0, green: 0.478, blue: 1.0, alpha: 1).cgColor)
    context.setLineWidth(lineWidth)
    context.strokePath()

    context.restoreGState()

    let dotRadius = size * 0.044
    let dotRect = CGRect(
        x: size * 0.686 - dotRadius,
        y: size * 0.704 - dotRadius,
        width: dotRadius * 2,
        height: dotRadius * 2
    )
    context.setFillColor(NSColor(calibratedRed: 0.0, green: 0.478, blue: 1.0, alpha: 1).cgColor)
    context.fillEllipse(in: dotRect)
}

func writePNG(_ image: CGImage, to url: URL) throws {
    let bitmap = NSBitmapImageRep(cgImage: image)
    guard let png = bitmap.representation(using: .png, properties: [:]) else {
        throw NSError(domain: "IconGenerator", code: 3, userInfo: [NSLocalizedDescriptionKey: "Failed to encode PNG"])
    }
    try png.write(to: url, options: [.atomic])
}

try FileManager.default.createDirectory(at: iconSetURL, withIntermediateDirectories: true)

for icon in images {
    let image = try drawIcon(pixels: icon.pixels)
    try writePNG(image, to: iconSetURL.appendingPathComponent(icon.filename))
}

import CoreGraphics
import Foundation
import ImageIO
import UniformTypeIdentifiers

struct IconImage {
    let filename: String
    let pixels: Int
}

let iconSetURL = URL(fileURLWithPath: CommandLine.arguments[1])
let defaultSourceURL = iconSetURL
    .deletingLastPathComponent()
    .deletingLastPathComponent()
    .deletingLastPathComponent()
    .deletingLastPathComponent()
    .deletingLastPathComponent()
    .appendingPathComponent("shared/assets/icons/stellacodex/StellacodeX-icon-light.png")
let sourceURL = CommandLine.arguments.count > 2
    ? URL(fileURLWithPath: CommandLine.arguments[2])
    : defaultSourceURL

guard let source = CGImageSourceCreateWithURL(sourceURL as CFURL, nil),
      let sourceImage = CGImageSourceCreateImageAtIndex(source, 0, nil) else {
    fatalError("Failed to load source icon: \(sourceURL.path)")
}

let markImage = try makeTransparentMark(from: sourceImage)

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
    let bytesPerPixel = 4
    let bytesPerRow = pixels * bytesPerPixel
    var data = Data(repeating: 0, count: pixels * bytesPerRow)
    guard let context = data.withUnsafeMutableBytes({ rawBuffer -> CGContext? in
        guard let baseAddress = rawBuffer.baseAddress else {
            return nil
        }
        return CGContext(
            data: baseAddress,
            width: pixels,
            height: pixels,
            bitsPerComponent: 8,
            bytesPerRow: bytesPerRow,
            space: CGColorSpaceCreateDeviceRGB(),
            bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
        )
    }) else {
        throw NSError(domain: "IconGenerator", code: 1, userInfo: [NSLocalizedDescriptionKey: "Failed to create bitmap context"])
    }

    let size = CGFloat(pixels)
    let rect = CGRect(x: 0, y: 0, width: size, height: size)
    let plateRect = rect.insetBy(dx: size * 0.085, dy: size * 0.085)
    let iconPath = makeContinuousIconPath(in: plateRect)
    let markRect = plateRect

    context.clear(rect)
    context.interpolationQuality = .high
    context.setAllowsAntialiasing(true)
    context.setShouldAntialias(true)
    context.addPath(iconPath)
    context.clip()

    drawGlassBase(in: context, rect: plateRect)

    context.saveGState()
    context.setShadow(
        offset: CGSize(width: size * 0.012, height: -size * 0.016),
        blur: size * 0.020,
        color: CGColor(red: 0.10, green: 0.13, blue: 0.18, alpha: 0.28)
    )
    context.draw(markImage, in: markRect)
    context.restoreGState()

    context.draw(markImage, in: markRect)

    guard let image = context.makeImage() else {
        throw NSError(domain: "IconGenerator", code: 2, userInfo: [NSLocalizedDescriptionKey: "Failed to create CGImage"])
    }
    return image
}

func makeTransparentMark(from image: CGImage) throws -> CGImage {
    let width = image.width
    let height = image.height
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
        throw NSError(domain: "IconGenerator", code: 5, userInfo: [NSLocalizedDescriptionKey: "Failed to create mark context"])
    }

    context.draw(image, in: CGRect(x: 0, y: 0, width: width, height: height))
    data.withUnsafeMutableBytes { rawBuffer in
        let pixels = rawBuffer.bindMemory(to: UInt8.self)
        var index = 0
        while index < pixels.count {
            let red = CGFloat(pixels[index])
            let green = CGFloat(pixels[index + 1])
            let blue = CGFloat(pixels[index + 2])
            let alpha = CGFloat(pixels[index + 3]) / 255.0
            let distanceFromWhite = sqrt(pow(255.0 - red, 2) + pow(255.0 - green, 2) + pow(255.0 - blue, 2))
            let opacity = min(1.0, max(0.0, (distanceFromWhite - 9.0) / 52.0)) * alpha
            pixels[index] = UInt8(max(0, min(255, red * opacity)))
            pixels[index + 1] = UInt8(max(0, min(255, green * opacity)))
            pixels[index + 2] = UInt8(max(0, min(255, blue * opacity)))
            pixels[index + 3] = UInt8(max(0, min(255, 255.0 * opacity)))
            index += 4
        }
    }

    guard let mark = context.makeImage() else {
        throw NSError(domain: "IconGenerator", code: 6, userInfo: [NSLocalizedDescriptionKey: "Failed to create mark image"])
    }
    return mark
}

func drawGlassBase(in context: CGContext, rect: CGRect) {
    let colorSpace = CGColorSpaceCreateDeviceRGB()
    let colors = [
        CGColor(red: 1.0, green: 1.0, blue: 0.985, alpha: 1.0),
        CGColor(red: 0.955, green: 0.970, blue: 0.980, alpha: 1.0),
        CGColor(red: 0.900, green: 0.925, blue: 0.950, alpha: 1.0)
    ] as CFArray
    let locations: [CGFloat] = [0.0, 0.58, 1.0]
    if let gradient = CGGradient(colorsSpace: colorSpace, colors: colors, locations: locations) {
        context.drawLinearGradient(
            gradient,
            start: CGPoint(x: rect.minX, y: rect.maxY),
            end: CGPoint(x: rect.maxX, y: rect.minY),
            options: []
        )
    }

    let size = rect.width
    var seed: UInt32 = 0x51E11A
    let step = max(1, Int(size / 160))
    for y in stride(from: 0, to: Int(size), by: step) {
        for x in stride(from: 0, to: Int(size), by: step) {
            seed = seed &* 1664525 &+ 1013904223
            let value = CGFloat((seed >> 24) & 0xff) / 255.0
            let alpha = 0.006 + value * 0.010
            context.setFillColor(CGColor(red: 1, green: 1, blue: 1, alpha: alpha))
            context.fill(CGRect(x: CGFloat(x), y: CGFloat(y), width: CGFloat(step), height: CGFloat(step)))
        }
    }

    if let highlight = CGGradient(colorsSpace: colorSpace, colors: [
        CGColor(red: 1, green: 1, blue: 1, alpha: 0.18),
        CGColor(red: 1, green: 1, blue: 1, alpha: 0.0)
    ] as CFArray, locations: [0.0, 1.0]) {
        context.drawLinearGradient(
            highlight,
            start: CGPoint(x: rect.midX, y: rect.maxY),
            end: CGPoint(x: rect.midX, y: rect.midY),
            options: []
        )
    }
}

func makeContinuousIconPath(in rect: CGRect) -> CGPath {
    let path = CGMutablePath()
    let points = 96
    let exponent: CGFloat = 5.0
    let halfWidth = rect.width / 2
    let halfHeight = rect.height / 2
    let center = CGPoint(x: rect.midX, y: rect.midY)

    for index in 0...points {
        let angle = CGFloat(index) / CGFloat(points) * 2.0 * .pi
        let cosValue = cos(angle)
        let sinValue = sin(angle)
        let x = center.x + halfWidth * signedPower(cosValue, 2.0 / exponent)
        let y = center.y + halfHeight * signedPower(sinValue, 2.0 / exponent)
        if index == 0 {
            path.move(to: CGPoint(x: x, y: y))
        } else {
            path.addLine(to: CGPoint(x: x, y: y))
        }
    }
    path.closeSubpath()
    return path
}

func signedPower(_ value: CGFloat, _ power: CGFloat) -> CGFloat {
    let magnitude = pow(abs(value), power)
    return value < 0 ? -magnitude : magnitude
}

func writePNG(_ image: CGImage, to url: URL) throws {
    guard let destination = CGImageDestinationCreateWithURL(url as CFURL, UTType.png.identifier as CFString, 1, nil) else {
        throw NSError(domain: "IconGenerator", code: 3, userInfo: [NSLocalizedDescriptionKey: "Failed to create PNG destination"])
    }
    CGImageDestinationAddImage(destination, image, nil)
    if !CGImageDestinationFinalize(destination) {
        throw NSError(domain: "IconGenerator", code: 4, userInfo: [NSLocalizedDescriptionKey: "Failed to encode PNG"])
    }
}

try FileManager.default.createDirectory(at: iconSetURL, withIntermediateDirectories: true)

for icon in images {
    let image = try drawIcon(pixels: icon.pixels)
    try writePNG(image, to: iconSetURL.appendingPathComponent(icon.filename))
}

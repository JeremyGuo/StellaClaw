import Foundation

enum TarGzipArchive {
    struct Entry {
        var name: String
        var data: Data
    }

    static func singleFile(name: String, data: Data) throws -> Data {
        try files([Entry(name: name, data: data)])
    }

    static func files(_ entries: [Entry]) throws -> Data {
        let tar = try tarFiles(entries)
        return gzipStored(data: tar)
    }

    private static func sanitizedArchiveName(_ name: String) -> String {
        let value = name
            .split(separator: "/")
            .last
            .map(String.init)?
            .trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
        return value.isEmpty ? "upload.bin" : value
    }

    private static func tarFiles(_ entries: [Entry]) throws -> Data {
        var output = Data()
        for entry in entries {
            let safeName = sanitizedArchiveName(entry.name)
            var header = [UInt8](repeating: 0, count: 512)
            write(safeName, into: &header, offset: 0, length: 100)
            writeOctal(0o644, into: &header, offset: 100, length: 8)
            writeOctal(0, into: &header, offset: 108, length: 8)
            writeOctal(0, into: &header, offset: 116, length: 8)
            writeOctal(Int64(entry.data.count), into: &header, offset: 124, length: 12)
            writeOctal(Int64(Date().timeIntervalSince1970), into: &header, offset: 136, length: 12)
            for index in 148..<156 {
                header[index] = 0x20
            }
            header[156] = Character("0").asciiValue ?? 0x30
            write("ustar", into: &header, offset: 257, length: 6)
            write("00", into: &header, offset: 263, length: 2)

            let checksum = header.reduce(0) { $0 + Int($1) }
            writeChecksum(checksum, into: &header)

            output.append(contentsOf: header)
            output.append(entry.data)
            let padding = (512 - (entry.data.count % 512)) % 512
            if padding > 0 {
                output.append(Data(repeating: 0, count: padding))
            }
        }
        output.append(Data(repeating: 0, count: 1024))
        return output
    }

    private static func gzipStored(data: Data) -> Data {
        var output = Data([0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff])
        if data.isEmpty {
            output.append(contentsOf: [0x01, 0x00, 0x00, 0xff, 0xff])
            output.append(littleEndianBytes(CRC32.checksum(data)))
            output.append(littleEndianBytes(0))
            return output
        }
        var offset = 0
        while offset < data.count {
            let remaining = data.count - offset
            let blockLength = min(remaining, 65_535)
            let isFinal = offset + blockLength >= data.count
            output.append(isFinal ? 0x01 : 0x00)
            let length = UInt16(blockLength)
            let inverted = ~length
            output.append(UInt8(length & 0xff))
            output.append(UInt8((length >> 8) & 0xff))
            output.append(UInt8(inverted & 0xff))
            output.append(UInt8((inverted >> 8) & 0xff))
            output.append(data.subdata(in: offset..<(offset + blockLength)))
            offset += blockLength
        }
        output.append(littleEndianBytes(CRC32.checksum(data)))
        output.append(littleEndianBytes(UInt32(data.count & 0xffff_ffff)))
        return output
    }

    private static func write(_ string: String, into header: inout [UInt8], offset: Int, length: Int) {
        let bytes = Array(string.utf8.prefix(length))
        for index in 0..<bytes.count {
            header[offset + index] = bytes[index]
        }
    }

    private static func writeOctal(_ value: Int64, into header: inout [UInt8], offset: Int, length: Int) {
        let text = String(value, radix: 8)
        let padded = String(repeating: "0", count: max(0, length - text.count - 1)) + text
        write(padded, into: &header, offset: offset, length: length - 1)
        header[offset + length - 1] = 0
    }

    private static func writeChecksum(_ value: Int, into header: inout [UInt8]) {
        let text = String(value, radix: 8)
        let padded = String(repeating: "0", count: max(0, 6 - text.count)) + text
        write(padded, into: &header, offset: 148, length: 6)
        header[154] = 0
        header[155] = 0x20
    }

    private static func littleEndianBytes(_ value: UInt32) -> Data {
        var value = value.littleEndian
        return Data(bytes: &value, count: MemoryLayout<UInt32>.size)
    }
}

private enum CRC32 {
    static func checksum(_ data: Data) -> UInt32 {
        var crc: UInt32 = 0xffff_ffff
        for byte in data {
            crc ^= UInt32(byte)
            for _ in 0..<8 {
                if crc & 1 == 1 {
                    crc = (crc >> 1) ^ 0xedb8_8320
                } else {
                    crc >>= 1
                }
            }
        }
        return crc ^ 0xffff_ffff
    }
}

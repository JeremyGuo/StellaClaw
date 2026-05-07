import { formatBytes } from './format';

const MAX_UPLOAD_FILE_COUNT = 200;
const MAX_UPLOAD_UNCOMPRESSED_BYTES = 30 * 1024 * 1024;

export function nextFrame() {
  return new Promise((resolve) => requestAnimationFrame(resolve));
}

export function uploadPayloadStats(files) {
  return files.reduce(
    (acc, file) => {
      if (!file.isDirectory) {
        acc.fileCount += 1;
        acc.bytes += file.data?.byteLength || 0;
      }
      return acc;
    },
    { fileCount: 0, bytes: 0 }
  );
}

export function trackUploadFile(stats, file) {
  stats.fileCount += 1;
  stats.bytes += file.size || 0;
  if (stats.fileCount > MAX_UPLOAD_FILE_COUNT) {
    throw new Error(`一次最多上传 ${MAX_UPLOAD_FILE_COUNT} 个文件`);
  }
  if (stats.bytes > MAX_UPLOAD_UNCOMPRESSED_BYTES) {
    throw new Error(`上传文件过大（原始大小超过 ${formatBytes(MAX_UPLOAD_UNCOMPRESSED_BYTES)}）`);
  }
}

export async function collectDroppedFiles(dataTransferItems) {
  const entries = [];
  const fsEntries = [];
  const plainFiles = [];
  const stats = { fileCount: 0, bytes: 0 };
  for (let i = 0; i < dataTransferItems.length; i += 1) {
    const item = dataTransferItems[i];
    if (item.kind !== 'file') continue;
    const entry = item.webkitGetAsEntry ? item.webkitGetAsEntry() : null;
    if (entry) {
      fsEntries.push(entry);
    } else {
      const file = item.getAsFile();
      if (file) plainFiles.push(file);
    }
  }
  for (const file of plainFiles) {
    trackUploadFile(stats, file);
    entries.push({ relativePath: file.name, data: await file.arrayBuffer(), isDirectory: false });
    if (entries.length % 20 === 0) await nextFrame();
  }
  for (const entry of fsEntries) {
    await traverseDroppedEntry(entry, '', entries, stats);
  }
  return entries;
}

export async function traverseDroppedEntry(entry, parentPath, results, stats) {
  const fullPath = parentPath ? `${parentPath}/${entry.name}` : entry.name;
  if (entry.isFile) {
    const file = await new Promise((resolve, reject) => entry.file(resolve, reject));
    trackUploadFile(stats, file);
    results.push({ relativePath: fullPath, data: await file.arrayBuffer(), isDirectory: false });
    if (results.length % 20 === 0) await nextFrame();
    return;
  }
  if (!entry.isDirectory) return;
  results.push({ relativePath: `${fullPath}/`, data: new ArrayBuffer(0), isDirectory: true });
  const reader = entry.createReader();
  const children = await new Promise((resolve, reject) => {
    const all = [];
    const readBatch = () => {
      reader.readEntries((batch) => {
        if (batch.length === 0) {
          resolve(all);
        } else {
          all.push(...batch);
          readBatch();
        }
      }, reject);
    };
    readBatch();
  });
  for (const child of children) {
    await traverseDroppedEntry(child, fullPath, results, stats);
  }
}

export async function packFilesToTarGz(fileEntries) {
  const blocks = [];
  for (let index = 0; index < fileEntries.length; index += 1) {
    const entry = fileEntries[index];
    const nameBytes = new TextEncoder().encode(entry.relativePath);
    if (nameBytes.length > 99) {
      const paxContent = new TextEncoder().encode(`path=${entry.relativePath}\n`);
      blocks.push(createTarHeader('PaxHeader', paxContent.length, 'x'));
      blocks.push(padToBlock(paxContent));
    }
    const data = new Uint8Array(entry.data);
    blocks.push(createTarHeader(entry.relativePath.length > 99 ? entry.relativePath.slice(0, 99) : entry.relativePath, data.length, entry.isDirectory ? '5' : '0'));
    if (data.length > 0) blocks.push(padToBlock(data));
    if (index > 0 && index % 20 === 0) await nextFrame();
  }
  blocks.push(new Uint8Array(1024));
  const tarData = concatenateBuffers(blocks);
  if (typeof CompressionStream === 'function') {
    const compressed = await new Response(
      new Blob([tarData]).stream().pipeThrough(new CompressionStream('gzip'))
    ).arrayBuffer();
    return compressed;
  }
  if (window.stellacode2?.gzip) {
    return window.stellacode2.gzip(tarData);
  }
  throw new Error('当前运行环境不支持 gzip 压缩');
}

export function createTarHeader(name, size, typeFlag) {
  const header = new Uint8Array(512);
  const encoder = new TextEncoder();
  header.set(encoder.encode(name.slice(0, 99)), 0);
  header.set(encoder.encode('0000755\0'), 100);
  header.set(encoder.encode('0001000\0'), 108);
  header.set(encoder.encode('0001000\0'), 116);
  header.set(encoder.encode(size.toString(8).padStart(11, '0') + '\0'), 124);
  header.set(encoder.encode(Math.floor(Date.now() / 1000).toString(8).padStart(11, '0') + '\0'), 136);
  header.set(encoder.encode('        '), 148);
  header[156] = encoder.encode(typeFlag || '0')[0];
  header.set(encoder.encode('ustar\0'), 257);
  header.set(encoder.encode('00'), 263);
  let checksum = 0;
  for (let index = 0; index < 512; index += 1) checksum += header[index];
  header.set(encoder.encode(checksum.toString(8).padStart(6, '0') + '\0 '), 148);
  return header;
}

export function padToBlock(data) {
  const remainder = data.length % 512;
  if (remainder === 0) return data;
  const padded = new Uint8Array(data.length + (512 - remainder));
  padded.set(data);
  return padded;
}

export function concatenateBuffers(arrays) {
  let total = 0;
  for (const array of arrays) total += array.length;
  const result = new Uint8Array(total);
  let offset = 0;
  for (const array of arrays) {
    result.set(array, offset);
    offset += array.length;
  }
  return result;
}

function readFileAsBase64(file) {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onerror = () => reject(reader.error || new Error('Failed to read file'));
    reader.onload = () => {
      const value = String(reader.result || '');
      resolve(value.includes(',') ? value.split(',').pop() : value);
    };
    reader.readAsDataURL(file);
  });
}

function imageSizeFromUrl(url) {
  return new Promise((resolve) => {
    if (!url) {
      resolve({});
      return;
    }
    const image = new Image();
    image.onload = () => resolve({ width: image.naturalWidth, height: image.naturalHeight });
    image.onerror = () => resolve({});
    image.src = url;
  });
}

function fileMediaType(file) {
  return file?.type || 'application/octet-stream';
}

export function isImageFileObject(file) {
  return String(fileMediaType(file)).toLowerCase().startsWith('image/');
}

export async function composerAttachmentFromFile(file, fallbackName = '') {
  const name = file?.name || fallbackName || 'attachment';
  const mediaType = fileMediaType(file);
  const previewUrl = isImageFileObject(file) ? URL.createObjectURL(file) : '';
  const imageSize = previewUrl ? await imageSizeFromUrl(previewUrl) : {};
  return {
    id: `${Date.now()}-${Math.random().toString(36).slice(2)}`,
    name,
    media_type: mediaType,
    size_bytes: file?.size || 0,
    data_base64: await readFileAsBase64(file),
    previewUrl,
    width: imageSize.width,
    height: imageSize.height
  };
}

export function outgoingAttachmentPayload(attachment) {
  return {
    name: attachment.name,
    media_type: attachment.media_type,
    uri: `data:${attachment.media_type || 'application/octet-stream'};base64,${attachment.data_base64}`,
    size_bytes: attachment.size_bytes,
    width: attachment.width,
    height: attachment.height
  };
}

export function selectionSummary(selection) {
  const locator = selection?.locator || {};
  if (locator.start_line && locator.end_line) {
    return `L${locator.start_line}-${locator.end_line}`;
  }
  if (locator.page) return `P${locator.page}`;
  const text = String(selection?.selected_text || '').replace(/\s+/g, ' ').trim();
  return text.length > 28 ? `${text.slice(0, 28)}...` : text || '选区';
}

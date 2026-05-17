export function messageText(message) {
  if (typeof message === 'string') return message;
  if (typeof message?.text_with_attachment_markers === 'string' && message.text_with_attachment_markers.trim()) return message.text_with_attachment_markers;
  if (typeof message?.rendered_text === 'string' && message.rendered_text.trim()) return message.rendered_text;
  if (typeof message?.preview === 'string' && message.preview.trim()) return message.preview;
  if (typeof message?.text === 'string' && message.text.trim()) return message.text;
  if (typeof message?.content === 'string' && message.content.trim()) return message.content;
  const items = Array.isArray(message?.items) && message.items.length > 0
    ? message.items
    : Array.isArray(message?.data)
      ? message.data
      : [];
  if (items.length > 0) {
    const text = items.map(itemText).filter(Boolean).join('\n');
    if (text.trim()) return text;
  }
  return '';
}

function itemText(item) {
  if (typeof item === 'string') return item;
  if (!item || typeof item !== 'object') return '';
  const payload = item.payload && typeof item.payload === 'object' ? item.payload : {};
  if (typeof item.text === 'string') return item.text;
  if (typeof item.text_with_attachment_markers === 'string') return item.text_with_attachment_markers;
  if (typeof item.content === 'string') return item.content;
  if (typeof payload.text === 'string') return payload.text;
  if (item.type === 'reasoning') {
    const summary = Array.isArray(payload.codex_summary)
      ? payload.codex_summary.map((part) => (typeof part === 'string' ? part : part?.text || '')).filter(Boolean).join('\n')
      : typeof payload.codex_summary === 'string'
        ? payload.codex_summary
        : '';
    return payload.text || item.summary || summary;
  }
  if (item.type === 'selection_reference') {
    const selection = item.selection || payload || item;
    return `[selection] ${selection.file_path || selection.fileName || ''}`;
  }
  if (item.type === 'file') {
    const file = item.file || payload || item;
    return `[file] ${file.name || file.path || file.uri || ''}`;
  }
  return '';
}

export function fileExtension(path = '') {
  const name = String(path).split(/[\\/]/).pop()?.toLowerCase() || '';
  const index = name.lastIndexOf('.');
  return index >= 0 ? name.slice(index + 1) : '';
}

export function fileNameFromPath(path = '') {
  return String(path).split(/[\\/]/).filter(Boolean).pop() || '';
}

export function isMarkdownFile(path = '') {
  return ['md', 'markdown', 'mdown'].includes(fileExtension(path));
}

export function isImageFile(path = '') {
  return ['png', 'jpg', 'jpeg', 'gif', 'webp', 'bmp', 'svg', 'avif'].includes(fileExtension(path));
}

export function isPdfFile(path = '') {
  return fileExtension(path) === 'pdf';
}

export function isHtmlFile(path = '') {
  return ['html', 'htm'].includes(fileExtension(path));
}

export function isWordFile(path = '') {
  return ['docx', 'doc'].includes(fileExtension(path));
}

export function isPresentationFile(path = '') {
  return ['pptx', 'ppt', 'ppsx', 'pps', 'potx', 'pot'].includes(fileExtension(path));
}

export function imageMimeType(path = '') {
  const ext = fileExtension(path);
  if (ext === 'jpg') return 'image/jpeg';
  if (ext === 'svg') return 'image/svg+xml';
  return ext ? `image/${ext}` : 'application/octet-stream';
}

export function attachmentName(attachment) {
  return attachment?.name || attachment?.filename || fileNameFromPath(attachment?.path || attachment?.url || attachment?.uri || attachment?.file_uri || '') || 'attachment';
}

export function dataUrlFromPart(part, fallbackMime) {
  if (!part || typeof part !== 'object') return '';
  if (typeof part.data_url === 'string' && part.data_url) return part.data_url;
  const mediaType = part.media_type || part.mime_type || part.mime || fallbackMime || 'application/octet-stream';
  if (typeof part.data_base64 === 'string' && part.data_base64) {
    return `data:${mediaType};base64,${part.data_base64}`;
  }
  if (typeof part.base64 === 'string' && part.base64) {
    return `data:${mediaType};base64,${part.base64}`;
  }
  if (typeof part.data === 'string' && part.data) {
    if (part.encoding === 'base64' || /^[A-Za-z0-9+/=\s]+$/.test(part.data.slice(0, 80))) {
      return `data:${mediaType};base64,${part.data.replace(/\s/g, '')}`;
    }
    if (part.encoding === 'utf8' && mediaType === 'image/svg+xml') {
      return `data:image/svg+xml;charset=utf-8,${encodeURIComponent(part.data)}`;
    }
  }
  return '';
}

export function externalAttachmentUrl(value) {
  if (typeof value !== 'string' || !value) return '';
  if (value.startsWith('/api/')) return '';
  return value;
}

export function isImageAttachment(attachment) {
  const mediaType = String(attachment?.media_type || attachment?.mime_type || attachment?.mime || '').toLowerCase();
  const name = attachmentName(attachment);
  return attachment?.kind === 'image' || mediaType.startsWith('image/') || isImageFile(attachment?.path || name);
}

export function attachmentUrl(attachment) {
  if (!attachment) return '';
  const name = attachmentName(attachment);
  const fallbackMime = attachment.media_type || attachment.mime_type || imageMimeType(name);
  return (
    dataUrlFromPart(attachment.thumbnail, fallbackMime)
    || dataUrlFromPart(attachment.preview, fallbackMime)
    || dataUrlFromPart(attachment, fallbackMime)
    || externalAttachmentUrl(attachment.url)
    || externalAttachmentUrl(attachment.uri)
    || externalAttachmentUrl(attachment.file_uri)
    || externalAttachmentUrl(attachment.src)
  );
}

import { Fragment, memo, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import ReactMarkdown from 'react-markdown';
import rehypeHighlight from 'rehype-highlight';
import remarkGfm from 'remark-gfm';
import { ChevronDown, Copy, Download, FileText, Pin, Plus, Send, TerminalSquare } from 'lucide-react';
import * as Popover from '@radix-ui/react-popover';
import { attachmentName, attachmentUrl, fileExtension, isImageAttachment, messageText } from '../lib/fileUtils';
import { handleExternalLinkClick, isExternalUrl } from '../lib/externalLinks';
import { formatBytes, formatTokens, modelAlias, modelDisplayName } from '../lib/format';
import { displayMessages, firstMessageId, isExecutionMessage, isFinalAssistantMessage, liveActivitySignature, markerIndexes, messageKey, splitMessageForDisplay, tokenUsage, toolCardsForMessage } from '../lib/messageUtils';

const COMMANDS = [
  { command: '/model', label: '切换模型', description: '选择当前 Conversation 使用的模型', options: 'models' },
  { command: '/reasoning', label: '推理强度', description: '调整当前 Conversation 的 reasoning effort', options: 'reasoning' },
  { command: '/remote', label: '远程模式', description: '设置 SSH host 和工作目录', insert: '/remote ' },
  { command: '/remote off', label: '关闭远程', description: '切回本地工具执行', send: true },
  { command: '/continue', label: '继续', description: '继续最近中断的回合', send: true },
  { command: '/cancel', label: '取消', description: '停止当前正在处理的回合', send: true },
  { command: '/compact', label: '压缩上下文', description: '压缩当前对话上下文', send: true },
  { command: '/status', label: '状态', description: '显示当前会话状态', send: true }
];

const REASONING_EFFORTS = [
  { value: 'low', label: 'Low', description: '使用较低 reasoning effort' },
  { value: 'medium', label: 'Medium', description: '使用中等 reasoning effort' },
  { value: 'high', label: 'High', description: '使用较高 reasoning effort' },
  { value: 'xhigh', label: 'XHigh', description: '使用最高 reasoning effort' },
  { value: 'default', label: 'Default', description: '恢复模型默认 reasoning effort' }
];

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

function isImageFileObject(file) {
  return String(fileMediaType(file)).toLowerCase().startsWith('image/');
}

async function composerAttachmentFromFile(file, fallbackName = '') {
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

function outgoingAttachmentPayload(attachment) {
  return {
    name: attachment.name,
    media_type: attachment.media_type,
    uri: `data:${attachment.media_type || 'application/octet-stream'};base64,${attachment.data_base64}`,
    size_bytes: attachment.size_bytes,
    width: attachment.width,
    height: attachment.height
  };
}

function selectionSummary(selection) {
  const locator = selection?.locator || {};
  if (locator.start_line && locator.end_line) {
    return `L${locator.start_line}-${locator.end_line}`;
  }
  if (locator.page) return `P${locator.page}`;
  const text = String(selection?.selected_text || '').replace(/\s+/g, ' ').trim();
  return text.length > 28 ? `${text.slice(0, 28)}...` : text || '选区';
}

export function ChatWorkspace({ conversationKey: activeMessageScope, modelSelectionPending = false, messages, messagesReady, mode, hasOlder, onLoadOlder, onSend, onLoadModels, sending, processing = false, runningActivities, selectionReferences = [], onRemoveSelectionReference, onOpenAttachment, onDownloadAttachment }) {
  const renderedMessages = useMemo(() => displayMessages(messages), [messages]);
  const activitySignature = useMemo(() => liveActivitySignature(runningActivities || []), [runningActivities]);
  const oldestMessageKey = useMemo(() => firstMessageId(messages) || messages[0]?.id || messages[0]?.index || '', [messages]);
  const newestMessageKey = useMemo(() => {
    const message = messages.at(-1);
    if (!message) return '';
    return [
      message.id,
      message.index,
      message.message_time,
      message.preview,
      message.text
    ].map((value) => String(value ?? '')).join(':');
  }, [messages]);
  const modeLabel = typeof mode === 'string' ? mode : mode?.label || '本地';
  const modeTone = typeof mode === 'string' ? '' : mode?.tone || 'local';
  const modeTitle = typeof mode === 'string' ? mode : mode?.title || modeLabel;
  const [commandPanel, setCommandPanel] = useState('commands');
  const [models, setModels] = useState([]);
  const [modelsLoading, setModelsLoading] = useState(false);
  const [modelsError, setModelsError] = useState('');
  const [draft, setDraft] = useState('');
  const [composerAttachments, setComposerAttachments] = useState([]);
  const progressRef = useRef(null);
  const composerRef = useRef(null);
  const textareaRef = useRef(null);
  const fileInputRef = useRef(null);
  const composerAttachmentsRef = useRef([]);
  const composingRef = useRef(false);
  const lastComposingEnterAtRef = useRef(0);
  const lastEnterKeyUpAtRef = useRef(0);
  const suppressNextEnterRef = useRef(false);
  const scrollRef = useRef(null);
  const previousCountRef = useRef(0);
  const loadingOlderRef = useRef(false);
  const prependAdjustRef = useRef(null);
  const stickToBottomRef = useRef(true);
  const [toolStopNoticeReady, setToolStopNoticeReady] = useState(false);
  const currentActivity = (runningActivities || []).at(-1) || null;
  const inlineActivity = currentActivity?.id === 'waiting-response' ? null : currentActivity;
  const progressVisible = Boolean(currentActivity);
  const sessionRunning = Boolean(processing || currentActivity);
  const latestToolGroupIndex = useMemo(() => {
    for (let index = renderedMessages.length - 1; index >= 0; index -= 1) {
      if (renderedMessages[index]?.type === 'toolGroup') return index;
    }
    return -1;
  }, [renderedMessages]);
  const toolStopNoticeCandidate = useMemo(() => {
    if (!messagesReady || sending || processing || currentActivity || !messages.length) return false;
    const lastMessage = messages.at(-1);
    return isExecutionMessage(lastMessage);
  }, [messages, messagesReady, sending, processing, currentActivity]);
  const turnStoppedAfterTool = toolStopNoticeCandidate && toolStopNoticeReady;

  useEffect(() => {
    if (!toolStopNoticeCandidate) {
      setToolStopNoticeReady(false);
      return undefined;
    }
    const timer = window.setTimeout(() => {
      setToolStopNoticeReady(true);
    }, 1800);
    return () => window.clearTimeout(timer);
  }, [toolStopNoticeCandidate, newestMessageKey]);

  const updateComposerMetrics = () => {
    const node = composerRef.current;
    if (!node) return;
    const root = node.closest('.chat-workspace');
    const composer = node.querySelector('.composer');
    root?.style.setProperty('--composer-wrap-height', `${Math.ceil(node.getBoundingClientRect().height)}px`);
    if (composer) {
      root?.style.setProperty('--composer-card-height', `${Math.ceil(composer.getBoundingClientRect().height)}px`);
    }
  };

  useLayoutEffect(() => {
    scrollRef.current?.style.setProperty('--progress-height', '0px');
  }, [activeMessageScope, activitySignature, progressVisible]);

  useLayoutEffect(() => {
    const node = composerRef.current;
    if (!node) return undefined;
    updateComposerMetrics();
    const observer = new ResizeObserver(updateComposerMetrics);
    observer.observe(node);
    const composer = node.querySelector('.composer');
    if (composer) observer.observe(composer);
    return () => observer.disconnect();
  }, []);

  useLayoutEffect(() => {
    const textarea = textareaRef.current;
    if (!textarea) return;
    const maxHeight = Number.parseFloat(window.getComputedStyle(textarea).maxHeight) || 220;
    textarea.style.height = 'auto';
    const nextHeight = Math.min(textarea.scrollHeight, maxHeight);
    textarea.style.height = `${nextHeight}px`;
    textarea.style.overflowY = textarea.scrollHeight > maxHeight ? 'auto' : 'hidden';
    updateComposerMetrics();
    if (stickToBottomRef.current) {
      requestAnimationFrame(scrollToBottom);
    }
  }, [draft, composerAttachments.length, selectionReferences.length]);

  useEffect(() => {
    if (modelSelectionPending) {
      openModelOptions();
    }
  }, [modelSelectionPending, activeMessageScope]);

  const scrollToBottom = () => {
    const list = scrollRef.current;
    if (!list) return;
    list.scrollTop = list.scrollHeight;
  };

  useLayoutEffect(() => {
    const list = scrollRef.current;
    if (!list) return;
    if (prependAdjustRef.current) {
      const { previousScrollHeight, previousScrollTop } = prependAdjustRef.current;
      prependAdjustRef.current = null;
      list.scrollTop = previousScrollTop + (list.scrollHeight - previousScrollHeight);
    } else if ((previousCountRef.current === 0 && renderedMessages.length > 0) || stickToBottomRef.current) {
      scrollToBottom();
      requestAnimationFrame(() => {
        scrollToBottom();
        requestAnimationFrame(scrollToBottom);
      });
    }
    previousCountRef.current = renderedMessages.length;
  }, [activeMessageScope, renderedMessages.length, messages.length, newestMessageKey, messagesReady, activitySignature]);

  useEffect(() => {
    previousCountRef.current = 0;
    loadingOlderRef.current = false;
    prependAdjustRef.current = null;
    stickToBottomRef.current = true;
    requestAnimationFrame(scrollToBottom);
    setDraft('');
    setComposerAttachments((current) => {
      current.forEach((attachment) => {
        if (attachment.previewUrl) URL.revokeObjectURL(attachment.previewUrl);
      });
      return [];
    });
  }, [activeMessageScope]);

  useEffect(() => {
    composerAttachmentsRef.current = composerAttachments;
  }, [composerAttachments]);

  useEffect(() => () => {
    composerAttachmentsRef.current.forEach((attachment) => {
      if (attachment.previewUrl) URL.revokeObjectURL(attachment.previewUrl);
    });
  }, []);

  const loadOlderPreservingViewport = async () => {
    const list = scrollRef.current;
    if (!list || !hasOlder || loadingOlderRef.current) return;
    loadingOlderRef.current = true;
    const previousScrollHeight = list.scrollHeight;
    const previousScrollTop = list.scrollTop;
    try {
      const changed = await onLoadOlder?.();
      if (changed) {
        prependAdjustRef.current = { previousScrollHeight, previousScrollTop };
      }
    } catch {
      // Lazy loading is opportunistic; keep the current viewport if the server refuses a page.
    } finally {
      loadingOlderRef.current = false;
    }
  };

  useEffect(() => {
    const list = scrollRef.current;
    if (!list || !hasOlder || loadingOlderRef.current) return;
    const children = Array.from(list.children).filter((child) => !child.classList.contains('empty-chat'));
    const firstContent = children[0];
    const lastContent = children.at(-1);
    const listRect = list.getBoundingClientRect();
    const composerTop = list.closest('.chat-workspace')?.querySelector('.composer-wrap')?.getBoundingClientRect().top ?? listRect.bottom;
    const firstContentTop = firstContent?.getBoundingClientRect().top ?? listRect.top;
    const lastContentBottom = lastContent?.getBoundingClientRect().bottom ?? listRect.top;
    const topLooksUnderfilled = firstContentTop > listRect.top + 24;
    const bottomLooksUnderfilled = lastContentBottom < composerTop - 120;
    if (
      list.scrollTop <= 96 ||
      list.scrollHeight <= list.clientHeight + 96 ||
      topLooksUnderfilled ||
      bottomLooksUnderfilled
    ) {
      loadOlderPreservingViewport();
    }
  }, [hasOlder, messages.length, oldestMessageKey, runningActivities?.length]);

  const handleScroll = () => {
    const list = scrollRef.current;
    if (!list) return;
    stickToBottomRef.current = list.scrollHeight - list.scrollTop - list.clientHeight < 80;
    if (list.scrollTop <= 96) {
      loadOlderPreservingViewport();
    }
  };

  const addComposerFiles = async (files, source = 'file') => {
    const values = Array.from(files || []).filter(Boolean);
    if (!values.length) return;
    const baseTime = Date.now();
    const next = await Promise.all(values.map((file, index) => {
      const fallbackName = source === 'paste' && isImageFileObject(file)
        ? `pasted-image-${baseTime + index}.png`
        : '';
      return composerAttachmentFromFile(file, fallbackName);
    }));
    setComposerAttachments((current) => [...current, ...next]);
    requestAnimationFrame(() => textareaRef.current?.focus());
  };

  const removeComposerAttachment = (id) => {
    setComposerAttachments((current) => {
      const removed = current.find((attachment) => attachment.id === id);
      if (removed?.previewUrl) URL.revokeObjectURL(removed.previewUrl);
      return current.filter((attachment) => attachment.id !== id);
    });
  };

  const submitDraft = async () => {
    if ((!draft.trim() && composerAttachments.length === 0 && selectionReferences.length === 0) || sending) return;
    const value = draft;
    const attachments = composerAttachments;
    const selections = selectionReferences;
    setDraft('');
    setComposerAttachments([]);
    const sent = await onSend?.(value, attachments.map(outgoingAttachmentPayload), selections);
    if (sent === false) {
      setDraft((current) => current || value);
      setComposerAttachments((current) => current.length ? current : attachments);
    } else {
      attachments.forEach((attachment) => {
        if (attachment.previewUrl) URL.revokeObjectURL(attachment.previewUrl);
      });
    }
  };

  const handlePaste = (event) => {
    const items = Array.from(event.clipboardData?.items || []);
    const files = items
      .filter((item) => item.kind === 'file')
      .map((item) => item.getAsFile())
      .filter(Boolean);
    if (files.length > 0) {
      event.preventDefault();
      addComposerFiles(files, 'paste').catch(() => {});
    }
  };

  const isImeComposingEnter = (event) => {
    if (event.key !== 'Enter') return false;
    const nativeEvent = event.nativeEvent || {};
    return composingRef.current
      || event.isComposing
      || nativeEvent.isComposing
      || nativeEvent.keyCode === 229;
  };

  const openModelOptions = async () => {
    setCommandPanel('models');
    setModelsError('');
    setModelsLoading(true);
    try {
      const nextModels = await onLoadModels?.();
      setModels(Array.isArray(nextModels) ? nextModels : []);
    } catch (error) {
      setModels([]);
      setModelsError(error?.message || '无法读取模型列表');
    } finally {
      setModelsLoading(false);
    }
  };

  const chooseCommand = (command) => {
    if (command.options === 'models') {
      openModelOptions();
      return;
    }
    if (command.options === 'reasoning') {
      setCommandPanel('reasoning');
      return;
    }
    if (command.send) {
      onSend?.(command.command);
      return;
    }
    setDraft(command.insert || command.command);
  };

  const chooseModel = (model) => {
    const alias = modelAlias(model);
    if (!alias) return;
    onSend?.(`/model ${alias}`);
  };

  const chooseReasoning = (effort) => {
    onSend?.(`/reasoning ${effort}`);
  };

  return (
    <section className="chat-workspace">
      <div className="message-scroll" ref={scrollRef} onScroll={handleScroll}>
        {modelSelectionPending ? (
          <ModelSelectionGate
            models={models}
            loading={modelsLoading}
            error={modelsError}
            onReload={openModelOptions}
            onChoose={chooseModel}
          />
        ) : renderedMessages.length === 0 ? (
          <div className="empty-chat">
            <strong>欢迎使用 Stellacode</strong>
            <span>选择一个 Conversation，或者新建对话，让 Stellacode 帮你检查项目、修改代码、运行命令和整理上下文。</span>
          </div>
        ) : (
          <>
            {renderedMessages.map((message, index) => (
              message.type === 'toolGroup'
                ? <MemoToolProcessGroup key={message.id} group={message} active={sessionRunning && index === latestToolGroupIndex} />
                : (
                  <MemoMessageArticle
                    key={messageKey(message, index)}
                    message={message}
                    onOpenAttachment={onOpenAttachment}
                    onDownloadAttachment={onDownloadAttachment}
                  />
                )
            ))}
            {inlineActivity && <InlineActivityStatus activity={inlineActivity} />}
            {turnStoppedAfterTool && (
              <div className="turn-continuation-notice">
                <span>本轮停在工具结果后，没有后续 assistant 消息。</span>
                <button type="button" onClick={() => onSend?.('/continue')} disabled={sending}>
                  继续
                </button>
              </div>
            )}
          </>
        )}
      </div>
      <LiveActivityStack activities={runningActivities} progressRef={progressRef} />
      <footer className="composer-wrap" ref={composerRef}>
        <div className="composer">
          {composerAttachments.length > 0 && (
            <div className="composer-attachments" aria-label="待发送附件">
              {composerAttachments.map((attachment) => (
                <button
                  className="composer-attachment-chip"
                  type="button"
                  key={attachment.id}
                  title={`移除 ${attachment.name}`}
                  onClick={() => removeComposerAttachment(attachment.id)}
                >
                  {attachment.previewUrl ? (
                    <img src={attachment.previewUrl} alt="" />
                  ) : (
                    <span className="composer-attachment-icon"><FileText size={13} /></span>
                  )}
                  <span>{attachment.name}</span>
                  <small>×</small>
                </button>
              ))}
            </div>
          )}
          {selectionReferences.length > 0 && (
            <div className="composer-selection-references" aria-label="待发送选区引用">
              {selectionReferences.map((selection, index) => (
                <button
                  className="composer-selection-chip"
                  type="button"
                  key={selection.id || `${selection.file_path}-${index}`}
                  title={`移除 ${selection.file_path}`}
                  onClick={() => onRemoveSelectionReference?.(selection.id)}
                >
                  <FileText size={13} />
                  <span>{selection.file_name || selection.file_path}</span>
                  <small>{selectionSummary(selection)}</small>
                  <em>×</em>
                </button>
              ))}
            </div>
          )}
          <input
            ref={fileInputRef}
            className="composer-file-input"
            type="file"
            multiple
            onChange={(event) => {
              const files = Array.from(event.currentTarget.files || []);
              event.currentTarget.value = '';
              addComposerFiles(files, 'file').catch(() => {});
            }}
          />
          <textarea
            ref={textareaRef}
            value={draft}
            onChange={(event) => setDraft(event.target.value)}
            onPaste={handlePaste}
            disabled={modelSelectionPending}
            onCompositionStart={() => {
              composingRef.current = true;
            }}
            onCompositionEnd={() => {
              composingRef.current = false;
              if (lastComposingEnterAtRef.current > lastEnterKeyUpAtRef.current) {
                suppressNextEnterRef.current = true;
                window.setTimeout(() => {
                  suppressNextEnterRef.current = false;
                }, 160);
              }
            }}
            onKeyDown={(event) => {
              if (event.key === 'Enter' && !event.shiftKey && isImeComposingEnter(event)) {
                lastComposingEnterAtRef.current = Date.now();
                return;
              }
              if (event.key === 'Enter' && !event.shiftKey && suppressNextEnterRef.current) {
                suppressNextEnterRef.current = false;
                event.preventDefault();
                return;
              }
              if (event.key === 'Enter' && !event.shiftKey) {
                event.preventDefault();
                submitDraft().catch(() => {});
              }
            }}
            onKeyUp={(event) => {
              if (event.key === 'Enter') {
                lastEnterKeyUpAtRef.current = Date.now();
                suppressNextEnterRef.current = false;
              }
            }}
            placeholder={modelSelectionPending ? '请先选择模型' : 'Ask Stellacode to change, inspect, or explain...'}
          />
          <div className="composer-row">
            <button className="composer-icon" type="button" title="添加附件" onClick={() => fileInputRef.current?.click()}>
              <Plus size={18} />
            </button>
            <Popover.Root onOpenChange={(open) => {
              if (open) {
                setCommandPanel('commands');
                setModelsError('');
              }
            }}>
              <Popover.Trigger asChild>
                <button className="composer-icon command-trigger" type="button" title="可用命令">
                  <TerminalSquare size={17} />
                </button>
              </Popover.Trigger>
              <Popover.Portal>
                <Popover.Content className="floating-popover command-popover" side="top" align="start" sideOffset={10}>
                  {commandPanel === 'models' ? (
                    <div className="command-panel">
                      <div className="command-popover-head">
                        <button className="command-back" type="button" onClick={() => setCommandPanel('commands')}>‹</button>
                        <strong>选择模型</strong>
                      </div>
                      {modelsLoading ? (
                        <div className="command-empty">正在加载模型...</div>
                      ) : modelsError ? (
                        <div className="command-empty error">{modelsError}</div>
                      ) : models.length === 0 ? (
                        <div className="command-empty">没有可用模型</div>
                      ) : (
                        <div className="command-list">
                          {models.map((model) => (
                            <Popover.Close asChild key={modelAlias(model)}>
                              <button className="command-row model-row" type="button" onClick={() => chooseModel(model)}>
                                <span>
                                  <strong>{modelAlias(model)}</strong>
                                  <small>{modelDisplayName(model)}</small>
                                </span>
                              </button>
                            </Popover.Close>
                          ))}
                        </div>
                      )}
                    </div>
                  ) : commandPanel === 'reasoning' ? (
                    <div className="command-panel">
                      <div className="command-popover-head">
                        <button className="command-back" type="button" onClick={() => setCommandPanel('commands')}>‹</button>
                        <strong>选择推理强度</strong>
                      </div>
                      <div className="command-list">
                        {REASONING_EFFORTS.map((effort) => (
                          <Popover.Close asChild key={effort.value}>
                            <button className="command-row model-row" type="button" onClick={() => chooseReasoning(effort.value)}>
                              <span>
                                <strong>{effort.label}</strong>
                                <small>{effort.description}</small>
                              </span>
                            </button>
                          </Popover.Close>
                        ))}
                      </div>
                    </div>
                  ) : (
                    <div className="command-panel">
                      <div className="command-list">
                        {COMMANDS.map((command) => (
                          command.options === 'models' || command.options === 'reasoning' ? (
                            <button key={command.command} className="command-row" type="button" onClick={() => chooseCommand(command)}>
                              <code>{command.command}</code>
                              <span>
                                <strong>{command.label}</strong>
                                <small>{command.description}</small>
                              </span>
                              <em>选择</em>
                            </button>
                          ) : (
                            <Popover.Close asChild key={command.command}>
                              <button className="command-row" type="button" onClick={() => chooseCommand(command)}>
                                <code>{command.command}</code>
                                <span>
                                  <strong>{command.label}</strong>
                                  <small>{command.description}</small>
                                </span>
                              </button>
                            </Popover.Close>
                          )
                        ))}
                      </div>
                    </div>
                  )}
                  <Popover.Arrow className="floating-popover-arrow" />
                </Popover.Content>
              </Popover.Portal>
            </Popover.Root>
            <span className={`mode-pill ${modeTone}`} title={modeTitle}>{modeLabel}</span>
            <button className="send-button" type="button" disabled={modelSelectionPending || (!draft.trim() && composerAttachments.length === 0 && selectionReferences.length === 0) || sending} onClick={() => submitDraft().catch(() => {})}>
              <Send size={18} />
            </button>
          </div>
        </div>
      </footer>
    </section>
  );
}

function ModelSelectionGate({ models, loading, error, onReload, onChoose }) {
  return (
    <section className="model-gate">
      <div className="model-gate-card">
        <strong>选择模型后开始对话</strong>
        <span>这个 Conversation 还没有初始化模型。请选择一个可用模型，Stellacode 会发送 `/model` 完成初始化。</span>
        {loading ? (
          <div className="command-empty">正在加载模型...</div>
        ) : error ? (
          <div className="command-empty error">{error}</div>
        ) : models.length === 0 ? (
          <div className="command-empty">没有可用模型</div>
        ) : (
          <div className="model-gate-list">
            {models.map((model) => (
              <button key={modelAlias(model)} className="model-gate-row" type="button" onClick={() => onChoose(model)}>
                <strong>{modelAlias(model)}</strong>
                <span>{modelDisplayName(model)}</span>
              </button>
            ))}
          </div>
        )}
        <button className="secondary-button" type="button" onClick={onReload}>刷新模型</button>
      </div>
    </section>
  );
}

export function LiveActivityStack({ activities, progressRef }) {
  if (!activities?.length) return null;
  const current = activities.at(-1) || {};
  const plan = normalizeActivityPlan(current.plan);
  if (!plan) return null;
  return (
    <section className="session-progress-card with-plan" aria-live="polite" ref={progressRef}>
      <ActivityPlanPanel plan={plan} />
    </section>
  );
}

function InlineActivityStatus({ activity }) {
  const state = String(activity?.state || 'running').toLowerCase();
  const title = String(activity?.title || '').trim();
  const detail = String(activity?.detail || activity?.activity || activity?.model || '').trim();
  const label = title || (state === 'failed' ? '执行失败' : state === 'done' ? '已完成' : '正在思考');
  return (
    <div className={`chat-activity-status ${state}`}>
      <i className="chat-activity-icon" aria-hidden="true" />
      <span>{label}</span>
      {detail && <code>{detail}</code>}
    </div>
  );
}

function ActivityStatus({ activity }) {
  const state = String(activity?.state || 'running').toLowerCase();
  const title = String(activity?.title || (state === 'failed' ? '执行失败' : state === 'done' ? '已完成' : '处理中')).trim();
  const detail = String(activity?.detail || activity?.activity || activity?.model || '').trim();
  return (
    <div className={`session-progress-head ${state}`}>
      <i className="session-progress-dot" aria-hidden="true" />
      <span>{title}</span>
      {detail && <code>{detail}</code>}
    </div>
  );
}

function normalizeActivityPlan(rawPlan) {
  const items = Array.isArray(rawPlan)
    ? rawPlan
    : Array.isArray(rawPlan?.items)
      ? rawPlan.items
      : Array.isArray(rawPlan?.plan)
        ? rawPlan.plan
        : [];
  if (!items.length && !rawPlan?.explanation) return null;
  const normalized = items
    .map(normalizePlanItem)
    .filter((item) => item.step);
  const completed = normalized.filter((item) => item.status === 'completed' || item.status === 'done').length;
  return {
    explanation: String(rawPlan?.explanation || '').trim(),
    items: normalized,
    completed,
    total: normalized.length
  };
}

function normalizePlanItem(item) {
  const rawStep = String(item?.step || item?.text || item?.title || '').trim();
  const marker = rawStep.match(/^\[(x|~|\s*)]\s*/i)?.[1]?.toLowerCase();
  const markerStatus = marker === 'x'
    ? 'completed'
    : marker === '~'
      ? 'in_progress'
      : marker !== undefined
        ? 'pending'
        : '';
  const rawStatus = String(item?.status || item?.state || '').toLowerCase();
  const normalizedStatus = normalizePlanStatus(rawStatus);
  const status = markerStatus && (!normalizedStatus || normalizedStatus === 'pending')
    ? markerStatus
    : normalizedStatus || markerStatus || 'pending';
  return {
    step: rawStep.replace(/^\[(?:x|~|\s*)]\s*/i, '').trim(),
    status
  };
}

function normalizePlanStatus(status) {
  if (status === 'completed' || status === 'done' || status === 'success') return 'completed';
  if (status === 'in_progress' || status === 'running' || status === 'active') return 'in_progress';
  if (status === 'pending' || status === 'todo') return 'pending';
  return '';
}

function ActivityPlanPanel({ plan }) {
  return (
    <section className="activity-plan-panel">
      <div className="activity-plan-head">
        <span>进度</span>
        <code>共 {plan.total} 项，已完成 {plan.completed} 项</code>
        <Pin className="activity-plan-pin" size={15} aria-hidden="true" />
      </div>
      <div className="activity-plan-body">
        {plan.explanation && <p>{plan.explanation}</p>}
        {plan.items.map((item, index) => (
          <div className={`activity-plan-row ${planStatusClass(item.status)}`} key={`${item.step}-${index}`}>
            <span>{planStatusMark(item.status)}</span>
            <strong>{index + 1}.</strong>
            <em>{item.step}</em>
          </div>
        ))}
      </div>
    </section>
  );
}

function planStatusClass(status) {
  if (status === 'completed' || status === 'done') return 'completed';
  if (status === 'in_progress' || status === 'running') return 'running';
  return 'pending';
}

function planStatusMark(status) {
  if (status === 'completed' || status === 'done') return '✓';
  if (status === 'in_progress' || status === 'running') return '◌';
  return '○';
}

export function MessageArticle({ message, onOpenAttachment, onDownloadAttachment }) {
  const usage = tokenUsage(message);
  const role = message.user_name || message.role || 'assistant';
  const className = messageArticleClassName(message);
  const roleName = String(message.role || '').toLowerCase();
  const auxiliaryMessages = Array.isArray(message._auxiliary) ? message._auxiliary : [];
  return (
    <article className={className}>
      {auxiliaryMessages.length > 0 && (
        <div className="message-role">
          <span>{role}</span>
          <AuxiliaryDots messages={auxiliaryMessages} />
        </div>
      )}
      <MessageBody message={message} onOpenAttachment={onOpenAttachment} onDownloadAttachment={onDownloadAttachment} />
      {(roleName === 'user' || (roleName === 'assistant' && !message._streaming)) && (
        <MessageActionBar message={message} role={roleName} usage={usage} />
      )}
      {message.pending && <div className="message-status">正在发送...</div>}
      {message.error && <div className="message-status error">{message.error}</div>}
    </article>
  );
}

function MessageActionBar({ message, role, usage }) {
  const [copied, setCopied] = useState(false);
  const text = messageText(message).trim();
  const replyTime = role === 'assistant' ? formatMessageTime(message?.message_time || message?.time || message?.created_at) : '';
  const showUsage = role === 'assistant' && Number(usage?.total || 0) > 0;
  if (!text && !replyTime && !showUsage) return null;
  const copyMessage = async () => {
    if (!text) return;
    try {
      await navigator.clipboard?.writeText(text);
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1200);
    } catch {
      setCopied(false);
    }
  };
  return (
    <div className={`message-actions ${role}`} aria-label="消息操作">
      {text && (
        <button className="message-action-button" type="button" onClick={copyMessage} title={copied ? '已复制' : '复制消息'} aria-label={copied ? '已复制' : '复制消息'}>
          <Copy size={15} strokeWidth={1.8} aria-hidden="true" />
        </button>
      )}
      {replyTime && <span className="message-reply-time">{replyTime}</span>}
      {showUsage && <TokenUsage usage={usage} />}
    </div>
  );
}

function formatMessageTime(value) {
  const date = parseMessageDate(value);
  if (!Number.isFinite(date.getTime())) return '';
  const now = new Date();
  const dateParts = systemDateParts(date);
  const nowParts = systemDateParts(now);
  const clock = systemClock(date);
  if (dateParts.year === nowParts.year && dateParts.month === nowParts.month && dateParts.day === nowParts.day) return clock;
  return `${dateParts.month}/${dateParts.day} ${clock}`;
}

function parseMessageDate(value) {
  if (!value) return new Date(Number.NaN);
  const raw = String(value).trim();
  if (!raw) return new Date(Number.NaN);
  const hasExplicitZone = /(?:z|[+-]\d{2}:?\d{2})$/i.test(raw);
  const looksLikeDateTime = /^\d{4}-\d{2}-\d{2}[T\s]\d{2}:\d{2}/.test(raw);
  const normalized = looksLikeDateTime && !hasExplicitZone ? `${raw.replace(' ', 'T')}Z` : raw;
  return new Date(normalized);
}

function systemDateParts(date) {
  const parts = new Intl.DateTimeFormat(undefined, {
    timeZone: systemTimeZone(),
    year: 'numeric',
    month: 'numeric',
    day: 'numeric'
  }).formatToParts(date);
  return {
    year: Number(parts.find((part) => part.type === 'year')?.value || 0),
    month: Number(parts.find((part) => part.type === 'month')?.value || 0),
    day: Number(parts.find((part) => part.type === 'day')?.value || 0)
  };
}

function systemClock(date) {
  return new Intl.DateTimeFormat(undefined, {
    timeZone: systemTimeZone(),
    hour: 'numeric',
    minute: '2-digit',
    hourCycle: 'h23'
  }).format(date);
}

function systemTimeZone() {
  return Intl.DateTimeFormat().resolvedOptions().timeZone;
}

function messageArticleClassName(message) {
  const classes = ['message', message.role || 'assistant'];
  if (message._forceSeparate) classes.push('force-separate');
  if (message._streaming) classes.push('streaming');
  if (message._streamFailed) classes.push('stream-failed');
  const itemList = Array.isArray(message?.items) ? message.items : [];
  if (itemList.some((item) => item?.type === 'selection_reference')) {
    classes.push('has-selection-reference');
  }
  if (String(message.role || '').toLowerCase() === 'user') {
    const text = messageText(message).trim();
    const attachments = [
      ...(Array.isArray(message?.attachments) ? message.attachments : []),
      ...(Array.isArray(message?.files) ? message.files : [])
    ];
    const itemAttachments = itemList.filter((item) => item?.type === 'file');
    if (text && (attachments.length > 0 || itemAttachments.length > 0 || Number(message?.attachment_count || 0) > 0)) {
      classes.push('media-combo');
    }
  }
  return classes.join(' ');
}

const MemoMessageArticle = memo(MessageArticle, (previous, next) => {
  if (previous.message !== next.message) return false;
  if (previous.onOpenAttachment !== next.onOpenAttachment || previous.onDownloadAttachment !== next.onDownloadAttachment) return false;
  return true;
});

export function AuxiliaryDots({ messages }) {
  return (
    <div className="aux-dots">
      {messages.map((message, index) => (
        <Popover.Root key={messageKey(message, index)}>
          <Popover.Trigger asChild>
            <button
              className={`aux-dot aux-dot-${index % 4}`}
              type="button"
              aria-label="查看辅助消息"
            />
          </Popover.Trigger>
          <Popover.Portal>
            <Popover.Content className="floating-popover aux-popover" side="bottom" align="start" sideOffset={8}>
              <pre>{messageText(message)}</pre>
              <Popover.Arrow className="floating-popover-arrow" />
            </Popover.Content>
          </Popover.Portal>
        </Popover.Root>
      ))}
    </div>
  );
}

export function TokenUsage({ usage }) {
  if (!Number(usage?.total || 0)) return null;
  const cacheReadDominant = usage.cacheRead > 0
    && usage.cacheRead >= usage.input
    && usage.cacheRead >= usage.output
    && usage.cacheRead >= usage.cacheWrite;
  return (
    <Popover.Root>
      <Popover.Trigger asChild>
        <button className={`token-usage${cacheReadDominant ? ' cache-read-dominant' : ''}`} type="button" aria-label="查看 Token Usage">
          <span className="token-dot" aria-hidden="true" />
          <span>{formatTokens(usage.total)}</span>
        </button>
      </Popover.Trigger>
      <Popover.Portal>
        <Popover.Content className="floating-popover token-popover" side="top" align="end" sideOffset={8}>
          <div><span>Input</span><strong>{usage.input}</strong></div>
          <div><span>Output</span><strong>{usage.output}</strong></div>
          <div><span>Cache Read</span><strong>{usage.cacheRead}</strong></div>
          <div><span>Cache Write</span><strong>{usage.cacheWrite}</strong></div>
          <div><span>Total</span><strong>{usage.total}</strong></div>
          <Popover.Arrow className="floating-popover-arrow" />
        </Popover.Content>
      </Popover.Portal>
    </Popover.Root>
  );
}

export function InlineTokenUsage({ usage }) {
  if (!Number(usage?.total || 0)) return null;
  return (
    <Popover.Root>
      <span className="tool-token-usage" onClick={(event) => event.stopPropagation()}>
        <Popover.Trigger asChild>
          <button className="token-dot" type="button" aria-label="查看 Token Usage" />
        </Popover.Trigger>
        <span>{formatTokens(usage.total)}</span>
      </span>
      <Popover.Portal>
        <Popover.Content className="floating-popover token-popover" side="top" align="end" sideOffset={8}>
          <div><span>Input</span><strong>{usage.input}</strong></div>
          <div><span>Output</span><strong>{usage.output}</strong></div>
          <div><span>Cache Read</span><strong>{usage.cacheRead}</strong></div>
          <div><span>Cache Write</span><strong>{usage.cacheWrite}</strong></div>
          <div><span>Total</span><strong>{usage.total}</strong></div>
          <Popover.Arrow className="floating-popover-arrow" />
        </Popover.Content>
      </Popover.Portal>
    </Popover.Root>
  );
}

export function ToolProcessGroup({ group, active = false }) {
  const messages = group.messages || [];
  const expandedRows = useMemo(() => messages.map((message, index) => {
    const { textMessage, toolCards, segments } = splitMessageForDisplay(message);
    return {
      id: messageKey(message, index),
      textMessage,
      segments,
      toolCards,
      usage: tokenUsage(message)
    };
  }), [messages]);
  const blocks = useMemo(() => toolProcessBlocks(expandedRows), [expandedRows]);
  const activeTail = active && !group.nextMessage;
  const lastToolBlockIndex = useMemo(() => {
    for (let index = blocks.length - 1; index >= 0; index -= 1) {
      if (blocks[index]?.type === 'tools') return index;
    }
    return -1;
  }, [blocks]);
  const hasFinalMessage = isFinalAssistantMessage(group.nextMessage);
  const toolsComplete = useMemo(() => {
    const toolBlocks = blocks.filter((block) => block.type === 'tools');
    return toolBlocks.length > 0 && toolBlocks.every((block) => toolCardsAreComplete(block.cards));
  }, [blocks]);
  const complete = !activeTail && (hasFinalMessage || toolsComplete);
  const [open, setOpen] = useState(() => !hasFinalMessage);
  const wasActiveTailRef = useRef(activeTail);
  const hadFinalMessageRef = useRef(hasFinalMessage);
  useEffect(() => {
    if (activeTail && !wasActiveTailRef.current) {
      setOpen(true);
    }
    wasActiveTailRef.current = activeTail;
  }, [activeTail]);
  useEffect(() => {
    if (!hadFinalMessageRef.current && hasFinalMessage) {
      setOpen(false);
    }
    hadFinalMessageRef.current = hasFinalMessage;
  }, [hasFinalMessage]);
  const elapsed = useToolRoundElapsed(messages, group.nextMessage, complete);
  const title = toolRoundTitle(elapsed, complete);
  return (
    <section className={`tool-process-group${open ? ' open' : ''}`}>
      <button className="tool-round-toggle" type="button" onClick={() => setOpen((value) => !value)}>
        <span>{title}</span>
        <ChevronDown size={15} strokeWidth={1.9} aria-hidden="true" />
      </button>
      <div className="tool-round-separator" aria-hidden="true" />
      {open && (
        <div className="tool-process-round-body">
          {blocks.map((block, index) => {
            if (block.type === 'tools') {
              const cardsComplete = toolCardsAreComplete(block.cards);
              return (
                <ToolProcessSegment
                  key={block.id}
                  block={block}
                  complete={complete || cardsComplete}
                  running={!complete && index === lastToolBlockIndex && (activeTail || !cardsComplete)}
                />
              );
            }
            return block.kind === 'reasoning'
              ? <ReasoningNote key={block.id} text={block.text} />
              : <MarkdownContent key={block.id} className="tool-note" text={block.text} attachments={block.attachments} />;
          })}
        </div>
      )}
    </section>
  );
}

const MemoToolProcessGroup = memo(ToolProcessGroup);

function useToolRoundElapsed(messages, nextMessage, complete) {
  const startMsRef = useRef(toolRoundStartMs(messages));
  const finalElapsedMsRef = useRef(null);
  const wasLiveRef = useRef(!complete);
  const [tickMs, setTickMs] = useState(() => Date.now());

  if (!complete) {
    wasLiveRef.current = true;
  } else if (wasLiveRef.current && finalElapsedMsRef.current === null) {
    finalElapsedMsRef.current = Math.max(0, Date.now() - startMsRef.current);
  }

  useEffect(() => {
    if (complete) return undefined;
    const timer = window.setInterval(() => {
      setTickMs(Date.now());
    }, 1000);
    return () => window.clearInterval(timer);
  }, [complete]);

  if (complete) {
    const elapsedMs = finalElapsedMsRef.current ?? toolRoundElapsedMs(messages, nextMessage);
    return elapsedMs !== null ? formatElapsedMs(elapsedMs) : '';
  }
  return formatElapsedMs(Math.max(0, tickMs - startMsRef.current));
}

function toolRoundStartMs(messages) {
  const times = (messages || [])
    .map(messageTimeMs)
    .filter((value) => Number.isFinite(value));
  return times.length > 0 ? Math.min(...times) : Date.now();
}

function toolRoundTitle(elapsed, complete) {
  if (!complete) return elapsed ? `处理中 ${elapsed}` : '处理中';
  return elapsed ? `已处理 ${elapsed}` : '已处理';
}

function toolRoundElapsedMs(messages, nextMessage) {
  const times = [...(messages || []), nextMessage]
    .map(messageTimeMs)
    .filter((value) => Number.isFinite(value));
  if (times.length < 2) return null;
  return Math.max(0, Math.max(...times) - Math.min(...times));
}

function messageTimeMs(message) {
  const value = message?.message_time || message?.created_at || message?.time || '';
  if (!value) return null;
  const parsed = Date.parse(value);
  return Number.isFinite(parsed) ? parsed : null;
}

function formatElapsedMs(ms) {
  const totalSeconds = Math.max(0, Math.round(ms / 1000));
  if (totalSeconds < 60) return `${totalSeconds}s`;
  const minutes = Math.floor(totalSeconds / 60);
  const seconds = totalSeconds % 60;
  if (minutes < 60) return seconds > 0 ? `${minutes}m ${seconds}s` : `${minutes}m`;
  const hours = Math.floor(minutes / 60);
  const remainingMinutes = minutes % 60;
  return remainingMinutes > 0 ? `${hours}h ${remainingMinutes}m` : `${hours}h`;
}

function toolProcessBlocks(rows) {
  const blocks = [];
  let pendingCards = [];
  let pendingId = '';
  const flushTools = () => {
    if (!pendingCards.length) return;
    blocks.push({
      type: 'tools',
      id: pendingId || `tools-${blocks.length}`,
      cards: pendingCards
    });
    pendingCards = [];
    pendingId = '';
  };
  const pushNote = (note) => {
    flushTools();
    blocks.push(note);
  };
  rows.forEach((row) => {
    const attachments = row.textMessage ? [...(row.textMessage.attachments || []), ...(row.textMessage.files || [])] : [];
    const text = row.textMessage ? messageText(row.textMessage) : '';
    if (text) {
      pushNote({
        type: 'note',
        kind: 'text',
        id: `${row.id}-text`,
        text,
        attachments
      });
    }
    let renderedCardIndex = 0;
    (row.segments || [{ notes: [], cards: row.toolCards }]).forEach((segment, segmentIndex) => {
      (segment.notes || []).forEach((note, noteIndex) => {
        pushNote({
          type: 'note',
          kind: note.kind === 'reasoning' ? 'reasoning' : 'text',
          id: `${row.id}-${segmentIndex}-note-${noteIndex}`,
          text: note.text,
          attachments
        });
      });
      if (!segment.cards?.length) return;
      const cards = segment.cards.map((card, cardIndex) => {
        const rowUsage = renderedCardIndex === row.toolCards.length - 1 ? row.usage : null;
        renderedCardIndex += 1;
        return {
          ...card,
          renderId: `${row.id}-${segmentIndex}-card-${cardIndex}`,
          sourceRowId: row.id,
          sourceRowUsage: rowUsage
        };
      });
      if (!pendingId) pendingId = `${row.id}-${segmentIndex}-tools`;
      pendingCards = pendingCards.concat(cards);
    });
  });
  flushTools();
  return blocks;
}

function ToolProcessSegment({ block, complete, running = false }) {
  const [open, setOpen] = useState(false);
  const toolRows = useMemo(() => mergedToolCards(block.cards), [block.cards]);
  const firstName = useMemo(() => block.cards[0]?.name || 'tool', [block.cards]);
  const summary = useMemo(() => toolGroupSummary(block.cards, firstName), [block.cards, firstName]);
  const title = complete ? summary.doneTitle : summary.runningTitle;
  return (
    <section className={`tool-process-segment${open ? ' open' : ''}${running ? ' running' : ''}`}>
      <button className="tool-process-toggle" type="button" onClick={() => setOpen((value) => !value)}>
        <TerminalSquare size={15} strokeWidth={1.9} aria-hidden="true" />
        <span>{title}</span>
        <ChevronDown size={15} strokeWidth={1.9} aria-hidden="true" />
      </button>
      {open && (
        <div className="tool-process-body">
          {toolRows.map((card) => (
            <ToolInlineCard
              key={card.renderId}
              kind={card.kind}
              name={card.name}
              payload={card.payload}
              callPayload={card.callPayload}
              resultPayload={card.resultPayload}
              usage={card.usage}
              running={card.running}
            />
          ))}
        </div>
      )}
    </section>
  );
}

function mergedToolCards(cards) {
  const rows = [];
  const byId = new Map();
  cards.forEach((card, index) => {
    const id = String(card.id || '').trim();
    if (!id) {
      rows.push({ order: index, call: card.kind === 'call' ? card : null, result: card.kind === 'result' ? card : null, sourceRowIds: new Set([card.sourceRowId].filter(Boolean)) });
      return;
    }
    let row = byId.get(id);
    if (!row) {
      row = { order: index, call: null, result: null, sourceRowIds: new Set() };
      byId.set(id, row);
      rows.push(row);
    }
    if (card.sourceRowId) row.sourceRowIds.add(card.sourceRowId);
    if (card.kind === 'result') {
      row.result = card;
    } else {
      row.call = card;
    }
  });
  const orderedRows = rows.sort((left, right) => left.order - right.order);
  const usageBySourceRow = new Map();
  cards.forEach((card) => {
    if (card.sourceRowId && Number(card.sourceRowUsage?.total || 0)) {
      usageBySourceRow.set(card.sourceRowId, card.sourceRowUsage);
    }
  });
  const lastMergedRowBySource = new Map();
  orderedRows.forEach((row) => {
    row.sourceRowIds.forEach((sourceRowId) => {
      lastMergedRowBySource.set(sourceRowId, row);
    });
  });
  const usageByMergedRow = new Map();
  usageBySourceRow.forEach((usage, sourceRowId) => {
    const row = lastMergedRowBySource.get(sourceRowId);
    if (row) usageByMergedRow.set(row, addToolUsage(usageByMergedRow.get(row), usage));
  });
  return orderedRows.map((row, index) => {
    const call = row.call;
    const result = row.result;
    const displayCard = call || result;
    const detailCard = result || call;
    return {
      renderId: call?.renderId || result?.renderId || `tool-row-${index}`,
      id: displayCard?.id || detailCard?.id || '',
      kind: result ? 'result' : 'call',
      name: displayCard?.name || detailCard?.name || 'tool',
      payload: displayCard?.payload ?? detailCard?.payload ?? '',
      callPayload: call?.payload,
      resultPayload: result?.payload,
      usage: usageByMergedRow.get(row) || null,
      running: Boolean(call && !result)
    };
  });
}

function addToolUsage(left, right) {
  if (!Number(right?.total || 0)) return left || null;
  if (!left) {
    return {
      input: Number(right.input || 0),
      output: Number(right.output || 0),
      cacheRead: Number(right.cacheRead || 0),
      cacheWrite: Number(right.cacheWrite || 0),
      total: Number(right.total || 0)
    };
  }
  return {
    input: Number(left.input || 0) + Number(right.input || 0),
    output: Number(left.output || 0) + Number(right.output || 0),
    cacheRead: Number(left.cacheRead || 0) + Number(right.cacheRead || 0),
    cacheWrite: Number(left.cacheWrite || 0) + Number(right.cacheWrite || 0),
    total: Number(left.total || 0) + Number(right.total || 0)
  };
}

function toolCardsAreComplete(cards) {
  if (!cards.length) return false;
  const open = new Set();
  let hasResult = false;
  cards.forEach((card, index) => {
    const id = String(card.id || `${card.name || 'tool'}-${index}`);
    if (card.kind === 'call') {
      open.add(id);
    } else if (card.kind === 'result') {
      hasResult = true;
      open.delete(id);
    }
  });
  return hasResult && (open.size === 0 || cards.at(-1)?.kind === 'result');
}

export function MessageBody({ message, onOpenAttachment, onDownloadAttachment }) {
  const text = messageText(message);
  const attachments = Array.isArray(message?.attachments) ? message.attachments : [];
  const files = Array.isArray(message?.files) ? message.files : [];
  const allAttachments = [...attachments, ...files];
  const inlineIndexes = markerIndexes(text);
  const structuredAttachmentIndexes = new Set(
    (Array.isArray(message?.items) ? message.items : [])
      .filter((item) => item?.type === 'file' && item.attachment_index !== undefined)
      .map((item) => Number(item.attachment_index))
      .filter((index) => Number.isFinite(index))
  );
  const trailingAttachments = allAttachments.filter((attachment, index) => {
    const attachmentIndex = Number(attachment?.index);
    return !inlineIndexes.has(index)
      && !inlineIndexes.has(attachmentIndex)
      && !structuredAttachmentIndexes.has(index)
      && !structuredAttachmentIndexes.has(attachmentIndex);
  });
  return (
    <div className="message-body">
      {Array.isArray(message?.items) && message.items.length > 0 ? (
        <StructuredItems role={message?.role} items={message.items} attachments={allAttachments} fallbackText={text} onOpenAttachment={onOpenAttachment} onDownloadAttachment={onDownloadAttachment} />
      ) : text ? (
        <MarkdownContent className="message-text" text={text} attachments={allAttachments} onOpenAttachment={onOpenAttachment} onDownloadAttachment={onDownloadAttachment} />
      ) : trailingAttachments.length > 0 ? null : (
        <div className="message-text muted">空消息</div>
      )}
      {trailingAttachments.length > 0 && <AttachmentList attachments={trailingAttachments} onOpenAttachment={onOpenAttachment} onDownloadAttachment={onDownloadAttachment} />}
      {Number(message?.attachment_count || 0) > 0 && allAttachments.length === 0 && (
        <div className="message-attachments muted">正在加载附件...</div>
      )}
      {message?.tool_name && (
        <div className="tool-chip">{message.tool_name}</div>
      )}
    </div>
  );
}

export function StructuredItems({ role, items, attachments, fallbackText, onOpenAttachment, onDownloadAttachment }) {
  const orderedItems = orderedStructuredItems(items, role);
  const hasTextItem = orderedItems.some(({ item }) => typeof item === 'string' || item?.type === 'text');
  const hasSelectionReference = orderedItems.some(({ item }) => item?.type === 'selection_reference');
  const fallback = String(fallbackText || '').trim();
  const rendered = orderedItems
    .map(({ item, index }) => {
      if (typeof item === 'string') {
        return <MarkdownContent key={index} className="message-text" text={item} attachments={attachments} onOpenAttachment={onOpenAttachment} onDownloadAttachment={onDownloadAttachment} />;
      }
      if (item?.type === 'text') {
        return <MarkdownContent key={index} className="message-text" text={item.text_with_attachment_markers || item.text || item.content || ''} attachments={attachments} onOpenAttachment={onOpenAttachment} onDownloadAttachment={onDownloadAttachment} />;
      }
      if (item?.type === 'file') {
        return <AttachmentCard key={index} attachment={attachments[item.attachment_index] || item} onOpenAttachment={onOpenAttachment} onDownloadAttachment={onDownloadAttachment} />;
      }
      if (item?.type === 'selection_reference') {
        return <SelectionReferenceCard key={index} selection={item.selection || item.payload || item} />;
      }
      if (item?.type === 'reasoning') {
        return <ReasoningNote key={index} text={item.text || item.summary || ''} />;
      }
      if (item?.type === 'tool_call' || item?.type === 'tool_result') {
        return (
          <ToolInlineCard
            key={index}
            kind={item.type === 'tool_result' ? 'result' : 'call'}
            name={item.tool_name || 'tool'}
            payload={item.arguments || item.structured || item.context_with_attachment_markers || item.context || item.result || ''}
          />
        );
      }
      return null;
    })
    .filter(Boolean);
  if (String(role || '').toLowerCase() === 'user' && hasSelectionReference && fallback && !hasTextItem) {
    rendered.push(
      <MarkdownContent
        key="fallback-text"
        className="message-text"
        text={fallbackText}
        attachments={attachments}
        onOpenAttachment={onOpenAttachment}
        onDownloadAttachment={onDownloadAttachment}
      />
    );
  }
  if (rendered.length) return <>{rendered}</>;
  return <MarkdownContent className="message-text" text={fallbackText} attachments={attachments} onOpenAttachment={onOpenAttachment} onDownloadAttachment={onDownloadAttachment} />;
}

function orderedStructuredItems(items, role) {
  const entries = (Array.isArray(items) ? items : []).map((item, index) => ({ item, index }));
  if (String(role || '').toLowerCase() !== 'user') return entries;
  const references = entries.filter(({ item }) => item?.type === 'selection_reference');
  if (!references.length) return entries;
  return [
    ...references,
    ...entries.filter(({ item }) => item?.type !== 'selection_reference')
  ];
}

function SelectionReferenceCard({ selection }) {
  const filePath = selection?.file_path || selection?.fileName || '';
  const sourceKind = selection?.source_kind || selection?.sourceKind || 'selection';
  const text = String(selection?.selected_text || '').replace(/\s+/g, ' ').trim();
  const locator = selection?.locator || {};
  const locatorText = locator.start_line && locator.end_line
    ? `L${locator.start_line}-${locator.end_line}`
    : locator.page
      ? `P${locator.page}`
      : locator.heading || locator.kind || '';
  return (
    <div className="selection-reference-card">
      <div>
        <FileText size={14} />
        <strong>{filePath}</strong>
        <span>{sourceKind}{locatorText ? ` · ${locatorText}` : ''}</span>
      </div>
      {text && <blockquote>{text.length > 240 ? `${text.slice(0, 240)}...` : text}</blockquote>}
    </div>
  );
}

function ReasoningNote({ text }) {
  const value = String(text || '').trim();
  if (!value) return null;
  return (
    <div className="reasoning-note">
      <span>思考</span>
      <MarkdownContent className="reasoning-note-text" text={value} />
    </div>
  );
}

export function MarkdownContent({ text, attachments = [], className = 'markdown-content', onOpenAttachment, onDownloadAttachment }) {
  const value = String(text || '');
  if (!value.trim()) return <span className="message-empty">空消息</span>;
  const parts = [];
  const pattern = /(\[\[attachment:(\d+)]]|\[tool_(call|result)\s+([^\]\n]+)\]\s*([\s\S]*?)(?=\n\[tool_(?:call|result)\s+|$))/g;
  let cursor = 0;
  let match;
  while ((match = pattern.exec(value)) !== null) {
    const before = value.slice(cursor, match.index);
    if (before.trim()) {
      parts.push(<MarkdownBlock key={`text-${cursor}`} text={before} />);
    }
    if (match[2] !== undefined) {
      const attachment = attachments[Number(match[2])];
      if (attachment) {
        parts.push(<AttachmentCard key={`attachment-${match.index}`} attachment={attachment} inline onOpenAttachment={onOpenAttachment} onDownloadAttachment={onDownloadAttachment} />);
      }
    } else if (match[3]) {
      parts.push(
        <ToolInlineCard
          key={`tool-${match.index}`}
          kind={match[3] === 'result' ? 'result' : 'call'}
          name={match[4].trim()}
          payload={match[5].trim()}
        />
      );
    }
    cursor = match.index + match[0].length;
  }
  const rest = value.slice(cursor);
  if (rest.trim()) {
    parts.push(<MarkdownBlock key={`text-${cursor}`} text={rest} />);
  }
  return <div className={className}>{parts.length ? parts : <MarkdownBlock text={value} />}</div>;
}

export function MarkdownBlock({ text }) {
  return (
    <ReactMarkdown
      remarkPlugins={[remarkGfm]}
      rehypePlugins={[rehypeHighlight]}
      components={{
        a: ({ node, ...props }) => (
          <a
            {...props}
            target={isExternalUrl(props.href) ? '_blank' : undefined}
            rel={isExternalUrl(props.href) ? 'noreferrer' : undefined}
            onClick={(event) => handleExternalLinkClick(event, props.href)}
          />
        ),
        img: ({ node, ...props }) => <img {...props} className="message-inline-image" loading="lazy" alt={props.alt || ''} />
      }}
    >
      {text}
    </ReactMarkdown>
  );
}

export function AttachmentList({ attachments, onOpenAttachment, onDownloadAttachment }) {
  return (
    <div className="message-attachments">
      {attachments.map((attachment, index) => (
        <AttachmentCard key={`${attachmentName(attachment)}-${attachment?.path || index}`} attachment={attachment} onOpenAttachment={onOpenAttachment} onDownloadAttachment={onDownloadAttachment} />
      ))}
    </div>
  );
}

export function AttachmentCard({ attachment, inline = false, onOpenAttachment, onDownloadAttachment }) {
  const name = attachmentName(attachment);
  const url = attachmentUrl(attachment);
  const size = formatBytes(attachment?.size_bytes || attachment?.size);
  const [loadedImageSize, setLoadedImageSize] = useState(null);
  const canOpen = Boolean(onOpenAttachment && attachment?.path);
  const canDownload = Boolean(onDownloadAttachment && attachment?.path);
  const openAttachment = () => {
    if (canOpen) onOpenAttachment(attachment);
  };
  const downloadAttachment = (event) => {
    event.stopPropagation();
    if (canDownload) onDownloadAttachment(attachment);
  };
  const meta = attachmentMeta(attachment, name);
  if (isImageAttachment(attachment)) {
    const imageWidth = attachmentImageDisplayWidth(attachment, loadedImageSize);
    const imageStyle = imageWidth ? { '--attachment-image-width': `${imageWidth}px` } : undefined;
    return (
      <div
        className={`message-attachment image${inline ? ' inline' : ''}${url ? '' : ' loading'}${canOpen ? ' clickable' : ''}`}
        style={imageStyle}
        title={canOpen ? `预览 ${name}` : name}
      >
        <button className="attachment-image-preview" type="button" onClick={openAttachment} disabled={!canOpen}>
          {url ? (
          <img
            src={url}
            alt={name}
            loading="lazy"
            onLoad={(event) => {
              const image = event.currentTarget;
              if (image.naturalWidth > 0 && image.naturalHeight > 0) {
                setLoadedImageSize({ width: image.naturalWidth, height: image.naturalHeight });
              }
            }}
          />
          ) : <span className="image-placeholder">正在加载图片</span>}
        </button>
        <div className="attachment-image-caption">
          <span>{name}</span>
          <AttachmentOpenMenu
            name={name}
            canOpen={canOpen}
            canDownload={canDownload}
            onOpen={openAttachment}
            onDownload={downloadAttachment}
          />
        </div>
      </div>
    );
  }
  return (
    <div
      className={`message-attachment file${canOpen ? ' clickable' : ''}`}
      title={canOpen ? `预览 ${name}` : name}
    >
      <button className="attachment-file-main" type="button" onClick={openAttachment} disabled={!canOpen}>
        <span className="attachment-file-icon"><FileText size={18} /></span>
        <span className="attachment-file-text">
          <strong>{name}</strong>
          <small>{[meta, size].filter(Boolean).join(' · ')}</small>
        </span>
      </button>
      <AttachmentOpenMenu
        name={name}
        canOpen={canOpen}
        canDownload={canDownload}
        onOpen={openAttachment}
        onDownload={downloadAttachment}
      />
    </div>
  );
}

function AttachmentOpenMenu({ name, canOpen, canDownload, onOpen, onDownload }) {
  if (!canOpen && !canDownload) return null;
  return (
    <Popover.Root>
      <Popover.Trigger asChild>
        <button className="attachment-open-button" type="button" title={`打开 ${name}`} onClick={(event) => event.stopPropagation()}>
          <span>打开</span>
          <ChevronDown size={14} />
        </button>
      </Popover.Trigger>
      <Popover.Portal>
        <Popover.Content className="floating-popover attachment-menu" side="bottom" align="end" sideOffset={6}>
          {canOpen && (
            <Popover.Close asChild>
              <button className="attachment-menu-item" type="button" onClick={onOpen}>
                预览
              </button>
            </Popover.Close>
          )}
          {canDownload && (
            <Popover.Close asChild>
              <button className="attachment-menu-item" type="button" onClick={onDownload}>
                <Download size={13} />
                下载
              </button>
            </Popover.Close>
          )}
          <Popover.Arrow className="floating-popover-arrow" />
        </Popover.Content>
      </Popover.Portal>
    </Popover.Root>
  );
}

function attachmentMeta(attachment, name) {
  const mediaType = String(attachment?.media_type || attachment?.mime_type || attachment?.mime || '').toLowerCase();
  const ext = fileExtension(name).toUpperCase();
  if (mediaType.startsWith('image/')) return ['图片', ext].filter(Boolean).join(' · ');
  if (mediaType.startsWith('text/') || ['MD', 'TXT', 'JSON', 'CSV', 'TS', 'JS', 'RS', 'PY', 'HTML', 'CSS'].includes(ext)) {
    return ['文档', ext].filter(Boolean).join(' · ');
  }
  if (ext === 'PDF') return '文档 · PDF';
  if (ext) return `文件 · ${ext}`;
  return attachment?.kind ? String(attachment.kind) : '文件';
}

function attachmentNumber(value) {
  const number = Number(value);
  return Number.isFinite(number) && number > 0 ? number : null;
}

function attachmentImageDisplayWidth(attachment, loadedImageSize) {
  const width = attachmentNumber(attachment?.width ?? attachment?.pixel_width ?? loadedImageSize?.width);
  const height = attachmentNumber(attachment?.height ?? attachment?.pixel_height ?? loadedImageSize?.height);
  if (!width) return null;
  const maxWidth = 340;
  const maxHeight = 240;
  const minWidth = 96;
  if (!height) return Math.max(minWidth, Math.min(maxWidth, Math.round(width)));
  const scale = Math.min(1, maxWidth / width, maxHeight / height);
  return Math.max(minWidth, Math.min(maxWidth, Math.round(width * scale)));
}

function parseToolPayload(payload) {
  if (!payload) return {};
  if (typeof payload === 'object') return payload;
  const value = String(payload || '').trim();
  if (!value) return {};
  try {
    return JSON.parse(value);
  } catch {
    return { text: value };
  }
}

function compactToolSummary(text, max = 180) {
  const value = String(text || '').replace(/\s+/g, ' ').trim();
  return value.length > max ? `${value.slice(0, max - 1)}...` : value;
}

function shellOutputText(data) {
  const output = data?.output;
  if (typeof output === 'string') return output;
  if (output && typeof output === 'object') {
    if (typeof output.text === 'string') return output.text;
    if (typeof output.stdout === 'string' || typeof output.stderr === 'string') {
      return [output.stdout, output.stderr].filter(Boolean).join('\n');
    }
  }
  return [data?.stdout, data?.stderr, data?.text].filter((value) => typeof value === 'string').join('\n');
}

function shellResultSummary(data) {
  const text = shellOutputText(data);
  const lines = String(text || '')
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter(Boolean);
  const interesting = [...lines].reverse().find((line) => (
    /Training:/.test(line)
    || /\bstep\s+\d+\b/i.test(line)
    || /\b(checkpoint|Traceback|RuntimeError|ValueError|CUDA|error)\b/i.test(line)
  ));
  return compactToolSummary(interesting || lines.at(-1) || '');
}

function toolDisplay(kind, name, payload) {
  const data = parseToolPayload(payload);
  const lowerName = String(name || '').toLowerCase();
  const isResult = kind === 'result';
  if (lowerName.includes('shell') || lowerName.includes('command') || lowerName.includes('terminal') || lowerName.includes('stdin')) {
    const command = data.command || data.cmd || data.text || '';
    const outputSummary = isResult ? shellResultSummary(data) : '';
    return {
      title: isResult ? '已运行' : '运行',
      chip: name || 'shell',
      summary: outputSummary || command || 'shell command',
      detailTitle: 'Shell'
    };
  }
  if (lowerName.includes('fetch') || lowerName.includes('browser') || lowerName.includes('open_url')) {
    const url = data.url || data.href || data.uri || data.text || '';
    return {
      title: isResult ? '已抓取' : '抓取',
      chip: name,
      summary: url || 'Web page',
      detailTitle: 'Web'
    };
  }
  if (lowerName.includes('image') || lowerName.includes('screenshot')) {
    const file = data.path || data.file_path || data.file || data.url || '';
    return {
      title: isResult ? '已查看' : '查看',
      chip: name,
      summary: file || 'Image',
      detailTitle: 'Image'
    };
  }
  if (lowerName.includes('search') || lowerName.includes('grep') || lowerName === 'rg') {
    const query = data.query || data.pattern || data.q || data.text || '';
    const path = data.path || data.directory || data.cwd || '';
    return {
      title: isResult ? '已搜索' : '搜索',
      chip: name,
      summary: query ? `Searched for ${query}${path ? ` in ${path}` : ''}` : 'Search',
      detailTitle: 'Search'
    };
  }
  if (lowerName.includes('edit') || lowerName.includes('write') || lowerName.includes('patch')) {
    const file = data.path || data.file_path || data.file || '';
    const added = data.added ?? data.additions ?? data.lines_added;
    const removed = data.removed ?? data.deletions ?? data.lines_removed;
    const diff = added !== undefined || removed !== undefined ? ` +${Number(added || 0)} -${Number(removed || 0)}` : '';
    return {
      title: isResult ? '已编辑' : '编辑',
      chip: name,
      summary: file ? `${file}${diff}` : 'Edited files',
      detailTitle: 'Edit'
    };
  }
  if (lowerName.includes('file_read') || lowerName.includes('read')) {
    const file = data.path || data.file_path || data.file || '';
    return {
      title: isResult ? '已读取' : '读取',
      chip: name,
      summary: file || 'Read file',
      detailTitle: 'File'
    };
  }
  const text = data.text || data.path || data.query || data.command || '';
  return {
    title: isResult ? '工具结果' : '调用工具',
    chip: name || 'tool',
    summary: text || name || 'tool',
    detailTitle: name || 'Tool'
  };
}

function toolGroupSummary(cards, fallbackName) {
  const records = toolOperationRecords(cards);
  const counts = new Map();
  records.forEach((card) => {
    const category = toolCategory(card.name);
    counts.set(category, (counts.get(category) || 0) + 1);
  });
  const parts = [
    toolCountLabel(counts, 'search'),
    toolCountLabel(counts, 'command'),
    toolCountLabel(counts, 'edit'),
    toolCountLabel(counts, 'fetch'),
    toolCountLabel(counts, 'read'),
    toolCountLabel(counts, 'image'),
    toolCountLabel(counts, 'plan'),
    toolCountLabel(counts, 'memory'),
    toolCountLabel(counts, 'skill'),
    toolCountLabel(counts, 'cron'),
    toolCountLabel(counts, 'agent'),
    toolCountLabel(counts, 'tool')
  ].filter(Boolean);
  const doneTitle = parts.join(' ') || `已处理 ${fallbackName || '工具'}`;
  const runningTitle = `正在处理 ${fallbackName || '工具'}`;
  return { doneTitle, runningTitle };
}

function toolOperationRecords(cards) {
  const hasCalls = cards.some((card) => card.kind === 'call');
  const seen = new Set();
  return cards.filter((card, index) => {
    if (hasCalls && card.kind !== 'call') return false;
    const id = String(card.id || '').trim();
    const key = id || `${card.kind}:${card.name || 'tool'}:${index}`;
    if (seen.has(key)) return false;
    seen.add(key);
    return true;
  });
}

function toolCategory(name) {
  const lowerName = String(name || '').toLowerCase();
  if (lowerName.includes('search') || lowerName.includes('grep') || lowerName === 'rg') return 'search';
  if (lowerName.includes('shell') || lowerName.includes('command') || lowerName.includes('terminal') || lowerName.includes('stdin')) return 'command';
  if (lowerName.includes('edit') || lowerName.includes('write') || lowerName.includes('patch')) return 'edit';
  if (lowerName.includes('fetch') || lowerName.includes('browser') || lowerName.includes('open_url')) return 'fetch';
  if (lowerName.includes('file_read') || lowerName.includes('read')) return 'read';
  if (lowerName.includes('image') || lowerName.includes('screenshot')) return 'image';
  if (lowerName.includes('plan')) return 'plan';
  if (lowerName.includes('memory')) return 'memory';
  if (lowerName.includes('skill')) return 'skill';
  if (lowerName.includes('cron') || lowerName.includes('automation')) return 'cron';
  if (lowerName.includes('agent') || lowerName.includes('subagent')) return 'agent';
  return 'tool';
}

function toolCountLabel(counts, category) {
  const count = counts.get(category) || 0;
  if (!count) return '';
  if (category === 'search') return `已探索 ${count} 次搜索`;
  if (category === 'command') return `已运行 ${count} 条命令`;
  if (category === 'edit') return `已编辑 ${count} 次`;
  if (category === 'fetch') return `已抓取 ${count} 个网页`;
  if (category === 'read') return `已读取 ${count} 个文件`;
  if (category === 'image') return `已查看 ${count} 张图片`;
  if (category === 'plan') return count === 1 ? '已更新计划' : `已更新 ${count} 次计划`;
  if (category === 'memory') return `已访问 ${count} 次记忆`;
  if (category === 'skill') return `已运行 ${count} 个技能`;
  if (category === 'cron') return `已处理 ${count} 个定时任务`;
  if (category === 'agent') return `已运行 ${count} 个代理`;
  return `已调用 ${count} 个工具`;
}

function ToolDetail({ title, name, payload }) {
  const data = parseToolPayload(payload);
  const patchText = editPatchText(name, data);
  if (patchText) {
    return <EditDiffDetail title={title} data={data} patchText={patchText} />;
  }
  const entries = Object.entries(data).filter(([, value]) => value !== undefined && value !== null && value !== '');
  if (entries.length === 1 && entries[0][0] === 'text') {
    return (
      <div className="tool-detail">
        <strong>{title}</strong>
        <pre><code>{entries[0][1]}</code></pre>
      </div>
    );
  }
  return (
    <div className="tool-detail">
      <strong>{title}</strong>
      {entries.length > 0 ? (
        <dl>
          {entries.map(([key, value]) => (
            <div key={key}>
              <dt>{key}</dt>
              <dd>{typeof value === 'string' ? value : JSON.stringify(value, null, 2)}</dd>
            </div>
          ))}
        </dl>
      ) : (
        <span className="muted">没有详细内容</span>
      )}
    </div>
  );
}

function editPatchText(name, data) {
  const lowerName = String(name || '').toLowerCase();
  if (!lowerName.includes('edit') && !lowerName.includes('write') && !lowerName.includes('patch')) return '';
  const patch = typeof data.patch === 'string'
    ? data.patch
    : typeof data.diff === 'string'
      ? data.diff
      : typeof data.text === 'string'
        ? data.text
        : '';
  if (!patch.trim()) return '';
  if (
    /(^|\n)(diff --git|--- |\+\+\+ |@@ |@@$|\*\*\* (Begin Patch|Update File|Add File|Delete File))/m.test(patch)
  ) {
    return patch;
  }
  return '';
}

function EditDiffDetail({ title, data, patchText }) {
  const files = useMemo(() => parsePatchForDisplay(patchText), [patchText]);
  const metaEntries = Object.entries(data)
    .filter(([key, value]) => (
      !['patch', 'diff', 'text'].includes(key)
      && value !== undefined
      && value !== null
      && value !== ''
    ));
  return (
    <div className="tool-detail edit-diff-detail">
      <strong>{title}</strong>
      {metaEntries.length > 0 ? (
        <div className="edit-diff-meta">
          {metaEntries.map(([key, value]) => (
            <div key={key}>
              <span>{key}</span>
              <code>{typeof value === 'string' ? value : JSON.stringify(value)}</code>
            </div>
          ))}
        </div>
      ) : null}
      <DiffViewer files={files} />
    </div>
  );
}

function DiffViewer({ files }) {
  if (!files.length) {
    return <span className="muted">没有可展示的 diff</span>;
  }
  return (
    <div className="edit-diff-view edit-diff-view-split-ready">
      <div className="diff-unified">
        {files.map((file, index) => (
          <DiffUnifiedFile file={file} key={`${file.path}-${index}`} />
        ))}
      </div>
      <div className="diff-split">
        {files.map((file, index) => (
          <DiffSplitFile file={file} key={`${file.path}-${index}`} />
        ))}
      </div>
    </div>
  );
}

function DiffUnifiedFile({ file }) {
  return (
    <section className="diff-file">
      <div className="diff-file-head">
        <span>{file.path}</span>
        <code>{file.additions ? `+${file.additions}` : '+0'} {file.deletions ? `-${file.deletions}` : '-0'}</code>
      </div>
      {file.hunks.map((hunk, index) => (
        <div className="diff-hunk" key={`${hunk.header}-${index}`}>
          {hunk.header ? <div className="diff-hunk-head">{hunk.header}</div> : null}
          {limitedDiffLines(hunk.lines).map((line, lineIndex) => (
            <div className={`diff-row ${line.type}`} key={lineIndex}>
              <span className="diff-gutter old">{formatLineNumber(line.oldNumber)}</span>
              <span className="diff-gutter new">{formatLineNumber(line.newNumber)}</span>
              <span className="diff-marker">{diffMarker(line.type)}</span>
              <code>{line.text}</code>
            </div>
          ))}
          {hunk.lines.length > DIFF_MAX_LINES_PER_HUNK ? (
            <div className="diff-omitted">{hunk.lines.length - DIFF_MAX_LINES_PER_HUNK} lines omitted</div>
          ) : null}
        </div>
      ))}
    </section>
  );
}

function DiffSplitFile({ file }) {
  const leftRef = useRef(null);
  const rightRef = useRef(null);
  const syncingRef = useRef(false);
  const [paneHeight, setPaneHeight] = useState('auto');
  const hunks = useMemo(() => file.hunks.map((hunk, index) => {
    const rows = splitDiffRows(hunk.lines);
    return {
      id: `${hunk.header}-${index}`,
      header: hunk.header,
      rows: limitedSplitRows(rows),
      omitted: Math.max(0, rows.length - DIFF_MAX_LINES_PER_HUNK)
    };
  }), [file.hunks]);
  useEffect(() => {
    const estimateRows = hunks.reduce((total, hunk) => total + hunk.rows.length + (hunk.header ? 1 : 0) + (hunk.omitted ? 1 : 0), 0);
    const naturalHeight = Math.max(72, 24 + estimateRows * 22);
    const maxHeight = Math.max(220, Math.round(window.innerHeight * 0.64));
    setPaneHeight(`${Math.min(naturalHeight, maxHeight, 680)}px`);
  }, [hunks]);
  useEffect(() => {
    const leftNode = leftRef.current;
    const rightNode = rightRef.current;
    if (!leftNode || !rightNode) return undefined;
    const syncScroll = (sourceNode, targetNode) => {
      if (syncingRef.current) return;
      if (Math.abs(targetNode.scrollTop - sourceNode.scrollTop) < 1) return;
      syncingRef.current = true;
      targetNode.scrollTop = sourceNode.scrollTop;
      window.requestAnimationFrame(() => {
        syncingRef.current = false;
      });
    };
    const syncLeft = () => syncScroll(leftNode, rightNode);
    const syncRight = () => syncScroll(rightNode, leftNode);
    leftNode.addEventListener('scroll', syncLeft, { passive: true });
    rightNode.addEventListener('scroll', syncRight, { passive: true });
    return () => {
      leftNode.removeEventListener('scroll', syncLeft);
      rightNode.removeEventListener('scroll', syncRight);
    };
  }, [hunks]);
  return (
    <section className="diff-file">
      <div className="diff-file-head">
        <span>{file.path}</span>
        <code>{file.additions ? `+${file.additions}` : '+0'} {file.deletions ? `-${file.deletions}` : '-0'}</code>
      </div>
      <div className="diff-split-panes" style={{ '--diff-pane-height': paneHeight }}>
        <DiffSplitPane side="old" hunks={hunks} paneRef={leftRef} />
        <DiffSplitPane side="new" hunks={hunks} paneRef={rightRef} />
      </div>
    </section>
  );
}

function DiffSplitPane({ side, hunks, paneRef }) {
  return (
    <div className={`diff-split-pane ${side}`} ref={paneRef}>
      {hunks.map((hunk) => (
        <div className="diff-split-pane-hunk" key={hunk.id}>
          {hunk.header ? <div className="diff-hunk-head split">{hunk.header}</div> : null}
          {hunk.rows.map((row, rowIndex) => (
            row.type === 'meta'
              ? <div className="diff-pane-line meta" key={rowIndex}><code>{row.text}</code></div>
              : <DiffSplitCell key={rowIndex} side={side} line={side === 'old' ? row.left : row.right} />
          ))}
          {hunk.omitted > 0 ? <div className="diff-omitted">{hunk.omitted} rows omitted</div> : null}
        </div>
      ))}
    </div>
  );
}

function DiffSplitCell({ line }) {
  const type = line?.type || 'empty';
  return (
    <div className={`diff-pane-line ${type}`}>
      <span className="diff-gutter">{formatLineNumber(line?.oldNumber ?? line?.newNumber)}</span>
      <code>{line?.text || ''}</code>
    </div>
  );
}

const DIFF_MAX_LINES_PER_HUNK = 260;

function limitedDiffLines(lines) {
  return lines.slice(0, DIFF_MAX_LINES_PER_HUNK);
}

function limitedSplitRows(rows) {
  return rows.slice(0, DIFF_MAX_LINES_PER_HUNK);
}

function parsePatchForDisplay(patchText) {
  const lines = String(patchText || '').replace(/\r\n/g, '\n').split('\n');
  const files = [];
  let current = null;
  let hunk = null;

  const ensureFile = (path = 'patch') => {
    if (!current || current.path !== path) {
      current = { path, additions: 0, deletions: 0, hunks: [] };
      files.push(current);
      hunk = null;
    }
    return current;
  };
  const ensureHunk = (header = '') => {
    ensureFile(current?.path || 'patch');
    if (!hunk || header) {
      hunk = { header, oldLine: null, newLine: null, lines: [] };
      const match = header.match(/@@\s+-(\d+)(?:,\d+)?\s+\+(\d+)(?:,\d+)?/);
      if (match) {
        hunk.oldLine = Number(match[1]);
        hunk.newLine = Number(match[2]);
      }
      current.hunks.push(hunk);
    }
    return hunk;
  };
  const addLine = (type, text, original) => {
    const target = ensureHunk('');
    let oldNumber = null;
    let newNumber = null;
    if (type === 'context') {
      oldNumber = target.oldLine;
      newNumber = target.newLine;
      if (target.oldLine !== null) target.oldLine += 1;
      if (target.newLine !== null) target.newLine += 1;
    } else if (type === 'remove') {
      oldNumber = target.oldLine;
      if (target.oldLine !== null) target.oldLine += 1;
      current.deletions += 1;
    } else if (type === 'add') {
      newNumber = target.newLine;
      if (target.newLine !== null) target.newLine += 1;
      current.additions += 1;
    }
    target.lines.push({ type, text, oldNumber, newNumber, original });
  };

  for (const rawLine of lines) {
    if (rawLine === '*** Begin Patch' || rawLine === '*** End Patch') {
      continue;
    }
    if (rawLine.startsWith('diff --git ')) {
      const match = rawLine.match(/^diff --git a\/(.+?) b\/(.+)$/);
      ensureFile(match?.[2] || match?.[1] || rawLine.replace(/^diff --git\s+/, ''));
      continue;
    }
    if (rawLine.startsWith('*** Update File: ') || rawLine.startsWith('*** Add File: ') || rawLine.startsWith('*** Delete File: ')) {
      ensureFile(rawLine.replace(/^\*\*\* (?:Update|Add|Delete) File:\s+/, '').trim() || 'patch');
      continue;
    }
    if (rawLine.startsWith('+++ ')) {
      const path = rawLine.replace(/^\+\+\+\s+/, '').replace(/^[ab]\//, '').trim();
      if (path && path !== '/dev/null') ensureFile(path);
      continue;
    }
    if (rawLine.startsWith('--- ')) {
      const path = rawLine.replace(/^---\s+/, '').replace(/^[ab]\//, '').trim();
      if (!current && path && path !== '/dev/null') ensureFile(path);
      continue;
    }
    if (rawLine.startsWith('@@')) {
      ensureHunk(rawLine);
      continue;
    }
    if (rawLine.startsWith('+') && !rawLine.startsWith('+++')) {
      addLine('add', rawLine.slice(1), rawLine);
    } else if (rawLine.startsWith('-') && !rawLine.startsWith('---')) {
      addLine('remove', rawLine.slice(1), rawLine);
    } else if (rawLine.startsWith(' ')) {
      addLine('context', rawLine.slice(1), rawLine);
    } else if (rawLine.trim()) {
      addLine('meta', rawLine, rawLine);
    }
  }
  return files
    .map((file) => ({
      ...file,
      hunks: file.hunks.filter((item) => item.lines.length > 0 || item.header)
    }))
    .filter((file) => file.hunks.length > 0);
}

function splitDiffRows(lines) {
  const rows = [];
  for (let index = 0; index < lines.length; index += 1) {
    const line = lines[index];
    if (line.type === 'remove') {
      const removes = [];
      const adds = [];
      while (lines[index]?.type === 'remove') {
        removes.push(lines[index]);
        index += 1;
      }
      while (lines[index]?.type === 'add') {
        adds.push(lines[index]);
        index += 1;
      }
      index -= 1;
      const count = Math.max(removes.length, adds.length);
      for (let pairIndex = 0; pairIndex < count; pairIndex += 1) {
        rows.push({ type: 'change', left: removes[pairIndex] || null, right: adds[pairIndex] || null });
      }
    } else if (line.type === 'add') {
      rows.push({ type: 'add', left: null, right: line });
    } else if (line.type === 'context') {
      rows.push({ type: 'context', left: line, right: line });
    } else {
      rows.push({ type: 'meta', text: line.text });
    }
  }
  return rows;
}

function formatLineNumber(value) {
  return Number.isFinite(value) ? value : '';
}

function diffMarker(type) {
  if (type === 'add') return '+';
  if (type === 'remove') return '-';
  return ' ';
}

export function ToolInlineCard({ kind, name, payload, callPayload, resultPayload, usage, running = false }) {
  const display = toolDisplay(kind, name, payload);
  const hasMergedPayload = callPayload !== undefined || resultPayload !== undefined;
  return (
    <details className={`tool-inline-card ${kind}${running ? ' running' : ''}`}>
      <summary>
        <span>{display.title}</span>
        <code>{display.chip}</code>
        <em>{display.summary}</em>
        <InlineTokenUsage usage={usage} />
      </summary>
      {hasMergedPayload ? (
        <MergedToolDetail name={name} callPayload={callPayload} resultPayload={resultPayload} />
      ) : (
        <ToolDetail title={display.detailTitle} name={name} payload={payload} />
      )}
    </details>
  );
}

function MergedToolDetail({ name, callPayload, resultPayload }) {
  return (
    <div className="merged-tool-detail">
      {callPayload !== undefined ? <ToolDetail title="调用参数" name={name} payload={callPayload} /> : null}
      {resultPayload !== undefined ? <ToolDetail title="工具结果" name={name} payload={resultPayload} /> : null}
    </div>
  );
}

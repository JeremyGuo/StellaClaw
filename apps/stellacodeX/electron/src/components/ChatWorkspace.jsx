import { Fragment, memo, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import ReactMarkdown from 'react-markdown';
import rehypeHighlight from 'rehype-highlight';
import rehypeKatex from 'rehype-katex';
import remarkGfm from 'remark-gfm';
import remarkMath from 'remark-math';
import 'katex/dist/katex.min.css';
import { ChevronDown, Code2, Copy, Download, Eye, FileText, Plus, Send, TerminalSquare } from 'lucide-react';
import * as Popover from '@radix-ui/react-popover';
import { attachmentName, attachmentUrl, fileExtension, fileNameFromPath, isImageAttachment, messageText } from '../lib/fileUtils';
import { handleExternalLinkClick, isExternalUrl } from '../lib/externalLinks';
import { formatBytes, formatTokens, modelAlias, modelDisplayName } from '../lib/format';
import { firstMessageId, isExecutionMessage, isFinalAssistantMessage, liveActivitySignature, markerIndexes, messageItems, messageKey, splitMessageForDisplay, tokenUsage, toolCardsForMessage } from '../lib/messageUtils';
import { measureChatPerf, recordChatPerf } from '../lib/chatPerfMetrics';
import { ChatPerfPopover } from './chat/ChatPerfPopover';
import { composerAttachmentFromFile, isImageFileObject, outgoingAttachmentPayload, selectionSummary } from './chat/composerAttachments';
import { InlineActivityStatus, LiveActivityStack, shouldShowInlineActivity } from './chat/LiveActivity';
import { renderCommitStart, useRenderCommitPerf } from './chat/perfHooks';
import { buildChatRenderModel } from './chat/renderModel';
import { mergedToolCards, sameToolBlock, sameUsage, toolCardsAreComplete, toolGroupSummary } from './chat/toolCards';
import { VIRTUALIZE_ENTRY_THRESHOLD, virtualWindowForEntries } from './chat/virtualWindow';

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

const datePartFormatters = new Map();
const clockFormatters = new Map();

export function ChatWorkspace({ conversationKey: activeMessageScope, modelSelectionPending = false, messages, messagesReady, mode, hasOlder, onLoadOlder, onSend, onLoadModels, sending, processing = false, runningActivities, selectionReferences = [], onRemoveSelectionReference, onOpenAttachment, onDownloadAttachment, onResolveAttachmentUrl, onOpenLocalLink }) {
  const renderStartedAt = renderCommitStart();
  const currentActivity = (runningActivities || []).at(-1) || null;
  const renderModel = useMemo(() => measureChatPerf('chat.render_model.total', () => buildChatRenderModel({
    messages,
    currentActivity,
    sending,
    processing,
    modelSelectionPending
  }), { messages: messages?.length || 0, activity: currentActivity?.id || currentActivity?.kind || '' }), [messages, currentActivity, sending, processing, modelSelectionPending]);
  const {
    renderedMessages,
    renderEntries,
    entryKeys,
    latestAssistantTurnIndex,
    pendingAssistantVisible,
    responseSpacerVisible
  } = renderModel;
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
  const contentRef = useRef(null);
  const virtualHeightsRef = useRef(new Map());
  const previousCountRef = useRef(0);
  const loadingOlderRef = useRef(false);
  const prependAdjustRef = useRef(null);
  const stickToBottomRef = useRef(true);
  const [toolStopNoticeReady, setToolStopNoticeReady] = useState(false);
  const [viewport, setViewport] = useState({ scrollTop: 0, clientHeight: 0 });
  const [virtualHeightVersion, setVirtualHeightVersion] = useState(0);
  const [elapsedTickMs, setElapsedTickMs] = useState(() => Date.now());
  const inlineActivity = shouldShowInlineActivity(currentActivity) ? currentActivity : null;
  const progressVisible = Boolean(currentActivity);
  const sessionRunning = Boolean(processing || currentActivity);
  const virtualWindow = useMemo(() => measureChatPerf('chat.virtual_window', () => virtualWindowForEntries({
    entries: renderEntries,
    keys: entryKeys,
    heightCache: virtualHeightsRef.current,
    heightVersion: virtualHeightVersion,
    viewport,
    activeIndex: latestAssistantTurnIndex
  }), { entries: renderEntries.length, virtualized: renderEntries.length > VIRTUALIZE_ENTRY_THRESHOLD }), [renderEntries, entryKeys, virtualHeightVersion, viewport, latestAssistantTurnIndex]);
  const toolStopNoticeCandidate = useMemo(() => {
    if (!messagesReady || sending || processing || currentActivity || !messages.length) return false;
    const lastMessage = messages.at(-1);
    return isExecutionMessage(lastMessage);
  }, [messages, messagesReady, sending, processing, currentActivity]);
  const turnStoppedAfterTool = toolStopNoticeCandidate && toolStopNoticeReady;

  useRenderCommitPerf('chat.workspace.render_commit', renderStartedAt, () => ({
      messages: messages?.length || 0,
      entries: renderEntries.length,
      visible: virtualWindow.items.length,
      activity: currentActivity?.id || currentActivity?.kind || ''
  }));

  useEffect(() => {
    if (!sessionRunning) return undefined;
    setElapsedTickMs(Date.now());
    const timer = window.setInterval(() => {
      setElapsedTickMs(Date.now());
    }, 1000);
    return () => window.clearInterval(timer);
  }, [sessionRunning, activeMessageScope]);

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

  function updateResponseSpacerMetrics() {
    const list = scrollRef.current;
    if (!list) return;
    if (!responseSpacerVisible) {
      list.style.setProperty('--response-spacer-height', '0px');
      return;
    }
    const root = list.closest('.chat-workspace');
    const composerHeight = root?.querySelector('.composer-wrap')?.getBoundingClientRect().height || 0;
    const usableHeight = Math.max(160, list.clientHeight - composerHeight);
    const height = Math.round(Math.min(48, Math.max(12, usableHeight * 0.05)));
    list.style.setProperty('--response-spacer-height', `${height}px`);
  }

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
    updateResponseSpacerMetrics();
    if (stickToBottomRef.current) {
      requestAnimationFrame(scrollToBottom);
    }
  }, [responseSpacerVisible, activeMessageScope, renderedMessages.length, messages.length, newestMessageKey, messagesReady, activitySignature, pendingAssistantVisible]);

  useLayoutEffect(() => {
    const list = scrollRef.current;
    const content = contentRef.current;
    const composer = composerRef.current;
    if (!list) return undefined;
    const observer = new ResizeObserver(() => {
      measureChatPerf('chat.resize_observer.message_scroll', () => {
        updateResponseSpacerMetrics();
        syncViewport();
        if (stickToBottomRef.current) requestAnimationFrame(scrollToBottom);
      });
    });
    observer.observe(list);
    if (content) observer.observe(content);
    if (composer) observer.observe(composer);
    return () => observer.disconnect();
  }, [responseSpacerVisible, activeMessageScope, renderEntries.length]);

  useLayoutEffect(() => {
    const content = contentRef.current;
    if (!content || !virtualWindow.virtualized) return undefined;
    let frame = 0;
    const measure = () => {
      frame = 0;
      const changed = measureChatPerf('chat.virtual_measure.visible_entries', () => {
        let didChange = false;
        content.querySelectorAll('[data-virtual-key]').forEach((node) => {
          const key = node.getAttribute('data-virtual-key');
          if (!key) return;
          const height = Math.ceil(node.getBoundingClientRect().height);
          if (!Number.isFinite(height) || height <= 0) return;
          if (Math.abs((virtualHeightsRef.current.get(key) || 0) - height) > 1) {
            virtualHeightsRef.current.set(key, height);
            didChange = true;
          }
        });
        return didChange;
      }, { visible: virtualWindow.items.length });
      if (changed) {
        setVirtualHeightVersion((value) => value + 1);
        if (stickToBottomRef.current) requestAnimationFrame(scrollToBottom);
      }
    };
    const scheduleMeasure = () => {
      if (frame) return;
      frame = requestAnimationFrame(measure);
    };
    scheduleMeasure();
    const observer = new ResizeObserver(scheduleMeasure);
    content.querySelectorAll('[data-virtual-key]').forEach((node) => observer.observe(node));
    return () => {
      if (frame) cancelAnimationFrame(frame);
      observer.disconnect();
    };
  }, [virtualWindow.virtualized, virtualWindow.start, virtualWindow.end, activeMessageScope, newestMessageKey, activitySignature]);

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
    list.scrollTop = Math.max(0, list.scrollHeight - list.clientHeight);
  };

  const syncViewport = () => {
    const list = scrollRef.current;
    if (!list) return;
    const next = {
      scrollTop: list.scrollTop,
      clientHeight: list.clientHeight
    };
    setViewport((current) => (
      Math.abs(current.scrollTop - next.scrollTop) < 1 && Math.abs(current.clientHeight - next.clientHeight) < 1
        ? current
        : next
    ));
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
    setViewport({ scrollTop: 0, clientHeight: scrollRef.current?.clientHeight || 0 });
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
    syncViewport();
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

  const continueTurn = useCallback(() => {
    onSend?.('/continue');
  }, [onSend]);

  return (
    <section className="chat-workspace">
      <div className="message-scroll" ref={scrollRef} onScroll={handleScroll}>
        <MemoMessageStreamView
          modelSelectionPending={modelSelectionPending}
          models={models}
          modelsLoading={modelsLoading}
          modelsError={modelsError}
          onReloadModels={openModelOptions}
          onChooseModel={chooseModel}
          renderedMessages={renderedMessages}
          virtualWindow={virtualWindow}
          contentRef={contentRef}
          sessionRunning={sessionRunning}
          latestAssistantTurnIndex={latestAssistantTurnIndex}
          elapsedTickMs={elapsedTickMs}
          pendingAssistantVisible={pendingAssistantVisible}
          inlineActivity={inlineActivity}
          turnStoppedAfterTool={turnStoppedAfterTool}
          onContinue={continueTurn}
          sending={sending}
          onOpenAttachment={onOpenAttachment}
          onDownloadAttachment={onDownloadAttachment}
          onResolveAttachmentUrl={onResolveAttachmentUrl}
          onOpenLocalLink={onOpenLocalLink}
        />
      </div>
      <ChatPerfPopover />
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

function MessageStreamView({
  modelSelectionPending,
  models,
  modelsLoading,
  modelsError,
  onReloadModels,
  onChooseModel,
  renderedMessages,
  virtualWindow,
  contentRef,
  sessionRunning,
  latestAssistantTurnIndex,
  elapsedTickMs,
  pendingAssistantVisible,
  inlineActivity,
  turnStoppedAfterTool,
  onContinue,
  sending,
  onOpenAttachment,
  onDownloadAttachment,
  onResolveAttachmentUrl,
  onOpenLocalLink
}) {
  if (modelSelectionPending) {
    return (
      <ModelSelectionGate
        models={models}
        loading={modelsLoading}
        error={modelsError}
        onReload={onReloadModels}
        onChoose={onChooseModel}
      />
    );
  }

  if (renderedMessages.length === 0) {
    return (
      <div className="empty-chat">
        <strong>欢迎使用 Stellacode</strong>
        <span>选择一个 Conversation，或者新建对话，让 Stellacode 帮你检查项目、修改代码、运行命令和整理上下文。</span>
      </div>
    );
  }

  return (
    <div className="message-stream-content" ref={contentRef}>
      {virtualWindow.virtualized && virtualWindow.topPadding > 0 && (
        <div className="virtual-transcript-spacer" style={{ height: `${virtualWindow.topPadding}px` }} aria-hidden="true" />
      )}
      {virtualWindow.items.map(({ entry, index, key }) => (
        <div className="virtual-entry" key={key} data-virtual-key={key}>
          {entry.type === 'assistantTurn'
            ? (
              <MemoAssistantTurn
                entry={entry}
                active={sessionRunning && index === latestAssistantTurnIndex}
                elapsedNowMs={sessionRunning && index === latestAssistantTurnIndex ? elapsedTickMs : undefined}
                onOpenAttachment={onOpenAttachment}
                onDownloadAttachment={onDownloadAttachment}
                onResolveAttachmentUrl={onResolveAttachmentUrl}
                onOpenLocalLink={onOpenLocalLink}
              />
            )
            : (
              <MemoMessageArticle
                message={entry.message}
                onOpenAttachment={onOpenAttachment}
                onDownloadAttachment={onDownloadAttachment}
                onResolveAttachmentUrl={onResolveAttachmentUrl}
                onOpenLocalLink={onOpenLocalLink}
              />
            )}
        </div>
      ))}
      {virtualWindow.virtualized && virtualWindow.bottomPadding > 0 && (
        <div className="virtual-transcript-spacer" style={{ height: `${virtualWindow.bottomPadding}px` }} aria-hidden="true" />
      )}
      {pendingAssistantVisible && <PendingAssistantPlaceholder />}
      {inlineActivity && <InlineActivityStatus activity={inlineActivity} />}
      {turnStoppedAfterTool && (
        <div className="turn-continuation-notice">
          <span>本轮停在工具结果后，没有后续 assistant 消息。</span>
          <button type="button" onClick={onContinue} disabled={sending}>
            继续
          </button>
        </div>
      )}
      <div className="response-spacer" aria-hidden="true" />
    </div>
  );
}

const MemoMessageStreamView = memo(MessageStreamView, (previous, next) => {
  if (previous.modelSelectionPending !== next.modelSelectionPending) return false;
  if (next.modelSelectionPending) {
    return previous.models === next.models
      && previous.modelsLoading === next.modelsLoading
      && previous.modelsError === next.modelsError
      && previous.onReloadModels === next.onReloadModels
      && previous.onChooseModel === next.onChooseModel;
  }
  return previous.renderedMessages === next.renderedMessages
    && previous.virtualWindow === next.virtualWindow
    && previous.contentRef === next.contentRef
    && previous.sessionRunning === next.sessionRunning
    && previous.latestAssistantTurnIndex === next.latestAssistantTurnIndex
    && previous.elapsedTickMs === next.elapsedTickMs
    && previous.pendingAssistantVisible === next.pendingAssistantVisible
    && previous.inlineActivity === next.inlineActivity
    && previous.turnStoppedAfterTool === next.turnStoppedAfterTool
    && previous.onContinue === next.onContinue
    && previous.sending === next.sending
    && previous.onOpenAttachment === next.onOpenAttachment
    && previous.onDownloadAttachment === next.onDownloadAttachment
    && previous.onResolveAttachmentUrl === next.onResolveAttachmentUrl
    && previous.onOpenLocalLink === next.onOpenLocalLink;
});

function PendingAssistantPlaceholder({ compact = false, label = '正在思考' }) {
  return (
    <div className={`pending-assistant-placeholder${compact ? ' compact' : ''}`} aria-live="polite">
      <span>{label}</span>
    </div>
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

export function MessageArticle({ message, onOpenAttachment, onDownloadAttachment, onResolveAttachmentUrl, onOpenLocalLink }) {
  const renderStartedAt = renderCommitStart();
  const usage = tokenUsage(message);
  const role = message.user_name || message.role || 'assistant';
  const className = messageArticleClassName(message);
  const roleName = String(message.role || '').toLowerCase();
  const auxiliaryMessages = Array.isArray(message._auxiliary) ? message._auxiliary : [];
  useRenderCommitPerf('chat.message.render_commit', renderStartedAt, () => ({
      role: roleName,
      streaming: Boolean(message?._streaming),
      textLen: messageText(message).length,
      items: messageItems(message).length
  }));
  return (
    <article className={className}>
      {auxiliaryMessages.length > 0 && (
        <div className="message-role">
          <span>{role}</span>
          <AuxiliaryDots messages={auxiliaryMessages} />
        </div>
      )}
      <MessageBody message={message} onOpenAttachment={onOpenAttachment} onDownloadAttachment={onDownloadAttachment} onResolveAttachmentUrl={onResolveAttachmentUrl} onOpenLocalLink={onOpenLocalLink} />
      {(roleName === 'user' || (roleName === 'assistant' && !message._streaming)) && (
        <MessageActionBar message={message} role={roleName} usage={usage} />
      )}
      {message.pending && <div className="message-status">正在发送...</div>}
      {!message.pending && message.queued && <div className="message-status">已排队</div>}
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
  const parts = datePartFormatter(systemTimeZone()).formatToParts(date);
  return {
    year: Number(parts.find((part) => part.type === 'year')?.value || 0),
    month: Number(parts.find((part) => part.type === 'month')?.value || 0),
    day: Number(parts.find((part) => part.type === 'day')?.value || 0)
  };
}

function systemClock(date) {
  return clockFormatter(systemTimeZone()).format(date);
}

function systemTimeZone() {
  return Intl.DateTimeFormat().resolvedOptions().timeZone;
}

function datePartFormatter(timeZone) {
  const key = timeZone || 'system';
  let formatter = datePartFormatters.get(key);
  if (!formatter) {
    formatter = new Intl.DateTimeFormat(undefined, {
      timeZone,
      year: 'numeric',
      month: 'numeric',
      day: 'numeric'
    });
    datePartFormatters.set(key, formatter);
  }
  return formatter;
}

function clockFormatter(timeZone) {
  const key = timeZone || 'system';
  let formatter = clockFormatters.get(key);
  if (!formatter) {
    formatter = new Intl.DateTimeFormat(undefined, {
      timeZone,
      hour: 'numeric',
      minute: '2-digit',
      hourCycle: 'h23'
    });
    clockFormatters.set(key, formatter);
  }
  return formatter;
}

function messageArticleClassName(message) {
  const classes = ['message', message.role || 'assistant'];
  if (message._forceSeparate) classes.push('force-separate');
  if (message._streaming) classes.push('streaming');
  if (message.pending) classes.push('pending');
  if (message.queued) classes.push('queued');
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
  if (
    previous.onOpenAttachment !== next.onOpenAttachment
    || previous.onDownloadAttachment !== next.onDownloadAttachment
    || previous.onResolveAttachmentUrl !== next.onResolveAttachmentUrl
    || previous.onOpenLocalLink !== next.onOpenLocalLink
  ) return false;
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

export function AssistantTurn({ entry, active = false, elapsedNowMs, onOpenAttachment, onDownloadAttachment, onResolveAttachmentUrl, onOpenLocalLink }) {
  const finalMessage = entry.finalMessage;
  const complete = isFinalAssistantMessage(finalMessage);
  return (
    <section className={`assistant-turn${active ? ' active' : ''}${complete ? ' complete' : ''}`}>
      <MemoToolProcessGroup group={entry.processGroup} active={active} elapsedNowMs={elapsedNowMs} />
      {finalMessage && (
        <MemoMessageArticle
          message={finalMessage}
          onOpenAttachment={onOpenAttachment}
          onDownloadAttachment={onDownloadAttachment}
          onResolveAttachmentUrl={onResolveAttachmentUrl}
          onOpenLocalLink={onOpenLocalLink}
        />
      )}
    </section>
  );
}

const MemoAssistantTurn = memo(AssistantTurn, (previous, next) => (
  previous.active === next.active
  && previous.elapsedNowMs === next.elapsedNowMs
  && previous.onOpenAttachment === next.onOpenAttachment
  && previous.onDownloadAttachment === next.onDownloadAttachment
  && previous.onResolveAttachmentUrl === next.onResolveAttachmentUrl
  && previous.onOpenLocalLink === next.onOpenLocalLink
  && sameAssistantTurnEntry(previous.entry, next.entry)
));

function sameAssistantTurnEntry(left, right) {
  if (left === right) return true;
  if (!left || !right) return false;
  return left.type === right.type
    && left.id === right.id
    && sameMessageProjection(left.finalMessage, right.finalMessage)
    && sameToolGroup(left.processGroup, right.processGroup);
}

function sameMessageProjection(left, right) {
  if (left === right) return true;
  if (!left || !right) return false;
  return String(left.id ?? left.message_id ?? '') === String(right.id ?? right.message_id ?? '')
    && left.index === right.index
    && left.role === right.role
    && left.message_time === right.message_time
    && left.text === right.text
    && left.preview === right.preview
    && left.content === right.content
    && left.text_with_attachment_markers === right.text_with_attachment_markers
    && left.items === right.items
    && left.attachments === right.attachments
    && left.files === right.files
    && Boolean(left._forceSeparate) === Boolean(right._forceSeparate)
    && Boolean(left._streaming) === Boolean(right._streaming);
}

function sameToolGroup(left, right) {
  if (left === right) return true;
  if (!left || !right) return false;
  if (left.id !== right.id || left.nextMessage !== right.nextMessage) return false;
  const leftMessages = Array.isArray(left.messages) ? left.messages : [];
  const rightMessages = Array.isArray(right.messages) ? right.messages : [];
  if (leftMessages.length !== rightMessages.length) return false;
  for (let index = 0; index < leftMessages.length; index += 1) {
    if (leftMessages[index] !== rightMessages[index]) return false;
  }
  return true;
}

export function ToolProcessGroup({ group, active = false, elapsedNowMs }) {
  const renderStartedAt = renderCommitStart();
  const messages = group.messages || [];
  const expandedRows = useMemo(() => measureChatPerf('chat.tool_group.expand_rows', () => messages.map((message, index) => {
    const { textMessage, toolCards, segments } = splitMessageForDisplay(message);
    return {
      id: messageKey(message, index),
      textMessage,
      segments,
      toolCards,
      usage: tokenUsage(message)
    };
  }), { messages: messages.length }), [messages]);
  const blocks = useMemo(() => measureChatPerf('chat.tool_group.blocks', () => toolProcessBlocks(expandedRows), { rows: expandedRows.length }), [expandedRows]);
  const activeTail = active && !group.nextMessage;
  const lastToolBlockIndex = useMemo(() => {
    for (let index = blocks.length - 1; index >= 0; index -= 1) {
      if (blocks[index]?.type === 'tools') return index;
    }
    return -1;
  }, [blocks]);
  const hasFinalMessage = isFinalAssistantMessage(group.nextMessage);
  const toolsComplete = useMemo(() => measureChatPerf('chat.tool_group.complete_check', () => {
    const toolBlocks = blocks.filter((block) => block.type === 'tools');
    return toolBlocks.length > 0 && toolBlocks.every((block) => toolCardsAreComplete(block.cards));
  }, { blocks: blocks.length }), [blocks]);
  const waitingForNextItem = activeTail && toolsComplete && !hasFinalMessage;
  const complete = !activeTail && (hasFinalMessage || toolsComplete);
  const [open, setOpen] = useState(() => !hasFinalMessage);
  const bodyPresent = useDeferredPresence(open, 180);
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
  const elapsed = useToolRoundElapsed(messages, group.nextMessage, complete, active ? elapsedNowMs : undefined);
  const summary = useMemo(() => measureChatPerf('chat.tool_group.summary', () => toolRoundSummary(blocks), { blocks: blocks.length }), [blocks]);
  const title = toolRoundTitle(elapsed, complete, summary);
  useRenderCommitPerf('chat.tool_group.render_commit', renderStartedAt, () => ({
    messages: messages.length,
    blocks: blocks.length,
    active,
    open
  }));
  return (
    <section className={`tool-process-group${open ? ' open' : ''}${complete ? ' complete' : ''}${activeTail ? ' active' : ''}`}>
      <button className="tool-round-toggle" type="button" onClick={() => setOpen((value) => !value)}>
        <span>{title}</span>
        {summary.total > 0 && <em>{summary.label}</em>}
        <ChevronDown size={15} strokeWidth={1.9} aria-hidden="true" />
      </button>
      {!open && summary.total > 0 && (
        <div className="tool-round-compact" aria-hidden="true">
          {summary.names.slice(0, 4).map((name, index) => (
            <code key={`${name}-${index}`}>{name}</code>
          ))}
          {summary.extra > 0 && <code>+{summary.extra}</code>}
        </div>
      )}
      <div className="tool-round-separator" aria-hidden="true" />
      <div className="tool-round-body-shell" aria-hidden={!open}>
        <div className="tool-round-body-clip">
          {bodyPresent && (
            <div className="tool-process-round-body">
              {blocks.map((block, index) => {
                if (block.type === 'tools') {
                  const cardsComplete = toolCardsAreComplete(block.cards);
                  return (
                    <MemoToolProcessSegment
                      key={block.id}
                      block={block}
                      complete={complete || cardsComplete}
                      running={!cardsComplete && index === lastToolBlockIndex && activeTail}
                    />
                  );
                }
                return block.kind === 'reasoning'
                  ? <ReasoningNote key={block.id} text={block.text} collapsible defaultOpen={!complete && activeTail} live={!complete && activeTail} />
                  : <MemoMarkdownContent key={block.id} className="tool-note" text={block.text} attachments={block.attachments} plain={!complete && activeTail} />;
              })}
              {waitingForNextItem && <PendingAssistantPlaceholder compact label="正在思考" />}
            </div>
          )}
        </div>
      </div>
    </section>
  );
}

const MemoToolProcessGroup = memo(ToolProcessGroup, (previous, next) => (
  previous.active === next.active
  && previous.elapsedNowMs === next.elapsedNowMs
  && sameToolGroup(previous.group, next.group)
));

function useDeferredPresence(present, delayMs) {
  const [mounted, setMounted] = useState(present);
  useEffect(() => {
    if (present) {
      setMounted(true);
      return undefined;
    }
    const timer = window.setTimeout(() => setMounted(false), delayMs);
    return () => window.clearTimeout(timer);
  }, [delayMs, present]);
  return mounted;
}

function useToolRoundElapsed(messages, nextMessage, complete, nowMs) {
  const startMsRef = useRef(toolRoundStartMs(messages));
  const finalElapsedMsRef = useRef(null);
  const wasLiveRef = useRef(!complete);

  if (!complete) {
    wasLiveRef.current = true;
  } else if (wasLiveRef.current && finalElapsedMsRef.current === null) {
    finalElapsedMsRef.current = Math.max(0, Date.now() - startMsRef.current);
  }

  if (complete) {
    const elapsedMs = finalElapsedMsRef.current ?? toolRoundElapsedMs(messages, nextMessage);
    return elapsedMs !== null ? formatElapsedMs(elapsedMs) : '';
  }
  const liveNowMs = Number.isFinite(nowMs) ? nowMs : Date.now();
  return formatElapsedMs(Math.max(0, liveNowMs - startMsRef.current));
}

function toolRoundStartMs(messages) {
  const times = (messages || [])
    .map(messageTimeMs)
    .filter((value) => Number.isFinite(value));
  return times.length > 0 ? Math.min(...times) : Date.now();
}

function toolRoundTitle(elapsed, complete, summary = {}) {
  const prefix = complete ? '已处理' : '处理中';
  const detail = elapsed ? ` ${elapsed}` : '';
  if (summary.reasoning > 0 && summary.tools === 0) return `${prefix}${detail} · 思考`;
  if (summary.tools > 0) return `${prefix}${detail} · 工具`;
  return `${prefix}${detail}`;
}

function toolRoundSummary(blocks) {
  const names = [];
  let reasoning = 0;
  for (const block of blocks || []) {
    if (block?.kind === 'reasoning') reasoning += 1;
    if (block?.type !== 'tools') continue;
    for (const card of mergedToolCards(block.cards || [])) {
      const name = String(card.name || 'tool').trim() || 'tool';
      if (!names.includes(name)) names.push(name);
    }
  }
  const parts = [];
  if (reasoning > 0) parts.push(`${reasoning} 思考`);
  if (names.length > 0) parts.push(`${names.length} 工具`);
  return {
    reasoning,
    tools: names.length,
    names,
    extra: Math.max(0, names.length - 4),
    total: reasoning + names.length,
    label: parts.join(' · ')
  };
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
    const segments = row.segments || [{ notes: [], cards: row.toolCards }];
    const hasSegmentNotes = segments.some((segment) => Array.isArray(segment?.notes) && segment.notes.length > 0);
    if (text && !hasSegmentNotes) {
      pushNote({
        type: 'note',
        kind: 'text',
        id: `${row.id}-text`,
        text,
        attachments
      });
    }
    let renderedCardIndex = 0;
    segments.forEach((segment, segmentIndex) => {
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
  const [open, setOpen] = useState(() => Boolean(running));
  const manualOpenRef = useRef(false);
  const wasRunningRef = useRef(Boolean(running));
  const toolRows = useMemo(() => mergedToolCards(block.cards), [block.cards]);
  const firstName = useMemo(() => block.cards[0]?.name || 'tool', [block.cards]);
  const summary = useMemo(() => toolGroupSummary(block.cards, firstName), [block.cards, firstName]);
  const title = complete ? summary.doneTitle : summary.runningTitle;
  useEffect(() => {
    if (running && !manualOpenRef.current) {
      setOpen(true);
    }
    if (!running && wasRunningRef.current && complete && !manualOpenRef.current) {
      setOpen(false);
    }
    wasRunningRef.current = running;
  }, [complete, running]);
  return (
    <section className={`tool-process-segment${open ? ' open' : ''}${running ? ' running' : ''}`}>
      <button
        className="tool-process-toggle"
        type="button"
        onClick={() => {
          manualOpenRef.current = true;
          setOpen((value) => !value);
        }}
      >
        <TerminalSquare size={15} strokeWidth={1.9} aria-hidden="true" />
        <span>{title}</span>
        <ChevronDown size={15} strokeWidth={1.9} aria-hidden="true" />
      </button>
      {open && (
        <div className="tool-process-body">
          {toolRows.map((card) => (
            <MemoToolInlineCard
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

const MemoToolProcessSegment = memo(ToolProcessSegment, (previous, next) => (
  previous.complete === next.complete
  && previous.running === next.running
  && sameToolBlock(previous.block, next.block)
));

function artifactAttachmentForPath(path, attachments = []) {
  const cleanPath = String(path || '').trim();
  const existing = attachments.find((attachment) => attachmentMatchesPath(attachment, cleanPath));
  if (existing) return existing;
  const name = fileNameFromPath(cleanPath) || cleanPath;
  return {
    path: cleanPath,
    workspace_path: cleanPath,
    relative_path: cleanPath,
    name,
    media_type: mediaTypeForArtifactPath(cleanPath)
  };
}

function attachmentMatchesPath(attachment, path) {
  const target = normalizeArtifactPath(path);
  if (!target) return false;
  const candidates = [
    attachment?.path,
    attachment?.file_path,
    attachment?.workspace_path,
    attachment?.relative_path,
    attachment?.workspace_relative_path,
    attachment?.uri,
    attachment?.file_uri,
    attachment?.url,
    attachment?.file?.path,
    attachment?.file?.uri
  ];
  return candidates.some((candidate) => normalizeArtifactPath(candidate) === target);
}

function normalizeArtifactPath(value) {
  let path = String(value || '').trim();
  if (!path) return '';
  if (/^file:/i.test(path)) {
    try {
      path = decodeURIComponent(new URL(path).pathname);
    } catch {
      path = path.replace(/^file:\/\//i, '');
    }
  }
  return path.replace(/\\/g, '/').replace(/^\.\//, '').replace(/\/+/g, '/');
}

function mediaTypeForArtifactPath(path) {
  const ext = fileExtension(path);
  if (['png'].includes(ext)) return 'image/png';
  if (['jpg', 'jpeg'].includes(ext)) return 'image/jpeg';
  if (ext === 'gif') return 'image/gif';
  if (ext === 'webp') return 'image/webp';
  if (ext === 'svg') return 'image/svg+xml';
  if (['html', 'htm'].includes(ext)) return 'text/html';
  if (ext === 'pdf') return 'application/pdf';
  return '';
}

function uniqueAttachments(attachments) {
  const seen = new Set();
  const result = [];
  attachments.forEach((attachment) => {
    const key = attachmentIdentity(attachment);
    if (key && seen.has(key)) return;
    if (key) seen.add(key);
    result.push(attachment);
  });
  return result;
}

export function MessageBody({ message, onOpenAttachment, onDownloadAttachment, onResolveAttachmentUrl, onOpenLocalLink }) {
  const text = messageText(message);
  const structuredItems = messageItems(message);
  const plainStreaming = Boolean(message?._streaming && String(message?.role || '').toLowerCase() === 'assistant');
  const attachments = Array.isArray(message?.attachments) ? message.attachments : [];
  const files = Array.isArray(message?.files) ? message.files : [];
  const allAttachments = [...attachments, ...files];
  const inlineIndexes = markerIndexes(text);
  const inlineAttachmentKeys = new Set(
    Array.from(inlineIndexes)
      .map((index) => attachmentIdentity(allAttachments[index]))
      .filter(Boolean)
  );
  const structuredAttachmentIndexes = new Set(
    structuredItems
      .filter((item) => item?.type === 'file' && item.attachment_index !== undefined)
      .map((item) => Number(item.attachment_index))
      .filter((index) => Number.isFinite(index))
  );
  const structuredAttachmentKeys = new Set(
    structuredItems
      .filter((item) => item?.type === 'file')
      .map(attachmentIdentity)
      .filter(Boolean)
  );
  const trailingAttachments = allAttachments.filter((attachment, index) => {
    const attachmentIndex = Number(attachment?.index);
    const key = attachmentIdentity(attachment);
    return !inlineIndexes.has(index)
      && !inlineIndexes.has(attachmentIndex)
      && !(key && inlineAttachmentKeys.has(key))
      && !structuredAttachmentIndexes.has(index)
      && !structuredAttachmentIndexes.has(attachmentIndex)
      && !(key && structuredAttachmentKeys.has(key));
  });
  const displayTrailingAttachments = uniqueAttachments(trailingAttachments);
  return (
    <div className="message-body">
      {structuredItems.length > 0 ? (
        <StructuredItems role={message?.role} items={structuredItems} attachments={allAttachments} fallbackText={text} plain={plainStreaming} onOpenAttachment={onOpenAttachment} onDownloadAttachment={onDownloadAttachment} onResolveAttachmentUrl={onResolveAttachmentUrl} onOpenLocalLink={onOpenLocalLink} />
      ) : text ? (
        <MemoMarkdownContent className="message-text" text={text} attachments={allAttachments} plain={plainStreaming} onOpenAttachment={onOpenAttachment} onDownloadAttachment={onDownloadAttachment} onResolveAttachmentUrl={onResolveAttachmentUrl} onOpenLocalLink={onOpenLocalLink} />
      ) : null}
      {displayTrailingAttachments.length > 0 && <AttachmentList attachments={displayTrailingAttachments} onOpenAttachment={onOpenAttachment} onDownloadAttachment={onDownloadAttachment} onResolveAttachmentUrl={onResolveAttachmentUrl} />}
      {Number(message?.attachment_count || 0) > 0 && allAttachments.length === 0 && (
        <div className="message-attachments muted">正在加载附件...</div>
      )}
      {message?.tool_name && (
        <div className="tool-chip">{message.tool_name}</div>
      )}
    </div>
  );
}

function attachmentIdentity(attachment) {
  if (!attachment || typeof attachment !== 'object') return '';
  const file = attachment.file && typeof attachment.file === 'object' ? attachment.file : {};
  return String(
    attachment.uri
    || attachment.file_uri
    || attachment.url
    || attachment.path
    || file.uri
    || file.file_uri
    || file.url
    || file.path
    || attachment.name
    || attachment.filename
    || ''
  ).trim();
}

export function StructuredItems({ role, items, attachments, fallbackText, plain = false, onOpenAttachment, onDownloadAttachment, onResolveAttachmentUrl, onOpenLocalLink }) {
  const orderedItems = orderedStructuredItems(items, role);
  const hasTextItem = orderedItems.some(({ item }) => typeof item === 'string' || item?.type === 'text');
  const hasSelectionReference = orderedItems.some(({ item }) => item?.type === 'selection_reference');
  const fallback = String(fallbackText || '').trim();
  const rendered = orderedItems
    .map(({ item, index }) => {
      if (typeof item === 'string') {
        return <MemoMarkdownContent key={index} className="message-text" text={item} attachments={attachments} plain={plain} onOpenAttachment={onOpenAttachment} onDownloadAttachment={onDownloadAttachment} onResolveAttachmentUrl={onResolveAttachmentUrl} onOpenLocalLink={onOpenLocalLink} />;
      }
      if (item?.type === 'text') {
        return <MemoMarkdownContent key={index} className="message-text" text={item.text_with_attachment_markers || item.text || item.content || ''} attachments={attachments} plain={plain} onOpenAttachment={onOpenAttachment} onDownloadAttachment={onDownloadAttachment} onResolveAttachmentUrl={onResolveAttachmentUrl} onOpenLocalLink={onOpenLocalLink} />;
      }
      if (item?.type === 'file') {
        return <AttachmentCard key={index} attachment={attachments[item.attachment_index] || item} onOpenAttachment={onOpenAttachment} onDownloadAttachment={onDownloadAttachment} onResolveAttachmentUrl={onResolveAttachmentUrl} />;
      }
      if (item?.type === 'selection_reference') {
        return <SelectionReferenceCard key={index} selection={item.selection || item.payload || item} />;
      }
      if (item?.type === 'reasoning') {
        return <ReasoningNote key={index} text={item.text || item.summary || ''} />;
      }
      if (item?.type === 'tool_call' || item?.type === 'tool_result') {
        return (
          <MemoToolInlineCard
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
      <MemoMarkdownContent
        key="fallback-text"
        className="message-text"
        text={fallbackText}
        attachments={attachments}
        plain={plain}
        onOpenAttachment={onOpenAttachment}
        onDownloadAttachment={onDownloadAttachment}
        onResolveAttachmentUrl={onResolveAttachmentUrl}
        onOpenLocalLink={onOpenLocalLink}
      />
    );
  }
  if (rendered.length) return <>{rendered}</>;
  return <MemoMarkdownContent className="message-text" text={fallbackText} attachments={attachments} plain={plain} onOpenAttachment={onOpenAttachment} onDownloadAttachment={onDownloadAttachment} onResolveAttachmentUrl={onResolveAttachmentUrl} onOpenLocalLink={onOpenLocalLink} />;
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

function ReasoningNote({ text, collapsible = false, defaultOpen = true, live = false }) {
  const value = String(text || '').trim();
  const [open, setOpen] = useState(defaultOpen);
  useEffect(() => {
    if (live) setOpen(true);
  }, [live]);
  if (!value) return null;
  if (collapsible) {
    return (
      <div className={`reasoning-note collapsible${open ? ' open' : ''}${live ? ' live' : ''}`}>
        <button type="button" onClick={() => setOpen((current) => !current)}>
          <span>思考</span>
          <em>{open ? '收起' : shortReasoningSummary(value)}</em>
          <ChevronDown size={14} strokeWidth={1.9} aria-hidden="true" />
        </button>
        {open && <MemoMarkdownContent className="reasoning-note-text" text={value} plain={live} />}
      </div>
    );
  }
  return (
    <div className="reasoning-note">
      <span>思考</span>
      <MemoMarkdownContent className="reasoning-note-text" text={value} plain={live} />
    </div>
  );
}

function shortReasoningSummary(value) {
  const text = String(value || '').replace(/\s+/g, ' ').trim();
  if (!text) return '展开';
  return text.length > 72 ? `${text.slice(0, 72)}...` : text;
}

export function MarkdownContent({ text, attachments = [], className = 'markdown-content', plain = false, onOpenAttachment, onDownloadAttachment, onResolveAttachmentUrl, onOpenLocalLink }) {
  const renderStartedAt = renderCommitStart();
  const value = String(text || '');
  useRenderCommitPerf(plain ? 'chat.markdown.plain_commit' : 'chat.markdown.rich_commit', renderStartedAt, () => {
    if (!value.trim()) return null;
    return {
      className,
      chars: value.length,
      attachments: attachments.length
    };
  });
  if (!value.trim()) return null;
  if (plain) {
    return (
      <div className={`${className} plain-stream-text`}>
        <PlainTextBlock text={value} />
      </div>
    );
  }
  const parts = [];
  const pattern = /(\[\[attachment:(\d+)]]|\[tool_(call|result)\s+([^\]\n]+)\]\s*([\s\S]*?)(?=\n\[tool_(?:call|result)\s+|$))/gi;
  let cursor = 0;
  let match;
  while ((match = pattern.exec(value)) !== null) {
    const before = value.slice(cursor, match.index);
    if (before.trim()) {
      parts.push(<MarkdownBlock key={`text-${cursor}`} text={before} attachments={attachments} onOpenAttachment={onOpenAttachment} onDownloadAttachment={onDownloadAttachment} onResolveAttachmentUrl={onResolveAttachmentUrl} onOpenLocalLink={onOpenLocalLink} />);
    }
    if (match[2] !== undefined) {
      const attachment = attachments[Number(match[2])];
      if (attachment) {
        parts.push(<AttachmentCard key={`attachment-${match.index}`} attachment={attachment} inline onOpenAttachment={onOpenAttachment} onDownloadAttachment={onDownloadAttachment} onResolveAttachmentUrl={onResolveAttachmentUrl} />);
      }
    } else if (match[3]) {
      parts.push(
        <MemoToolInlineCard
          key={`tool-${match.index}`}
          kind={match[3].toLowerCase() === 'result' ? 'result' : 'call'}
          name={match[4].trim()}
          payload={match[5].trim()}
        />
      );
    }
    cursor = match.index + match[0].length;
  }
  const rest = value.slice(cursor);
  if (rest.trim()) {
    parts.push(<MarkdownBlock key={`text-${cursor}`} text={rest} attachments={attachments} onOpenAttachment={onOpenAttachment} onDownloadAttachment={onDownloadAttachment} onResolveAttachmentUrl={onResolveAttachmentUrl} onOpenLocalLink={onOpenLocalLink} />);
  }
  return <div className={className}>{parts.length ? parts : <MarkdownBlock text={value} attachments={attachments} onOpenAttachment={onOpenAttachment} onDownloadAttachment={onDownloadAttachment} onResolveAttachmentUrl={onResolveAttachmentUrl} onOpenLocalLink={onOpenLocalLink} />}</div>;
}

const MemoMarkdownContent = memo(MarkdownContent, (previous, next) => (
  previous.text === next.text
  && previous.attachments === next.attachments
  && previous.className === next.className
  && previous.plain === next.plain
  && previous.onOpenAttachment === next.onOpenAttachment
  && previous.onDownloadAttachment === next.onDownloadAttachment
  && previous.onResolveAttachmentUrl === next.onResolveAttachmentUrl
  && previous.onOpenLocalLink === next.onOpenLocalLink
));

function PlainTextBlock({ text }) {
  const value = String(text || '');
  const previousRef = useRef('');
  const previous = previousRef.current;
  const canAnimateAppend = previous && value.length > previous.length && value.startsWith(previous);
  const stableText = canAnimateAppend ? previous : '';
  const appendedText = canAnimateAppend ? value.slice(previous.length) : value;
  useEffect(() => {
    previousRef.current = value;
  }, [value]);
  if (!canAnimateAppend) {
    return <span className="stream-text-fade" key={value.length}>{value}</span>;
  }
  return (
    <>
      <span>{stableText}</span>
      <span className="stream-text-fade" key={value.length}>{appendedText}</span>
    </>
  );
}

function normalizeTexMathDelimiters(text) {
  const value = String(text || '');
  if (!value.includes('\\(') && !value.includes('\\)') && !value.includes('\\[') && !value.includes('\\]')) return value;
  const lines = value.split(/(\n)/);
  let inFence = null;
  const processInline = (line) => {
    let output = '';
    let inlineTicks = '';
    for (let index = 0; index < line.length;) {
      const tickMatch = line.slice(index).match(/^`+/);
      if (tickMatch) {
        const ticks = tickMatch[0];
        output += ticks;
        if (!inlineTicks) {
          inlineTicks = ticks;
        } else if (ticks.length === inlineTicks.length) {
          inlineTicks = '';
        }
        index += ticks.length;
        continue;
      }
      if (!inlineTicks) {
        const pair = line.slice(index, index + 2);
        if (pair === '\\(' || pair === '\\)') {
          output += '$';
          index += 2;
          continue;
        }
        if (pair === '\\[' || pair === '\\]') {
          output += '$$';
          index += 2;
          continue;
        }
      }
      output += line[index];
      index += 1;
    }
    return output;
  };

  return lines.map((part) => {
    if (part === '\n') return part;
    const fenceMatch = part.match(/^(\s*)(`{3,}|~{3,})/);
    if (inFence) {
      if (fenceMatch && fenceMatch[2][0] === inFence.marker && fenceMatch[2].length >= inFence.length) {
        inFence = null;
      }
      return part;
    }
    if (fenceMatch) {
      inFence = { marker: fenceMatch[2][0], length: fenceMatch[2].length };
      return part;
    }
    return processInline(part);
  }).join('');
}

export function MarkdownBlock({ text, attachments = [], onOpenAttachment, onDownloadAttachment, onResolveAttachmentUrl, onOpenLocalLink }) {
  const markdownText = useMemo(() => normalizeTexMathDelimiters(text), [text]);
  return (
    <ReactMarkdown
      remarkPlugins={[remarkGfm, remarkMath]}
      rehypePlugins={[rehypeHighlight, rehypeKatex]}
      components={{
        a: ({ node, ...props }) => {
          const href = String(props.href || '');
          const external = isExternalUrl(href);
          return (
            <a
              {...props}
              target={external ? '_blank' : undefined}
              rel={external ? 'noreferrer' : undefined}
              onClick={(event) => {
                if (external) {
                  handleExternalLinkClick(event, href);
                  return;
                }
                if (onOpenLocalLink?.(href)) {
                  event.preventDefault();
                  event.stopPropagation();
                }
              }}
            />
          );
        },
        img: ({ node, ...props }) => {
          const src = String(props.src || '').trim();
          if (src && !isExternalUrl(src) && !/^(?:data:|blob:)/i.test(src)) {
            return (
              <AttachmentCard
                attachment={artifactAttachmentForPath(src, attachments)}
                inline
                onOpenAttachment={onOpenAttachment}
                onDownloadAttachment={onDownloadAttachment}
                onResolveAttachmentUrl={onResolveAttachmentUrl}
              />
            );
          }
          return <img {...props} className="message-inline-image" loading="lazy" alt={props.alt || ''} />;
        }
      }}
    >
      {markdownText}
    </ReactMarkdown>
  );
}

export function AttachmentList({ attachments, onOpenAttachment, onDownloadAttachment, onResolveAttachmentUrl }) {
  return (
    <div className="message-attachments">
      {attachments.map((attachment, index) => (
        <AttachmentCard key={`${attachmentName(attachment)}-${attachment?.path || index}`} attachment={attachment} onOpenAttachment={onOpenAttachment} onDownloadAttachment={onDownloadAttachment} onResolveAttachmentUrl={onResolveAttachmentUrl} />
      ))}
    </div>
  );
}

function useResolvedAttachmentUrl(attachment, onResolveAttachmentUrl) {
  const rawUrl = attachmentUrl(attachment);
  const needsResolve = isResolvableLocalAttachmentUrl(rawUrl) || (!rawUrl && hasLocalAttachmentPath(attachment));
  const initialUrl = needsResolve ? '' : rawUrl;
  const [url, setUrl] = useState(initialUrl);
  useEffect(() => {
    let disposed = false;
    const nextRawUrl = attachmentUrl(attachment);
    const shouldResolve = isResolvableLocalAttachmentUrl(nextRawUrl) || (!nextRawUrl && hasLocalAttachmentPath(attachment));
    setUrl(shouldResolve ? '' : nextRawUrl);
    if (!shouldResolve || !onResolveAttachmentUrl) return undefined;
    Promise.resolve(onResolveAttachmentUrl(attachment, nextRawUrl))
      .then((resolvedUrl) => {
        if (!disposed) setUrl(resolvedUrl || nextRawUrl || '');
      })
      .catch(() => {
        if (!disposed) setUrl(nextRawUrl || '');
      });
    return () => {
      disposed = true;
    };
  }, [attachment, onResolveAttachmentUrl]);
  return url;
}

function isResolvableLocalAttachmentUrl(value) {
  const url = String(value || '').trim();
  if (!url) return false;
  if (/^(?:data:|blob:|https?:)/i.test(url)) return false;
  if (/^file:/i.test(url)) return true;
  return url.startsWith('/') || /^[A-Za-z]:[\\/]/.test(url);
}

function hasLocalAttachmentPath(attachment) {
  const path = String(attachment?.path || attachment?.file_path || '').trim();
  return Boolean(path && !/^(?:data:|blob:|https?:)/i.test(path));
}

export function AttachmentCard({ attachment, inline = false, onOpenAttachment, onDownloadAttachment, onResolveAttachmentUrl }) {
  const name = attachmentName(attachment);
  const url = useResolvedAttachmentUrl(attachment, onResolveAttachmentUrl);
  const size = formatBytes(attachment?.size_bytes || attachment?.size);
  const [loadedImageSize, setLoadedImageSize] = useState(null);
  const [imageFailed, setImageFailed] = useState(false);
  const hasAttachmentLocation = Boolean(attachment?.path || attachment?.file_path || attachment?.uri || attachment?.file_uri || attachment?.url);
  const canOpen = Boolean(onOpenAttachment && hasAttachmentLocation);
  const canDownload = Boolean(onDownloadAttachment && hasAttachmentLocation);
  const openAttachment = () => {
    if (canOpen) onOpenAttachment(attachment);
  };
  const downloadAttachment = (event) => {
    event.stopPropagation();
    if (canDownload) onDownloadAttachment(attachment);
  };
  const meta = attachmentMeta(attachment, name);
  useEffect(() => {
    setImageFailed(false);
    setLoadedImageSize(null);
  }, [url]);
  if (inline && isHtmlAttachment(attachment)) {
    return <HtmlAttachmentCard attachment={attachment} name={name} url={url} canOpen={canOpen} canDownload={canDownload} openAttachment={openAttachment} downloadAttachment={downloadAttachment} />;
  }
  if (isImageAttachment(attachment)) {
    const imageWidth = attachmentImageDisplayWidth(attachment, loadedImageSize);
    const imageStyle = imageWidth ? { '--attachment-image-width': `${imageWidth}px` } : undefined;
    const showImage = Boolean(url && !imageFailed);
    return (
      <div
        className={`message-attachment image${inline ? ' inline' : ''}${showImage ? '' : ' loading'}${canOpen ? ' clickable' : ''}`}
        style={imageStyle}
        title={canOpen ? `预览 ${name}` : name}
      >
        <button className="attachment-image-preview" type="button" onClick={openAttachment} disabled={!canOpen}>
          {showImage ? (
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
            onError={() => setImageFailed(true)}
          />
          ) : <span className="image-placeholder">{imageFailed ? '无法加载图片' : '正在加载图片'}</span>}
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
      className={`message-attachment file${inline ? ' inline' : ''}${canOpen ? ' clickable' : ''}`}
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

function HtmlAttachmentCard({ attachment, name, url, canOpen, canDownload, openAttachment, downloadAttachment }) {
  const [mode, setMode] = useState('render');
  const source = htmlAttachmentSource(attachment, url);
  const renderUrl = htmlRenderableAttachmentUrl(attachment, url);
  const hasSource = source.trim().length > 0;
  useEffect(() => {
    setMode('render');
  }, [name, url, source]);
  return (
    <div
      className={`message-attachment html inline${renderUrl || hasSource ? '' : ' loading'}${canOpen ? ' clickable' : ''}`}
      title={canOpen ? `预览 ${name}` : name}
    >
      <div className="attachment-html-toolbar">
        <span>{name}</span>
        <div className="attachment-html-tabs" role="tablist" aria-label="HTML attachment preview mode">
          <button
            className={mode === 'render' ? 'active' : ''}
            type="button"
            role="tab"
            aria-selected={mode === 'render'}
            onClick={(event) => {
              event.stopPropagation();
              setMode('render');
            }}
          >
            <Eye size={12} />
            预览
          </button>
          <button
            className={mode === 'source' ? 'active' : ''}
            type="button"
            role="tab"
            aria-selected={mode === 'source'}
            onClick={(event) => {
              event.stopPropagation();
              setMode('source');
            }}
          >
            <Code2 size={12} />
            源码
          </button>
        </div>
        <AttachmentOpenMenu
          name={name}
          canOpen={canOpen}
          canDownload={canDownload}
          onOpen={openAttachment}
          onDownload={downloadAttachment}
        />
      </div>
      <div className="attachment-html-body">
        {mode === 'render' ? (
          hasSource || renderUrl ? (
            <iframe
              title={name}
              src={hasSource ? undefined : renderUrl}
              srcDoc={hasSource ? source : undefined}
              sandbox="allow-scripts allow-forms"
              loading="lazy"
              referrerPolicy="no-referrer"
            />
          ) : (
            <div className="attachment-html-placeholder">正在加载 HTML 预览</div>
          )
        ) : hasSource ? (
          <pre><code>{source}</code></pre>
        ) : (
          <div className="attachment-html-placeholder">没有可显示的 HTML 源码</div>
        )}
      </div>
    </div>
  );
}

function htmlAttachmentSource(attachment, url) {
  const direct = String(
    attachment?.text
    || attachment?.content
    || (attachment?.encoding === 'utf8' ? attachment?.data : '')
    || attachment?.html
    || ''
  );
  if (direct.trim()) return direct;
  const preview = attachment?.preview;
  if (preview && typeof preview === 'object') {
    const previewText = String(preview.text || preview.content || (preview.encoding === 'utf8' ? preview.data : '') || '');
    if (previewText.trim()) return previewText;
  }
  return dataHtmlFromUrl(url);
}

function htmlRenderableAttachmentUrl(attachment, url) {
  const raw = String(url || attachment?.url || attachment?.uri || attachment?.file_uri || '').trim();
  return /^(https?:|data:text\/html|blob:)/i.test(raw) ? raw : '';
}

function dataHtmlFromUrl(url) {
  const value = String(url || '');
  const match = value.match(/^data:text\/html(?:;charset=[^;,]+)?(;base64)?,(.*)$/i);
  if (!match) return '';
  const body = match[2] || '';
  try {
    return match[1] ? atob(body) : decodeURIComponent(body);
  } catch {
    return '';
  }
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

function isHtmlAttachment(attachment) {
  const mediaType = String(attachment?.media_type || attachment?.mime_type || attachment?.mime || '').toLowerCase();
  const name = attachmentName(attachment);
  return mediaType === 'text/html' || ['html', 'htm'].includes(fileExtension(name || attachment?.path || attachment?.uri || ''));
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

function parseToolPayload(payload, options = {}) {
  if (!payload) return {};
  if (typeof payload === 'object') return payload;
  const value = String(payload || '').trim();
  if (!value) return {};
  const maxJsonChars = Number(options.maxJsonChars ?? Number.POSITIVE_INFINITY);
  if (Number.isFinite(maxJsonChars) && value.length > maxJsonChars) {
    return { text: value };
  }
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
  const lowerName = String(name || '').toLowerCase();
  const isResult = kind === 'result';
  const payloadText = typeof payload === 'string' ? payload : '';
  if (lowerName.includes('edit') || lowerName.includes('write') || lowerName.includes('patch')) {
    const data = isResult ? parseToolPayload(payload, { maxJsonChars: 8000 }) : lightToolPayload(payload);
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
  const data = payloadText.length > 12000
    ? lightToolPayload(payload)
    : parseToolPayload(payload);
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

function lightToolPayload(payload) {
  if (!payload || typeof payload === 'object') return payload || {};
  const text = String(payload || '');
  const result = {};
  const sample = text.slice(0, 8192);
  [
    ['path', /"path"\s*:\s*"([^"\\]*(?:\\.[^"\\]*)*)"/],
    ['file_path', /"file_path"\s*:\s*"([^"\\]*(?:\\.[^"\\]*)*)"/],
    ['file', /"file"\s*:\s*"([^"\\]*(?:\\.[^"\\]*)*)"/],
    ['command', /"command"\s*:\s*"([^"\\]*(?:\\.[^"\\]*)*)"/],
    ['cmd', /"cmd"\s*:\s*"([^"\\]*(?:\\.[^"\\]*)*)"/],
    ['url', /"url"\s*:\s*"([^"\\]*(?:\\.[^"\\]*)*)"/],
    ['query', /"query"\s*:\s*"([^"\\]*(?:\\.[^"\\]*)*)"/],
    ['pattern', /"pattern"\s*:\s*"([^"\\]*(?:\\.[^"\\]*)*)"/]
  ].forEach(([key, pattern]) => {
    const match = sample.match(pattern);
    if (!match) return;
    try {
      result[key] = JSON.parse(`"${match[1]}"`);
    } catch {
      result[key] = match[1];
    }
  });
  if (!Object.keys(result).length) {
    result.text = compactToolSummary(text, 180);
  }
  return result;
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
  const renderStartedAt = renderCommitStart();
  const [open, setOpen] = useState(false);
  const display = useMemo(() => measureChatPerf('chat.tool_card.display', () => toolDisplay(kind, name, payload), {
    kind,
    name,
    payloadChars: typeof payload === 'string' ? payload.length : 0
  }), [kind, name, payload]);
  const hasMergedPayload = callPayload !== undefined || resultPayload !== undefined;
  useRenderCommitPerf('chat.tool_card.render_commit', renderStartedAt, () => ({
      kind,
      name,
      open,
      running,
      payloadChars: typeof payload === 'string' ? payload.length : 0
  }));
  return (
    <details
      className={`tool-inline-card ${kind}${running ? ' running' : ''}`}
      open={open}
      onToggle={(event) => setOpen(event.currentTarget.open)}
    >
      <summary>
        <span>{display.title}</span>
        <code>{display.chip}</code>
        <em>{display.summary}</em>
        <InlineTokenUsage usage={usage} />
      </summary>
      {open && (hasMergedPayload ? (
        <MergedToolDetail name={name} callPayload={callPayload} resultPayload={resultPayload} running={running} />
      ) : (
        running
          ? <StreamingToolPayloadDetail title={display.detailTitle} payload={payload} />
          : <ToolDetail title={display.detailTitle} name={name} payload={payload} />
      ))}
    </details>
  );
}

const MemoToolInlineCard = memo(ToolInlineCard, (previous, next) => (
  previous.kind === next.kind
  && previous.name === next.name
  && previous.payload === next.payload
  && previous.callPayload === next.callPayload
  && previous.resultPayload === next.resultPayload
  && previous.running === next.running
  && sameUsage(previous.usage, next.usage)
));

function MergedToolDetail({ name, callPayload, resultPayload, running = false }) {
  return (
    <div className="merged-tool-detail">
      {callPayload !== undefined ? (
        running && resultPayload === undefined
          ? <StreamingToolPayloadDetail title="调用参数" payload={callPayload} />
          : <ToolDetail title="调用参数" name={name} payload={callPayload} />
      ) : null}
      {resultPayload !== undefined ? <ToolDetail title="工具结果" name={name} payload={resultPayload} /> : null}
    </div>
  );
}

function StreamingToolPayloadDetail({ title, payload }) {
  const value = typeof payload === 'string' ? payload : JSON.stringify(payload ?? '', null, 2);
  return (
    <div className="tool-detail">
      <strong>{title}</strong>
      <pre><code>{value}</code></pre>
    </div>
  );
}

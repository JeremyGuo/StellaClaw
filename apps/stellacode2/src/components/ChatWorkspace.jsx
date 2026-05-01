import { Fragment, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import ReactMarkdown from 'react-markdown';
import rehypeHighlight from 'rehype-highlight';
import remarkGfm from 'remark-gfm';
import { FileText, Plus, Send, TerminalSquare } from 'lucide-react';
import * as Popover from '@radix-ui/react-popover';
import { attachmentName, attachmentUrl, isImageAttachment, messageText } from '../lib/fileUtils';
import { formatBytes, formatTokens, modelAlias, modelDisplayName } from '../lib/format';
import { displayMessages, firstMessageId, firstToolNameForMessage, liveActivitySignature, markerIndexes, messageIndex, messageKey, shouldTypewriterMessage, splitMessageForDisplay, tokenUsage, toolCardsForMessage } from '../lib/messageUtils';

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

export function ChatWorkspace({ conversationKey: activeMessageScope, modelSelectionPending = false, messages, messagesReady, draft, setDraft, mode, hasOlder, onLoadOlder, onSend, onLoadModels, sending, runningActivities }) {
  const renderedMessages = useMemo(() => displayMessages(messages), [messages]);
  const activitySignature = useMemo(() => liveActivitySignature(runningActivities || []), [runningActivities]);
  const oldestMessageKey = useMemo(() => firstMessageId(messages) || messages[0]?.id || messages[0]?.index || '', [messages]);
  const modeLabel = typeof mode === 'string' ? mode : mode?.label || '本地';
  const modeTone = typeof mode === 'string' ? '' : mode?.tone || 'local';
  const modeTitle = typeof mode === 'string' ? mode : mode?.title || modeLabel;
  const [typingKeys, setTypingKeys] = useState(() => new Set());
  const [commandPanel, setCommandPanel] = useState('commands');
  const [models, setModels] = useState([]);
  const [modelsLoading, setModelsLoading] = useState(false);
  const [modelsError, setModelsError] = useState('');
  const progressRef = useRef(null);
  const composerRef = useRef(null);
  const composingRef = useRef(false);
  const compositionEndedAtRef = useRef(0);
  const scrollRef = useRef(null);
  const previousCountRef = useRef(0);
  const loadingOlderRef = useRef(false);
  const prependAdjustRef = useRef(null);
  const stickToBottomRef = useRef(true);
  const knownMessagesRef = useRef(new Set());
  const typedMessagesRef = useRef(new Set());
  const typewriterHydratedRef = useRef(false);
  const newestSeenIndexRef = useRef(-1);
  const currentActivity = (runningActivities || []).at(-1) || null;
  const currentPlan = normalizeActivityPlan(currentActivity?.plan);
  const progressVisible = Boolean(currentActivity);

  useLayoutEffect(() => {
    const node = progressRef.current;
    if (!node || !progressVisible) {
      scrollRef.current?.style.setProperty('--progress-height', '0px');
      return undefined;
    }
    const update = () => {
      scrollRef.current?.style.setProperty('--progress-height', `${Math.ceil(node.getBoundingClientRect().height)}px`);
    };
    update();
    const observer = new ResizeObserver(update);
    observer.observe(node);
    return () => observer.disconnect();
  }, [activitySignature, progressVisible]);

  useLayoutEffect(() => {
    const node = composerRef.current;
    if (!node) return undefined;
    const update = () => {
      const root = node.closest('.chat-workspace');
      const composer = node.querySelector('.composer');
      root?.style.setProperty('--composer-wrap-height', `${Math.ceil(node.getBoundingClientRect().height)}px`);
      if (composer) {
        root?.style.setProperty('--composer-card-height', `${Math.ceil(composer.getBoundingClientRect().height)}px`);
      }
    };
    update();
    const observer = new ResizeObserver(update);
    observer.observe(node);
    return () => observer.disconnect();
  }, []);

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
  }, [renderedMessages.length, messages.length, activitySignature]);

  useEffect(() => {
    knownMessagesRef.current = new Set();
    typedMessagesRef.current = new Set();
    typewriterHydratedRef.current = false;
    newestSeenIndexRef.current = -1;
    setTypingKeys(new Set());
  }, [activeMessageScope]);

  useEffect(() => {
    const known = knownMessagesRef.current;
    const maxIndex = messages.reduce((max, message) => Math.max(max, messageIndex(message)), -1);
    if (!messagesReady || !typewriterHydratedRef.current) {
      messages.forEach((message, index) => {
        const key = messageKey(message, index);
        known.add(key);
        if (shouldTypewriterMessage(message)) {
          typedMessagesRef.current.add(`${key}:${messageText(message)}`);
        }
      });
      newestSeenIndexRef.current = maxIndex;
      typewriterHydratedRef.current = Boolean(messagesReady);
      return;
    }

    const nextTyping = [];
    const previousNewestIndex = newestSeenIndexRef.current;
    messages.forEach((message, index) => {
      const key = messageKey(message, index);
      const signature = `${key}:${messageText(message)}`;
      const isNew = !known.has(key);
      known.add(key);
      const isAppendedCurrentMessage = messageIndex(message) > previousNewestIndex;
      if (isNew && isAppendedCurrentMessage && shouldTypewriterMessage(message) && !typedMessagesRef.current.has(signature)) {
        typedMessagesRef.current.add(signature);
        nextTyping.push(key);
      }
    });
    newestSeenIndexRef.current = Math.max(newestSeenIndexRef.current, maxIndex);

    if (nextTyping.length > 0) {
      setTypingKeys((current) => {
        const next = new Set(current);
        nextTyping.forEach((key) => next.add(key));
        return next;
      });
    }
  }, [messages, activeMessageScope, messagesReady]);

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

  const submitDraft = () => {
    if (!draft.trim() || sending) return;
    onSend?.(draft);
  };

  const isImeComposingEnter = (event) => {
    if (event.key !== 'Enter') return false;
    const nativeEvent = event.nativeEvent || {};
    const recentlyEnded = Date.now() - compositionEndedAtRef.current < 80;
    return composingRef.current
      || event.isComposing
      || nativeEvent.isComposing
      || nativeEvent.keyCode === 229
      || recentlyEnded;
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
          renderedMessages.map((message, index) => (
            message.type === 'toolGroup'
              ? <ToolProcessGroup key={message.id} group={message} />
              : (
                <MessageArticle
                  key={messageKey(message, index)}
                  message={message}
                  typewriter={typingKeys.has(messageKey(message, index))}
                  onTypewriterDone={() => {
                    const key = messageKey(message, index);
                    setTypingKeys((current) => {
                      if (!current.has(key)) return current;
                      const next = new Set(current);
                      next.delete(key);
                      return next;
                    });
                  }}
                />
              )
          ))
        )}
      </div>
      <LiveActivityStack activities={runningActivities} progressRef={progressRef} />
      <footer className={`composer-wrap${progressVisible ? ' with-progress' : ''}`} ref={composerRef}>
        <div className="composer">
          <textarea
            value={draft}
            onChange={(event) => setDraft(event.target.value)}
            disabled={modelSelectionPending}
            onCompositionStart={() => {
              composingRef.current = true;
            }}
            onCompositionEnd={() => {
              composingRef.current = false;
              compositionEndedAtRef.current = Date.now();
            }}
            onKeyDown={(event) => {
              if (event.key === 'Enter' && !event.shiftKey && isImeComposingEnter(event)) {
                return;
              }
              if (event.key === 'Enter' && !event.shiftKey) {
                event.preventDefault();
                submitDraft();
              }
            }}
            placeholder={modelSelectionPending ? '请先选择模型' : 'Ask Stellacode to change, inspect, or explain...'}
          />
          <div className="composer-row">
            <button className="composer-icon" type="button" title="添加附件">
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
            <button className="send-button" type="button" disabled={modelSelectionPending || !draft.trim() || sending} onClick={submitDraft}>
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
  return (
    <section className={`session-progress-card${plan ? '' : ' compact'}`} aria-live="polite" ref={progressRef}>
      {plan ? <ActivityPlanPanel plan={plan} /> : <ActivityStatus activity={current} />}
    </section>
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
    <details className="activity-plan-panel" open>
      <summary>
        <span>计划</span>
        <code>共 {plan.total} 项，已完成 {plan.completed} 项</code>
      </summary>
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
    </details>
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

export function MessageArticle({ message, typewriter = false, onTypewriterDone }) {
  const usage = tokenUsage(message);
  const role = message.user_name || message.role || 'assistant';
  return (
    <article className={`message ${message.role || 'assistant'}${message._forceSeparate ? ' force-separate' : ''}`}>
      <div className="message-role">
        <span>{role}</span>
        {Array.isArray(message._auxiliary) && message._auxiliary.length > 0 && (
          <AuxiliaryDots messages={message._auxiliary} />
        )}
      </div>
      <MessageBody message={message} typewriter={typewriter} onTypewriterDone={onTypewriterDone} />
      {message.pending && <div className="message-status">正在发送...</div>}
      {message.error && <div className="message-status error">{message.error}</div>}
      {String(message.role || '').toLowerCase() === 'assistant' && (
        <TokenUsage usage={usage} />
      )}
    </article>
  );
}

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
  return (
    <Popover.Root>
      <div className="token-usage">
        <Popover.Trigger asChild>
          <button className="token-dot" type="button" aria-label="查看 Token Usage" />
        </Popover.Trigger>
        <span>{formatTokens(usage.total)}</span>
      </div>
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

export function ToolProcessGroup({ group }) {
  const [open, setOpen] = useState(false);
  const messages = group.messages || [];
  const expandedRows = useMemo(() => messages.map((message, index) => {
    const { textMessage, toolCards } = splitMessageForDisplay(message);
    return {
      id: messageKey(message, index),
      textMessage,
      toolCards,
      usage: tokenUsage(message)
    };
  }), [messages]);
  const cards = useMemo(() => expandedRows.flatMap((row) => row.toolCards), [expandedRows]);
  const firstName = useMemo(() => firstToolNameForMessage(messages[0]), [messages]);
  const summary = useMemo(() => toolGroupSummary(cards, messages, firstName), [cards, messages, firstName]);
  const done = useMemo(() => Boolean(group.nextMessage) || toolCardsAreComplete(cards), [cards, group.nextMessage]);
  const shouldAutoCollapse = Boolean(group.nextMessage);
  const elapsed = '';
  useEffect(() => {
    setOpen(!shouldAutoCollapse);
  }, [shouldAutoCollapse]);
  return (
    <details className="tool-process-group" open={open} onToggle={(event) => setOpen(event.currentTarget.open)}>
      <summary>
        <span>{done ? summary.doneTitle : summary.runningTitle}{elapsed}</span>
      </summary>
      {open && (
        <div className="tool-process-body">
          {expandedRows.map((row) => {
            const attachments = row.textMessage ? [...(row.textMessage.attachments || []), ...(row.textMessage.files || [])] : [];
            const text = row.textMessage ? messageText(row.textMessage) : '';
            return (
              <Fragment key={row.id}>
                {text && <MarkdownContent className="tool-note" text={text} attachments={attachments} />}
                {row.toolCards.map((card, index) => {
                  const showRowUsage = index === row.toolCards.length - 1;
                  const cardKey = `${row.id}-${index}`;
                  return (
                    <ToolInlineCard
                      key={cardKey}
                      kind={card.kind}
                      name={card.name}
                      payload={card.payload}
                      usage={showRowUsage ? row.usage : null}
                    />
                  );
                })}
              </Fragment>
            );
          })}
        </div>
      )}
    </details>
  );
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

export function MessageBody({ message, typewriter = false, onTypewriterDone }) {
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
      {typewriter && text ? (
        <TypewriterMarkdown className="message-text" text={text} attachments={allAttachments} onDone={onTypewriterDone} />
      ) : Array.isArray(message?.items) && message.items.length > 0 ? (
        <StructuredItems items={message.items} attachments={allAttachments} fallbackText={text} />
      ) : text ? (
        <MarkdownContent className="message-text" text={text} attachments={allAttachments} />
      ) : (
        <div className="message-text muted">空消息</div>
      )}
      {trailingAttachments.length > 0 && <AttachmentList attachments={trailingAttachments} />}
      {Number(message?.attachment_count || 0) > 0 && allAttachments.length === 0 && (
        <div className="message-attachments muted">正在加载附件...</div>
      )}
      {message?.tool_name && (
        <div className="tool-chip">{message.tool_name}</div>
      )}
    </div>
  );
}

export function StructuredItems({ items, attachments, fallbackText }) {
  const rendered = items
    .map((item, index) => {
      if (typeof item === 'string') {
        return <MarkdownContent key={index} className="message-text" text={item} attachments={attachments} />;
      }
      if (item?.type === 'text') {
        return <MarkdownContent key={index} className="message-text" text={item.text_with_attachment_markers || item.text || item.content || ''} attachments={attachments} />;
      }
      if (item?.type === 'file') {
        return <AttachmentCard key={index} attachment={attachments[item.attachment_index] || item} />;
      }
      if (item?.type === 'tool_call' || item?.type === 'tool_result') {
        return (
          <ToolInlineCard
            key={index}
            kind={item.type === 'tool_result' ? 'result' : 'call'}
            name={item.tool_name || 'tool'}
            payload={item.arguments || item.context_with_attachment_markers || item.context || item.result || ''}
          />
        );
      }
      return null;
    })
    .filter(Boolean);
  if (rendered.length) return <>{rendered}</>;
  return <MarkdownContent className="message-text" text={fallbackText} attachments={attachments} />;
}

export function TypewriterMarkdown({ text, attachments = [], className = 'message-text', onDone }) {
  const value = String(text || '');
  const [count, setCount] = useState(0);
  const doneRef = useRef(false);

  useEffect(() => {
    setCount(0);
    doneRef.current = false;
  }, [value]);

  useEffect(() => {
    if (!value) return undefined;
    let frame = 0;
    let cancelled = false;
    const total = value.length;
    const step = Math.max(2, Math.ceil(total / 80));
    const tick = () => {
      if (cancelled) return;
      setCount((current) => {
        const next = Math.min(total, current + step);
        if (next >= total && !doneRef.current) {
          doneRef.current = true;
          window.setTimeout(() => onDone?.(), 80);
        }
        return next;
      });
      frame = window.setTimeout(tick, 18);
    };
    frame = window.setTimeout(tick, 18);
    return () => {
      cancelled = true;
      window.clearTimeout(frame);
    };
  }, [value, onDone]);

  return (
    <div className="typewriter-message">
      <MarkdownContent className={className} text={value.slice(0, count)} attachments={attachments} />
      {count < value.length && <span className="typewriter-caret" aria-hidden="true" />}
    </div>
  );
}

export function MarkdownContent({ text, attachments = [], className = 'markdown-content' }) {
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
        parts.push(<AttachmentCard key={`attachment-${match.index}`} attachment={attachment} inline />);
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
        a: ({ node, ...props }) => <a {...props} target="_blank" rel="noreferrer" />,
        img: ({ node, ...props }) => <img {...props} className="message-inline-image" loading="lazy" alt={props.alt || ''} />
      }}
    >
      {text}
    </ReactMarkdown>
  );
}

export function AttachmentList({ attachments }) {
  return (
    <div className="message-attachments">
      {attachments.map((attachment, index) => (
        <AttachmentCard key={`${attachmentName(attachment)}-${attachment?.path || index}`} attachment={attachment} />
      ))}
    </div>
  );
}

export function AttachmentCard({ attachment, inline = false }) {
  const name = attachmentName(attachment);
  const url = attachmentUrl(attachment);
  const size = formatBytes(attachment?.size_bytes || attachment?.size);
  if (isImageAttachment(attachment)) {
    return (
      <button className={`message-attachment image${inline ? ' inline' : ''}${url ? '' : ' loading'}`} type="button">
        {url ? <img src={url} alt={name} loading="lazy" /> : <span className="image-placeholder">正在加载图片</span>}
        <span>{name}</span>
      </button>
    );
  }
  return (
    <button className="message-attachment file" type="button">
      <span className="attachment-file-icon"><FileText size={14} /></span>
      <span>{name}</span>
      {size && <small>{size}</small>}
    </button>
  );
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

function toolDisplay(kind, name, payload) {
  const data = parseToolPayload(payload);
  const lowerName = String(name || '').toLowerCase();
  const isResult = kind === 'result';
  if (lowerName.includes('shell')) {
    const command = data.command || data.cmd || data.text || '';
    return {
      title: isResult ? '已运行' : '运行',
      chip: 'shell',
      summary: command || 'shell command',
      detailTitle: 'Shell'
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

function compactFileName(path) {
  return String(path || '').split('/').filter(Boolean).at(-1) || String(path || '');
}

function diffLabel(data) {
  const added = data.added ?? data.additions ?? data.lines_added ?? data.bytes_written;
  const removed = data.removed ?? data.deletions ?? data.lines_removed;
  if (added === undefined && removed === undefined) return '';
  return `+${Number(added || 0)} -${Number(removed || 0)}`;
}

function toolGroupSummary(cards, messages, fallbackName) {
  const names = cards.map((card) => String(card.name || '').toLowerCase());
  const editLike = names.some((name) => name.includes('edit') || name.includes('write') || name.includes('patch'));
  const readLike = names.some((name) => name.includes('read'));
  const searchLike = names.some((name) => name.includes('search') || name.includes('grep') || name === 'rg');
  const shellLike = names.some((name) => name.includes('shell'));
  const fileRows = [];
  const seen = new Set();
  cards.forEach((card) => {
    const data = parseToolPayload(card.payload);
    const lowerName = String(card.name || '').toLowerCase();
    const path = data.path || data.file_path || data.file || data.target || '';
    if (!path) return;
    const isEdit = lowerName.includes('edit') || lowerName.includes('write') || lowerName.includes('patch');
    const isRead = lowerName.includes('read');
    if (!isEdit && !isRead) return;
    const key = `${isEdit ? 'edit' : 'read'}:${path}`;
    if (seen.has(key)) return;
    seen.add(key);
    fileRows.push({
      action: isEdit ? '已编辑' : '已读取',
      path: compactFileName(path),
      diff: isEdit ? diffLabel(data) : ''
    });
  });
  const baseName = editLike ? '文件'
    : searchLike ? '搜索'
      : shellLike ? '命令'
        : readLike ? '文件'
          : fallbackName || '工具';
  const doneTitle = editLike && fileRows.length
    ? '已编辑文件'
    : searchLike
      ? '已搜索'
      : shellLike
        ? `已运行 ${fallbackName || '命令'}`
        : `已处理 · ${baseName}`;
  const runningTitle = editLike && fileRows.length
    ? '正在编辑文件'
    : searchLike
      ? `正在搜索`
      : shellLike
        ? `正在运行 ${fallbackName || '命令'}`
        : `正在处理 · ${baseName}`;
  return {
    doneTitle,
    runningTitle,
    fileRows
  };
}

function ToolDetail({ title, payload }) {
  const data = parseToolPayload(payload);
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

export function ToolInlineCard({ kind, name, payload, usage }) {
  const display = toolDisplay(kind, name, payload);
  return (
    <details className={`tool-inline-card ${kind}`}>
      <summary>
        <span>{display.title}</span>
        <code>{display.chip}</code>
        <em>{display.summary}</em>
        <InlineTokenUsage usage={usage} />
        <i className="tool-detail-dot" aria-hidden="true" />
      </summary>
      <ToolDetail title={display.detailTitle} payload={payload} />
    </details>
  );
}

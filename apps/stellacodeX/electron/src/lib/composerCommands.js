export function slashCommandState(value) {
  const command = String(value || '').trim();
  const name = command.split(/\s+/, 1)[0]?.toLowerCase() || '';
  if (name === '/model') {
    return { control: true, name, title: '切换模型', detail: '模型切换命令已发送' };
  }
  if (name === '/remote') {
    return { control: true, name, title: '切换远程模式', detail: '远程模式命令已发送' };
  }
  if (name === '/reasoning') {
    return { control: true, name, title: '切换推理强度', detail: 'reasoning effort 命令已发送' };
  }
  if (name === '/cancel') {
    return { control: true, name, title: '取消执行', detail: '取消命令已发送' };
  }
  if (name === '/compact') {
    return { control: true, name, title: '压缩上下文', detail: '压缩命令已发送' };
  }
  if (name === '/status') {
    return { control: true, name, title: '读取状态', detail: '状态命令已发送' };
  }
  return { control: false, name, title: '等待响应', detail: '消息已送达，等待模型开始处理' };
}

export function controlCommandActivity(command) {
  const name = String(command || '').trim().split(/\s+/, 1)[0]?.toLowerCase() || '';
  if (name === '/model') return '模型切换命令已发送';
  if (name === '/reasoning') return 'Reasoning 命令已发送';
  return 'Conversation 设置命令已发送';
}

export function composerModeInfo(status) {
  const remote = String(status?.remote || status?.tool_remote_mode || '').trim();
  const fixedRemote = remote.match(/^fixed ssh `([^`]*)` `([^`]*)`/);
  if (fixedRemote) {
    const host = fixedRemote[1];
    const cwd = fixedRemote[2];
    return {
      label: '远程',
      tone: 'remote',
      title: cwd ? `工具运行在 ${host}:${cwd}` : `工具运行在 ${host}`
    };
  }
  if (remote && remote !== 'selectable') {
    return { label: '远程', tone: 'remote', title: remote };
  }
  return {
    label: '本地',
    tone: 'local',
    title: '工具在当前 Stellaclaw 工作区执行'
  };
}

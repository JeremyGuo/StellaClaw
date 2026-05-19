import React from 'react';

export class AppErrorBoundary extends React.Component {
  constructor(props) {
    super(props);
    this.state = { error: null };
  }

  static getDerivedStateFromError(error) {
    return { error };
  }

  componentDidCatch(error, info) {
    console.error('[StellaCodeX fatal render error]', error, info);
  }

  render() {
    if (this.state.error) {
      return (
        <div className="app-fatal-error">
          <strong>界面渲染失败</strong>
          <span>{this.state.error?.message || '未知错误'}</span>
          <button type="button" onClick={() => window.location.reload()}>
            重新加载
          </button>
        </div>
      );
    }
    return this.props.children;
  }
}

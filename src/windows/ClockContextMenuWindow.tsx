import { emit } from '@tauri-apps/api/event';
import { invoke } from '@tauri-apps/api/core';
import { type CSSProperties, type ReactElement, useEffect } from 'react';

/** 诊断日志：经 ov-err 事件交由后端写入 overlay_err.log。 */
const log = (message: string): void => {
  void emit('ov-err', `ccm: ${message}`).catch(() => {});
};

/** 任务栏时钟右键菜单窗口：展示「设置 / 退出」，由后端在时钟点击处显示。 */
const ClockContextMenuWindow = (): ReactElement => {
  useEffect(() => {
    log(`loaded ${window.location.href}`);
    /** 记录窗口内点击位置，验证点击是否可达前端。 */
    const onMouseDown = (event: MouseEvent): void => {
      log(`mousedown ${event.clientX},${event.clientY}`);
    };
    /** Esc 关闭菜单。 */
    const onKeyDown = (event: KeyboardEvent): void => {
      if (event.key === 'Escape') {
        log('esc');
        void invoke('hide_clock_context_menu').catch((error) => log(`esc ERR ${error}`));
      }
    };
    window.addEventListener('mousedown', onMouseDown);
    window.addEventListener('keydown', onKeyDown);
    return () => {
      window.removeEventListener('mousedown', onMouseDown);
      window.removeEventListener('keydown', onKeyDown);
    };
  }, []);

  /** 执行菜单动作：后端会先隐藏菜单，再执行对应行为。 */
  const runAction = (name: 'settings' | 'exit'): void => {
    log(`action ${name}`);
    invoke('clock_menu_action', { action: name })
      .then(() => log(`action ${name} ok`))
      .catch((error) => log(`action ${name} ERR ${error}`));
  };

  /**
   * 根级兜底路由：点击落在按钮边缘/内边距（死区）时按最近项处理——
   * 菜单窗口下半部视为退出、上半部视为设置，确保窗口内任意点击都有效。
   */
  const onRootMouseDown = (event: React.MouseEvent<HTMLDivElement>): void => {
    const rect = event.currentTarget.getBoundingClientRect();
    if (event.target === event.currentTarget) {
      const action = event.clientY - rect.top > rect.height / 2 ? 'exit' : 'settings';
      log(`root fallback ${action} at ${event.clientX},${event.clientY}`);
      runAction(action);
    }
  };

  return (
    <div style={styles.root} onMouseDown={onRootMouseDown}>
      <style>{`.ccm-item:hover { background: rgba(255, 255, 255, 0.12); }`}</style>
      <button
        type="button"
        className="ccm-item"
        style={styles.item}
        onMouseDown={() => runAction('settings')}
      >
        设置
      </button>
      <div style={styles.divider} />
      <button
        type="button"
        className="ccm-item"
        style={styles.item}
        onMouseDown={() => runAction('exit')}
      >
        退出
      </button>
    </div>
  );
};

const styles: Record<string, CSSProperties> = {
  root: {
    width: '100vw',
    height: '100vh',
    boxSizing: 'border-box',
    display: 'flex',
    flexDirection: 'column',
    background: 'rgba(43, 43, 43, 0.98)',
    borderRadius: 8,
    padding: '6px 6px 0',
    userSelect: 'none',
    fontFamily: '"Segoe UI", system-ui, sans-serif',
    fontSize: 14,
    color: '#f0f0f0',
  },
  item: {
    all: 'unset',
    boxSizing: 'border-box',
    flex: 1,
    display: 'flex',
    alignItems: 'center',
    padding: '0 16px',
    borderRadius: 6,
    cursor: 'pointer',
  },
  divider: {
    height: 1,
    margin: '2px 8px',
    background: 'rgba(255, 255, 255, 0.14)',
  },
};

export default ClockContextMenuWindow;

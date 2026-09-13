import { invoke } from '@tauri-apps/api/core';
import { Button, ColorPicker, Divider, Form, Slider, Switch, Tag, Tooltip } from 'antd';
import React, { useEffect, useState } from 'react';
import { syncValuesConfig } from '../../../sync/base/syncValuesConfig.ts';
import { useConfigSync } from '../../../sync/configStore.ts';
import type { ClockbarStyle } from '../../../sync/type/configTypes.ts';
import { isDesktop, isWindows } from '../../../utils/platform.ts';

/** 段 id → 中文名（与后端 SEG_IDS/time 对齐） */
const SEG_LABELS: Record<string, string> = {
  weather: '天气',
  festival: '节日',
  term: '节气',
  lunar: '农历',
  time: '时间',
};

/** 与 configStore 默认值一致的兜底样式（D4 现行为） */
const DEFAULT_CLOCKBAR_STYLE: ClockbarStyle = {
  order: ['weather', 'festival', 'term', 'lunar', 'time'],
  show: { weather: true, festival: true, term: true, lunar: true, time: true },
  colors: {},
  sizes: {},
  gap: 10,
};

/** 归一化：剔除未知 id、保底字段齐全（旧 liConfig 可能缺字段） */
function normalizeStyle(raw: unknown): ClockbarStyle {
  const d = DEFAULT_CLOCKBAR_STYLE;
  if (!raw || typeof raw !== 'object') return { ...d };
  const r = raw as Partial<ClockbarStyle>;
  const known = Object.keys(SEG_LABELS);
  const order = Array.isArray(r.order)
    ? r.order.filter((id) => known.includes(id))
    : [];
  return {
    order: order.length === 5 ? order : [...d.order],
    show: { ...d.show, ...(r.show ?? {}) },
    colors: { ...(r.colors ?? {}) },
    sizes: { ...(r.sizes ?? {}) },
    gap: typeof r.gap === 'number' ? Math.min(40, Math.max(0, r.gap)) : d.gap,
  };
}

const WidgetShowForm: React.FC = () => {
  const { data: config } = useConfigSync();

  /** 桌面组件开关的提交加载态。 */
  const [desktopWidgetLoading, setDesktopWidgetLoading] = useState<boolean>(false);
  /** 任务栏弹窗开关的提交加载态。 */
  const [taskbarWidgetLoading, setTaskbarWidgetLoading] = useState<boolean>(false);
  /** 注入式任务栏时钟开关的提交加载态。 */
  const [clockbarInjectionLoading, setClockbarInjectionLoading] = useState<boolean>(false);
  /** 时钟段样式（S 阶段：开关/顺序/颜色/字号/间距） */
  const [clockbarStyle, setClockbarStyle] = useState<ClockbarStyle>(DEFAULT_CLOCKBAR_STYLE);

  useEffect(() => {
    if (config?.clockbarStyle) setClockbarStyle(normalizeStyle(config.clockbarStyle));
  }, [config?.clockbarStyle]);

  if (!isDesktop) {
    return null;
  }

  /** 统一提交：后端命令即时生效（数据线程 ≤1s 下发）+ liConfig 持久化 */
  const updateStyle = async (next: ClockbarStyle): Promise<void> => {
    setClockbarStyle(next);
    try {
      await invoke('set_clockbar_style', { style: next });
    } catch (err) {
      console.error('设置时钟段样式失败:', err);
    }
    await syncValuesConfig({ clockbarStyle: next });
  };

  // 处理桌面组件开关变化
  const handleDesktopWidgetEnabledChange = async (checked: boolean): Promise<void> => {
    setDesktopWidgetLoading(true);
    try {
      await invoke('set_desktop_widget_enabled', { enabled: checked });
    } catch (err) {
      console.error('设置桌面组件开关失败:', err);
    } finally {
      setDesktopWidgetLoading(false);
    }
  };

  // 处理任务栏弹窗组件开关变化
  const handleTaskbarWidgetEnabledChange = async (checked: boolean): Promise<void> => {
    setTaskbarWidgetLoading(true);
    try {
      await invoke('set_taskbar_widget_enabled_command', { enabled: checked });
    } catch (err) {
      console.error('设置任务栏弹窗开关失败:', err);
    } finally {
      setTaskbarWidgetLoading(false);
    }
  };

  // 处理注入式任务栏时钟开关变化（E6：与旧覆盖层互斥，二选一）
  const handleClockbarInjectionChange = async (checked: boolean): Promise<void> => {
    setClockbarInjectionLoading(true);
    try {
      await invoke('set_clockbar_injection_enabled', { enabled: checked });
    } catch (err) {
      console.error('设置注入式任务栏时钟开关失败:', err);
    } finally {
      setClockbarInjectionLoading(false);
    }
  };

  /** 顺序上移/下移 */
  const moveSeg = (idx: number, dir: -1 | 1): void => {
    const target = idx + dir;
    if (target < 0 || target >= clockbarStyle.order.length) return;
    const order = [...clockbarStyle.order];
    [order[idx], order[target]] = [order[target], order[idx]];
    void updateStyle({ ...clockbarStyle, order });
  };

  const rowStyle: React.CSSProperties = {
    display: 'flex',
    alignItems: 'center',
    gap: 8,
    padding: '4px 0 4px 12px',
  };
  const segSliderStyle: React.CSSProperties = { width: 110 };

  return (
    <div>
      <Form
        labelCol={{ span: 5 }}
        wrapperCol={{ span: 14 }}
        labelAlign="left"
        colon={false}
        initialValues={config}
        onValuesChange={syncValuesConfig}
      >
        {isWindows && (
          <div>
            <Form.Item name="desktopWidgetEnabled" label="桌面组件">
              <Switch
                loading={desktopWidgetLoading}
                disabled={desktopWidgetLoading}
                onChange={handleDesktopWidgetEnabledChange}
              />
            </Form.Item>
            <Form.Item name="taskbarWidgetEnabled" label="替换任务栏日历">
              <Switch
                loading={taskbarWidgetLoading}
                disabled={taskbarWidgetLoading}
                onChange={handleTaskbarWidgetEnabledChange}
              />
            </Form.Item>
            <Form.Item
              name="clockbarInjectionEnabled"
              label="注入式时钟"
              tooltip="注入改造任务栏原生时钟显示天气/节日/节气/农历（与「替换任务栏日历」互斥，二者不同时操作时钟）"
            >
              <Switch
                loading={clockbarInjectionLoading}
                disabled={clockbarInjectionLoading}
                onChange={handleClockbarInjectionChange}
              />
            </Form.Item>
          </div>
        )}
      </Form>

      {isWindows && (
        <div style={{ marginTop: 8 }}>
          <Divider plain style={{ margin: '8px 0' }}>
            时钟段样式（注入式时钟开启时生效）
          </Divider>
          <div style={rowStyle}>
            <span style={{ width: 64 }}>段间距</span>
            <Slider
              min={0}
              max={40}
              step={1}
              value={clockbarStyle.gap}
              onChange={(v) => void updateStyle({ ...clockbarStyle, gap: v })}
              style={{ width: 160 }}
              tooltip={{ formatter: (v) => `${v}px` }}
            />
          </div>
          {clockbarStyle.order.map((id, idx) => {
            const isTime = id === 'time';
            const shown = clockbarStyle.show[id] ?? true;
            const color = clockbarStyle.colors[id];
            const size = clockbarStyle.sizes[id] ?? 1;
            return (
              <div key={id} style={rowStyle}>
                <span style={{ width: 64, display: 'inline-flex', alignItems: 'center', gap: 4 }}>
                  <Button
                    size="small"
                    type="text"
                    icon="↑"
                    disabled={idx === 0}
                    onClick={() => moveSeg(idx, -1)}
                    aria-label={`${SEG_LABELS[id]}上移`}
                  />
                  <Button
                    size="small"
                    type="text"
                    icon="↓"
                    disabled={idx === clockbarStyle.order.length - 1}
                    onClick={() => moveSeg(idx, 1)}
                    aria-label={`${SEG_LABELS[id]}下移`}
                  />
                  {SEG_LABELS[id]}
                </span>
                {isTime ? (
                  <>
                    <Tag style={{ marginInlineEnd: 0 }}>系统原生样式</Tag>
                    <span style={{ color: 'var(--ant-color-text-tertiary, #999)', fontSize: 12 }}>
                      时间段保持系统时钟原生外观
                    </span>
                  </>
                ) : (
                  <>
                    <Switch
                      size="small"
                      checked={shown}
                      checkedChildren="显"
                      unCheckedChildren="隐"
                      onChange={(checked) => {
                        const show = { ...clockbarStyle.show, [id]: checked };
                        void updateStyle({ ...clockbarStyle, show });
                      }}
                    />
                    <Tooltip title={color ? '自定义颜色（点击色块修改）' : '跟随主题（点击选择颜色）'}>
                      <ColorPicker
                        size="small"
                        disabledAlpha
                        value={color ?? '#808080'}
                        onChangeComplete={(c) => {
                          const colors = { ...clockbarStyle.colors, [id]: c.toHexString() };
                          void updateStyle({ ...clockbarStyle, colors });
                        }}
                      />
                    </Tooltip>
                    {color && (
                      <Button
                        size="small"
                        type="link"
                        onClick={() => {
                          const colors = { ...clockbarStyle.colors };
                          delete colors[id];
                          void updateStyle({ ...clockbarStyle, colors });
                        }}
                      >
                        跟随主题
                      </Button>
                    )}
                    <Slider
                      min={0.5}
                      max={2}
                      step={0.05}
                      value={size}
                      disabled={!shown}
                      onChange={(v) => {
                        const sizes = { ...clockbarStyle.sizes, [id]: v };
                        void updateStyle({ ...clockbarStyle, sizes });
                      }}
                      style={segSliderStyle}
                      tooltip={{ formatter: (v) => `${v?.toFixed(2)}×` }}
                    />
                  </>
                )}
              </div>
            );
          })}
          <div style={{ padding: '2px 12px 0', color: '#999', fontSize: 12 }}>
            顺序即任务栏时钟上的从左到右排列；自定义颜色优先于主题，点「跟随主题」恢复自动配色。
          </div>
        </div>
      )}
    </div>
  );
};

export default WidgetShowForm;

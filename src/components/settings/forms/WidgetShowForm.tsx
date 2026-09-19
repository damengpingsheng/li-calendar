import { invoke } from '@tauri-apps/api/core';
import {
  Button,
  ColorPicker,
  Divider,
  Form,
  Input,
  Select,
  Slider,
  Switch,
  Tag,
  Tooltip,
} from 'antd';
import React, { Component, useEffect, useState } from 'react';
import { syncValuesConfig } from '../../../sync/base/syncValuesConfig.ts';
import { useConfigSync } from '../../../sync/configStore.ts';
import type { ClockbarStyle } from '../../../sync/type/configTypes.ts';
import { isDesktop, isWindows } from '../../../utils/platform.ts';

/** 时钟段样式分组的错误边界：渲染异常时显示错误信息而不是白屏整页 */
class ClockbarStyleErrorBoundary extends Component<
  { children: React.ReactNode },
  { error: Error | null }
> {
  state = { error: null as Error | null };
  static getDerivedStateFromError(error: Error) {
    return { error };
  }
  componentDidCatch(error: Error) {
    console.error('时钟段样式分组渲染异常:', error);
  }
  render() {
    if (this.state.error) {
      return (
        <div style={{ padding: 12, color: '#c00', fontSize: 12 }}>
          时钟段样式面板渲染异常：{this.state.error.message}
          <Button size="small" type="link" onClick={() => this.setState({ error: null })}>
            重试
          </Button>
        </div>
      );
    }
    return this.props.children;
  }
}

/** 段 id → 中文名（与后端 SEG_IDS/time 对齐） */
const SEG_LABELS: Record<string, string> = {
  weather: '天气',
  festival: '节日',
  term: '节气',
  lunar: '农历',
  time: '时间',
};

/** 与 configStore 默认值一致的兜底样式（D4 现行为；v55 农历默认在日期行；v60 天气段增强） */
const DEFAULT_CLOCKBAR_STYLE: ClockbarStyle = {
  order: ['weather', 'festival', 'term', 'lunar', 'time'],
  show: { weather: true, festival: true, term: true, lunar: true, time: true },
  colors: {},
  sizes: {},
  rows: { weather: 1, festival: 1, term: 1, lunar: 2 },
  gap: 10,
  gap2: 10,
  vgap: 0,
  halignTime: 0,
  halignDate: 0,
  weatherCity: '',
  weatherAdcode: '',
  weatherEmoji: true,
  weatherEmojiColor: true,
  weatherText: true,
  weatherWind: true,
};

/** 预设常用色：取色面板底部直接点选，免拖滑条（任务栏文字深浅底色兼顾） */
const COLOR_PRESETS: { label: string; colors: string[] }[] = [
  {
    label: '常用颜色',
    colors: [
      '#FFFFFF',
      '#BFBFBF',
      '#595959',
      '#000000',
      '#FF4D4F',
      '#FF7A45',
      '#FAAD14',
      '#FADB14',
      '#52C41A',
      '#13C2C2',
      '#1677FF',
      '#9254DE',
      '#F759AB',
    ],
  },
];

/** 归一化：剔除未知 id、保底字段齐全（旧 liConfig 可能缺字段） */
function normalizeStyle(raw: unknown): ClockbarStyle {
  const d = DEFAULT_CLOCKBAR_STYLE;
  if (!raw || typeof raw !== 'object') return { ...d };
  const r = raw as Partial<ClockbarStyle>;
  const known = Object.keys(SEG_LABELS);
  const order = Array.isArray(r.order) ? r.order.filter((id) => known.includes(id)) : [];
  const rows: Record<string, number> = {};
  for (const id of ['weather', 'festival', 'term', 'lunar']) {
    const v = r.rows?.[id];
    rows[id] = v === 2 ? 2 : id === 'lunar' ? 2 : 1;
  }
  return {
    order: order.length === 5 ? order : [...d.order],
    show: { ...d.show, ...(r.show ?? {}) },
    colors: { ...(r.colors ?? {}) },
    sizes: { ...(r.sizes ?? {}) },
    rows,
    gap: typeof r.gap === 'number' ? Math.min(40, Math.max(0, r.gap)) : d.gap,
    gap2: typeof r.gap2 === 'number' ? Math.min(40, Math.max(0, r.gap2)) : d.gap2,
    vgap: typeof r.vgap === 'number' ? Math.min(20, Math.max(0, r.vgap)) : d.vgap,
    halignTime: r.halignTime === 1 || r.halignTime === 2 ? r.halignTime : 0,
    halignDate: r.halignDate === 1 || r.halignDate === 2 ? r.halignDate : 0,
    // v60 天气段增强（后端还会再清洗/钳制一次——双端防御）；v62 城区名留空=自动跟随
    weatherCity: typeof r.weatherCity === 'string' ? r.weatherCity.slice(0, 16) : d.weatherCity,
    weatherAdcode:
      typeof r.weatherAdcode === 'string'
        ? r.weatherAdcode.replace(/\D/g, '').slice(0, 6)
        : d.weatherAdcode,
    weatherEmoji: r.weatherEmoji !== false,
    weatherEmojiColor: r.weatherEmojiColor !== false,
    weatherText: r.weatherText !== false,
    weatherWind: r.weatherWind !== false,
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
  /** 当前展开取色面板的段 id（受控展开，供面板内「跟随主题」点击后立即收起） */
  const [openColorSeg, setOpenColorSeg] = useState<string | null>(null);

  useEffect(() => {
    if (config?.clockbarStyle) setClockbarStyle(normalizeStyle(config.clockbarStyle));
  }, [config?.clockbarStyle]);

  /** 城区名输入草稿（失焦/回车才提交，避免逐键 invoke 洪泛） */
  const [cityDraft, setCityDraft] = useState<string>(clockbarStyle.weatherCity);
  useEffect(() => {
    setCityDraft(clockbarStyle.weatherCity);
  }, [clockbarStyle.weatherCity]);
  const commitCity = (): void => {
    const v = cityDraft.trim().slice(0, 16);
    if (v !== clockbarStyle.weatherCity) {
      void commitStyle({ ...clockbarStyle, weatherCity: v });
    }
  };
  /** 城区 adcode 草稿（留空=IP 自动定位；6 位数字=锁定区县级） */
  const [adcodeDraft, setAdcodeDraft] = useState<string>(clockbarStyle.weatherAdcode);
  useEffect(() => {
    setAdcodeDraft(clockbarStyle.weatherAdcode);
  }, [clockbarStyle.weatherAdcode]);
  const commitAdcode = (): void => {
    const v = adcodeDraft.replace(/\D/g, '').slice(0, 6);
    if (v !== clockbarStyle.weatherAdcode) {
      void commitStyle({ ...clockbarStyle, weatherAdcode: v });
    }
  };

  if (!isDesktop) {
    return null;
  }

  /** 统一提交：后端命令即时生效（数据线程 ≤1s 下发）+ liConfig 持久化。
   * 仅离散操作（开关/选择/取色完成）调用；滑条拖动走 preview+onChangeComplete */
  const commitStyle = async (next: ClockbarStyle): Promise<void> => {
    setClockbarStyle(next);
    try {
      await invoke('set_clockbar_style', { style: next });
    } catch (err) {
      console.error('设置时钟段样式失败:', err);
    }
    await syncValuesConfig({ clockbarStyle: next });
  };
  /** 滑条拖动中的本地预览：只更新 state，不触发 invoke/持久化/跨窗口广播
   * （v58：拖动中高频提交曾致 262 次洪泛+设置页白屏） */
  const previewStyle = (next: ClockbarStyle): void => {
    setClockbarStyle(next);
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
    void commitStyle({ ...clockbarStyle, order });
  };

  const rowStyle: React.CSSProperties = {
    display: 'flex',
    alignItems: 'center',
    gap: 8,
    padding: '4px 0 4px 12px',
  };
  // 段样式五列网格（排序/显示/行/颜色/字号）：全部定宽锁定，任何行（含表头）列位
  // 一致，窗口拉宽/缩窄整表不动。显示列 60 包一层定宽 flex 容器再放开关——开关
  // 用自然宽度渲染（同天气段增强行，实测 57px），不参与网格项尺寸判定，杜绝裁字；
  // 行列 84=Select 定宽；颜色列容纳色块+「主题」状态标签
  const segGridStyle: React.CSSProperties = {
    display: 'grid',
    gridTemplateColumns: '96px 60px 84px 120px 110px',
    columnGap: 8,
    alignItems: 'center',
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
        <>
          <ClockbarStyleErrorBoundary>
            <div style={{ marginTop: 8 }}>
              <Divider plain style={{ margin: '8px 0' }}>
                时钟段样式（注入式时钟开启时生效）
              </Divider>
              <div style={rowStyle}>
                <span style={{ width: 96 }}>上下行间距</span>
                <Slider
                  min={0}
                  max={20}
                  step={1}
                  value={clockbarStyle.vgap}
                  onChange={(v) => previewStyle({ ...clockbarStyle, vgap: v })}
                  onChangeComplete={(v) => void commitStyle({ ...clockbarStyle, vgap: v })}
                  style={{ width: 160 }}
                  tooltip={{ formatter: (v) => `${v}px` }}
                />
                <span style={{ color: 'var(--ant-color-text-tertiary, #999)', fontSize: 12 }}>
                  时间行与日期行之间的垂直间隔
                </span>
              </div>
              <div style={rowStyle}>
                <span style={{ width: 96 }}>时间行间距</span>
                <Slider
                  min={0}
                  max={40}
                  step={1}
                  value={clockbarStyle.gap}
                  onChange={(v) => previewStyle({ ...clockbarStyle, gap: v })}
                  onChangeComplete={(v) => void commitStyle({ ...clockbarStyle, gap: v })}
                  style={{ width: 160 }}
                  tooltip={{ formatter: (v) => `${v}px` }}
                />
                <Tooltip title="整行在时钟区内的水平对齐（时间数字右侧的空白受此影响）">
                  <Select
                    size="small"
                    value={clockbarStyle.halignTime}
                    onChange={(v) => void commitStyle({ ...clockbarStyle, halignTime: v })}
                    options={[
                      { value: 0, label: '靠左' },
                      { value: 1, label: '居中' },
                      { value: 2, label: '靠右' },
                    ]}
                    style={{ width: 76 }}
                  />
                </Tooltip>
              </div>
              <div style={rowStyle}>
                <span style={{ width: 96 }}>日期行间距</span>
                <Slider
                  min={0}
                  max={40}
                  step={1}
                  value={clockbarStyle.gap2}
                  onChange={(v) => previewStyle({ ...clockbarStyle, gap2: v })}
                  onChangeComplete={(v) => void commitStyle({ ...clockbarStyle, gap2: v })}
                  style={{ width: 160 }}
                  tooltip={{ formatter: (v) => `${v}px` }}
                />
                <Tooltip title="整行在时钟区内的水平对齐">
                  <Select
                    size="small"
                    value={clockbarStyle.halignDate}
                    onChange={(v) => void commitStyle({ ...clockbarStyle, halignDate: v })}
                    options={[
                      { value: 0, label: '靠左' },
                      { value: 1, label: '居中' },
                      { value: 2, label: '靠右' },
                    ]}
                    style={{ width: 76 }}
                  />
                </Tooltip>
              </div>
              <div style={{ ...segGridStyle, padding: '0 0 2px 12px' }}>
                {(['排序', '显示', '所在行', '颜色', '字体大小'] as const).map((label) => (
                  <span
                    key={label}
                    style={{ color: 'var(--ant-color-text-tertiary, #999)', fontSize: 12 }}
                  >
                    {label}
                  </span>
                ))}
              </div>
              {clockbarStyle.order.map((id, idx) => {
                const isTime = id === 'time';
                const shown = clockbarStyle.show[id] ?? true;
                const color = clockbarStyle.colors[id];
                const size = clockbarStyle.sizes[id] ?? 1;
                const row =
                  id === 'lunar' ? (clockbarStyle.rows[id] ?? 2) : (clockbarStyle.rows[id] ?? 1);
                return (
                  <div key={id} style={segGridStyle}>
                    <span
                      style={{
                        display: 'inline-flex',
                        alignItems: 'center',
                        gap: 4,
                        whiteSpace: 'nowrap',
                      }}
                    >
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
                      <div
                        style={{
                          gridColumn: '2 / -1',
                          display: 'flex',
                          alignItems: 'center',
                          gap: 8,
                        }}
                      >
                        <Tag style={{ marginInlineEnd: 0 }}>系统原生样式</Tag>
                        <span
                          style={{ color: 'var(--ant-color-text-tertiary, #999)', fontSize: 12 }}
                        >
                          时间段保持系统时钟原生外观
                        </span>
                      </div>
                    ) : (
                      <>
                        <span style={{ width: 60, display: 'inline-flex' }}>
                          <Switch
                            checked={shown}
                            checkedChildren="显示"
                            unCheckedChildren="隐藏"
                            onChange={(checked) => {
                              const show = { ...clockbarStyle.show, [id]: checked };
                              void commitStyle({ ...clockbarStyle, show });
                            }}
                          />
                        </span>
                        <Tooltip title="段落所在行：时间行与时间同行，日期行与系统原生日期同行">
                          <Select
                            size="small"
                            value={row === 2 ? 2 : 1}
                            disabled={!shown}
                            onChange={(v) => {
                              const rows = { ...clockbarStyle.rows, [id]: v };
                              void commitStyle({ ...clockbarStyle, rows });
                            }}
                            options={[
                              { value: 1, label: '时间行' },
                              { value: 2, label: '日期行' },
                            ]}
                            style={{ width: 84 }}
                          />
                        </Tooltip>
                        <span style={{ display: 'inline-flex', alignItems: 'center', gap: 8 }}>
                          <Tooltip
                            title={
                              color
                                ? '自定义颜色（选预设或拖滑条；面板底部可恢复跟随主题）'
                                : '跟随主题（点击选色自定义）'
                            }
                          >
                            <ColorPicker
                              size="small"
                              disabledAlpha
                              value={color ?? '#808080'}
                              presets={COLOR_PRESETS}
                              open={openColorSeg === id}
                              onOpenChange={(o) => setOpenColorSeg(o ? id : null)}
                              panelRender={(panel) => (
                                <div>
                                  {panel}
                                  <Divider style={{ margin: '4px 0' }} />
                                  {color ? (
                                    <Button
                                      size="small"
                                      block
                                      onClick={() => {
                                        const colors = { ...clockbarStyle.colors };
                                        delete colors[id];
                                        void commitStyle({ ...clockbarStyle, colors });
                                        setOpenColorSeg(null);
                                      }}
                                    >
                                      跟随主题
                                    </Button>
                                  ) : (
                                    <div
                                      style={{
                                        textAlign: 'center',
                                        color: 'var(--ant-color-text-tertiary, #999)',
                                        fontSize: 12,
                                        padding: '2px 0',
                                      }}
                                    >
                                      当前跟随主题
                                    </div>
                                  )}
                                </div>
                              )}
                              onChangeComplete={(c) => {
                                const colors = {
                                  ...clockbarStyle.colors,
                                  [id]: c.toHexString(),
                                };
                                void commitStyle({ ...clockbarStyle, colors });
                              }}
                            />
                          </Tooltip>
                          {!color && <Tag style={{ marginInlineEnd: 0 }}>主题</Tag>}
                        </span>
                        <Slider
                          min={0.5}
                          max={2}
                          step={0.05}
                          value={size}
                          disabled={!shown}
                          onChange={(v) => {
                            const sizes = { ...clockbarStyle.sizes, [id]: v };
                            previewStyle({ ...clockbarStyle, sizes });
                          }}
                          onChangeComplete={(v) => {
                            const sizes = { ...clockbarStyle.sizes, [id]: v };
                            void commitStyle({ ...clockbarStyle, sizes });
                          }}
                          style={segSliderStyle}
                          tooltip={{ formatter: (v) => `${v?.toFixed(2)}×` }}
                        />
                      </>
                    )}
                  </div>
                );
              })}
              <Divider plain style={{ margin: '12px 0 4px' }}>
                天气段增强
              </Divider>
              <div style={rowStyle}>
                <span style={{ width: 96 }}>城区名</span>
                <Tooltip title="城区名留空=自动跟随定位的城区（高德响应自带，剥「市/区/县」后缀）；填写则固定显示所填文本">
                  <Input
                    size="small"
                    style={{ width: 110 }}
                    maxLength={16}
                    value={cityDraft}
                    placeholder="留空=自动"
                    onChange={(e) => setCityDraft(e.target.value)}
                    onBlur={commitCity}
                    onPressEnter={commitCity}
                  />
                </Tooltip>
                <Tooltip title="高德行政区划码：留空=按 IP 自动定位（直辖市只到市级）；填 6 位锁定区县级（如 110114=昌平区）。改后下一分钟内生效">
                  <Input
                    size="small"
                    style={{ width: 96 }}
                    maxLength={6}
                    value={adcodeDraft}
                    placeholder="adcode"
                    onChange={(e) => setAdcodeDraft(e.target.value.replace(/\D/g, '').slice(0, 6))}
                    onBlur={commitAdcode}
                    onPressEnter={commitAdcode}
                  />
                </Tooltip>
                <span style={{ whiteSpace: 'nowrap' }}>现象文字</span>
                <Switch
                  size="small"
                  checked={clockbarStyle.weatherText}
                  checkedChildren="显示"
                  unCheckedChildren="隐藏"
                  onChange={(checked) =>
                    void commitStyle({ ...clockbarStyle, weatherText: checked })
                  }
                />
                <Tooltip title="风向风级（高德数据源，如「东北风3~4级」），拼在现象文字后">
                  <span style={{ whiteSpace: 'nowrap' }}>风向风级</span>
                </Tooltip>
                <Switch
                  size="small"
                  checked={clockbarStyle.weatherWind}
                  checkedChildren="显示"
                  unCheckedChildren="隐藏"
                  onChange={(checked) =>
                    void commitStyle({ ...clockbarStyle, weatherWind: checked })
                  }
                />
              </div>
              <div style={rowStyle}>
                <span style={{ width: 96 }}>天气图标</span>
                <Tooltip title="Unicode emoji 拼进天气段（🌞⛅☁🌦⛈🌧🌨🌫🌪 按天气码映射）。黑白=文本字形 U+FE0E——2026-09-16 任务栏实测：仅 ☁ 等有文本字形者真黑白，其余回落彩色（Windows 字体 fallback）。">
                  <Select
                    size="small"
                    value={
                      !clockbarStyle.weatherEmoji
                        ? 'off'
                        : clockbarStyle.weatherEmojiColor
                          ? 'color'
                          : 'mono'
                    }
                    onChange={(v) =>
                      void commitStyle({
                        ...clockbarStyle,
                        weatherEmoji: v !== 'off',
                        weatherEmojiColor: v === 'color',
                      })
                    }
                    options={[
                      { value: 'color', label: '彩色' },
                      { value: 'mono', label: '黑白' },
                      { value: 'off', label: '关闭' },
                    ]}
                    style={{ width: 76 }}
                  />
                </Tooltip>
                <span style={{ color: 'var(--ant-color-text-tertiary, #999)', fontSize: 12 }}>
                  示例：昌平 ⛈ 26.1℃ 雷阵雨
                </span>
              </div>
              <div style={{ padding: '6px 12px 0', color: '#999', fontSize: 12, lineHeight: 1.9 }}>
                <div>顺序即任务栏时钟上的从左到右排列。</div>
                <div>
                  「日期行」为系统原生日期（时间+日期+星期保持系统样式），行归属可选择段落在时间行或日期行。
                </div>
                <div>
                  日期行首位恒为系统原生日期（如「周一
                  2026-9-14」，含星期几），其余段落按上方顺序追加其后。
                </div>
                <div>自定义颜色优先于主题，点「跟随主题」恢复自动配色。</div>
              </div>
            </div>
          </ClockbarStyleErrorBoundary>
          <Divider style={{ margin: '36px 0 12px' }} />
        </>
      )}
    </div>
  );
};

export default WidgetShowForm;

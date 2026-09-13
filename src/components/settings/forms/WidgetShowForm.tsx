import { invoke } from '@tauri-apps/api/core';
import { Form, Switch } from 'antd';
import React, { useState } from 'react';
import { syncValuesConfig } from '../../../sync/base/syncValuesConfig.ts';
import { useConfigSync } from '../../../sync/configStore.ts';
import { isDesktop, isWindows } from '../../../utils/platform.ts';

const WidgetShowForm: React.FC = () => {
  const { data: config } = useConfigSync();

  /** 桌面组件开关的提交加载态。 */
  const [desktopWidgetLoading, setDesktopWidgetLoading] = useState<boolean>(false);
  /** 任务栏弹窗开关的提交加载态。 */
  const [taskbarWidgetLoading, setTaskbarWidgetLoading] = useState<boolean>(false);
  /** 注入式任务栏时钟开关的提交加载态。 */
  const [clockbarInjectionLoading, setClockbarInjectionLoading] = useState<boolean>(false);

  if (!isDesktop) {
    return null;
  }

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

  return (
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
  );
};

export default WidgetShowForm;

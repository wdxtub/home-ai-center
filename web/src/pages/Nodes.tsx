import { useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import {
  Card, Table, Button, Modal, Form, Input, InputNumber, Switch, Tag, Space,
  Typography, Alert, App, Popconfirm, Tooltip, Select,
} from 'antd'
import { PlusOutlined, EditOutlined, DeleteOutlined, WarningOutlined } from '@ant-design/icons'
import { api, type LlmNode, type PriceList } from '../api'
import { cooldown } from '../format'

interface NodeForm {
  name: string
  base_url: string
  lan_base_url?: string
  max_concurrency: number
  default_max_output_tokens?: number
  enabled: boolean
  sort_order: number
  disabled_start?: number
  disabled_end?: number
  disabled_timezone?: string
  extra_body?: string
}

const BLANK: NodeForm = {
  name: '',
  base_url: '',
  max_concurrency: 2,
  default_max_output_tokens: 4096,
  enabled: true,
  sort_order: 0,
  extra_body: '{}',
}

export default function Nodes() {
  const qc = useQueryClient()
  const { message } = App.useApp()
  const [editing, setEditing] = useState<LlmNode | null>(null)
  const [open, setOpen] = useState(false)
  const [form] = Form.useForm<NodeForm>()

  const nodes = useQuery({
    queryKey: ['nodes'],
    queryFn: () => api.get<{ items: LlmNode[] }>('/nodes'),
    refetchInterval: 4000,
  })
  const prices = useQuery({ queryKey: ['prices'], queryFn: () => api.get<PriceList>('/prices') })

  const save = useMutation({
    mutationFn: (v: NodeForm & { id?: number }) => {
      const body = {
        ...v,
        disabled_start: v.disabled_start ?? null,
        disabled_end: v.disabled_end ?? null,
        disabled_timezone: v.disabled_start != null ? (v.disabled_timezone ?? 'Asia/Shanghai') : null,
        extra_body: safeJson(v.extra_body),
        lan_base_url: v.lan_base_url || null,
      }
      return v.id ? api.put(`/nodes/${v.id}`, body) : api.post('/nodes', body)
    },
    onSuccess: () => {
      message.success('已保存，配置立即生效')
      setOpen(false)
      void qc.invalidateQueries({ queryKey: ['nodes'] })
    },
    onError: (e: Error) => message.error(e.message),
  })

  const del = useMutation({
    mutationFn: (id: number) => api.del(`/nodes/${id}`),
    onSuccess: () => {
      message.success('已删除')
      void qc.invalidateQueries({ queryKey: ['nodes'] })
    },
    onError: (e: Error) => message.error(e.message),
  })

  function showEditor(n?: LlmNode) {
    setEditing(n ?? null)
    if (n) {
      form.setFieldsValue({
        name: n.name,
        base_url: n.base_url,
        lan_base_url: n.lan_base_url ?? undefined,
        max_concurrency: n.max_concurrency,
        default_max_output_tokens: n.default_max_output_tokens ?? undefined,
        enabled: n.enabled,
        sort_order: n.sort_order,
        disabled_start: n.disabled_start ?? undefined,
        disabled_end: n.disabled_end ?? undefined,
        disabled_timezone: n.disabled_timezone ?? undefined,
        extra_body: JSON.stringify(n.extra_body ?? {}, null, 2),
      })
    } else {
      form.setFieldsValue(BLANK)
    }
    setOpen(true)
  }

  const missing = prices.data?.models_without_price ?? []
  const items = nodes.data?.items ?? []

  return (
    <Space direction="vertical" size={16} style={{ width: '100%' }}>
      {missing.length > 0 && (
        <Alert
          type="warning"
          showIcon
          message={`${missing.length} 个模型还没配单价：${missing.join('、')}`}
          description="没配单价的模型会被直接拒绝调用（不允许免费白嫖）。到「设置 → 定价」里补上。"
        />
      )}

      <Card
        title="LLM 节点"
        extra={
          <Button type="primary" icon={<PlusOutlined />} onClick={() => showEditor()}>
            新建节点
          </Button>
        }
      >
        <Table<LlmNode>
          rowKey="id"
          dataSource={items}
          pagination={false}
          scroll={{ x: 1000 }}
          columns={[
            {
              title: '名称',
              dataIndex: 'name',
              fixed: 'left',
              width: 150,
              render: (v, n) => (
                <Space direction="vertical" size={0}>
                  <Typography.Text strong>{v}</Typography.Text>
                  <Typography.Text type="secondary" style={{ fontSize: 12 }}>
                    {n.base_url}
                  </Typography.Text>
                </Space>
              ),
            },
            {
              title: '并发',
              width: 100,
              render: (_, n) => (
                <Tag>
                  {n.active}/{n.max_concurrency}
                  {n.waiting > 0 && ` (+${n.waiting} 排队)`}
                </Tag>
              ),
            },
            {
              title: '可用 key',
              width: 110,
              render: (_, n) =>
                n.usable_keys === n.total_keys ? (
                  <Tag color="green">{n.total_keys}</Tag>
                ) : (
                  <Tag color={n.usable_keys > 0 ? 'gold' : 'red'}>
                    {n.usable_keys}/{n.total_keys}
                  </Tag>
                ),
            },
            {
              title: '状态',
              width: 190,
              render: (_, n) => (
                <Space direction="vertical" size={2}>
                  {!n.enabled && <Tag>已停用</Tag>}
                  {n.in_disabled_window && <Tag color="blue">落在禁用时段</Tag>}
                  {n.enabled && !n.in_disabled_window && n.healthy && <Tag color="green">正常</Tag>}
                  {!n.healthy && (
                    <Tooltip title={n.last_error ?? ''}>
                      <Tag color="red" icon={<WarningOutlined />}>
                        冷却 {cooldown(n.cooldown_secs_remaining)}
                      </Tag>
                    </Tooltip>
                  )}
                  {n.lan_configured &&
                    (n.lan_down ? (
                      <Tooltip title={`内网 ${n.lan_base_url ?? ''} 连不上，正在走公网`}>
                        <Tag color="orange">走公网 {cooldown(n.lan_retry_after_secs)}</Tag>
                      </Tooltip>
                    ) : (
                      <Tag color="cyan">内网优先</Tag>
                    ))}
                </Space>
              ),
            },
            {
              title: '排序',
              dataIndex: 'sort_order',
              width: 80,
              render: (v) => <Typography.Text type="secondary">{v}</Typography.Text>,
            },
            {
              title: '操作',
              fixed: 'right',
              width: 120,
              render: (_, n) => (
                <Space>
                  <Button size="small" icon={<EditOutlined />} onClick={() => showEditor(n)} />
                  <Popconfirm
                    title="删除节点？"
                    description="它下面的 API Key 与模型路由会一并删除。"
                    onConfirm={() => del.mutate(n.id)}
                  >
                    <Button size="small" danger icon={<DeleteOutlined />} />
                  </Popconfirm>
                </Space>
              ),
            },
          ]}
        />
      </Card>

      <Modal
        open={open}
        title={editing ? `编辑 ${editing.name}` : '新建节点'}
        onCancel={() => setOpen(false)}
        onOk={() => form.submit()}
        okText="保存"
        width={620}
        destroyOnHidden
      >
        <Form
          form={form}
          layout="vertical"
          onFinish={(v) => save.mutate({ ...v, id: editing?.id })}
        >
          <Form.Item name="name" label="名称" rules={[{ required: true }]} extra="字母数字开头，只能含字母数字、_ . -">
            <Input placeholder="lm-studio-01" />
          </Form.Item>
          <Form.Item
            name="base_url"
            label="上游地址"
            rules={[{ required: true }]}
            extra="只填到端口，例如 http://192.168.1.20:1234（网关自己补 /chat/completions）"
          >
            <Input placeholder="http://192.168.1.20:1234" />
          </Form.Item>
          <Form.Item name="lan_base_url" label="内网地址（可选）" extra="出站走内网更快；留空就用上面的地址">
            <Input placeholder="http://192.168.1.20:1234" />
          </Form.Item>
          <Space size="large" wrap>
            <Form.Item name="max_concurrency" label="最大并发" rules={[{ required: true }]}>
              <InputNumber min={1} max={64} />
            </Form.Item>
            <Form.Item name="default_max_output_tokens" label="默认 max_tokens">
              <InputNumber min={1} max={1_000_000} />
            </Form.Item>
            <Form.Item name="sort_order" label="排序（小的优先）">
              <InputNumber min={0} max={999} />
            </Form.Item>
            <Form.Item name="enabled" label="启用" valuePropName="checked">
              <Switch />
            </Form.Item>
          </Space>
          <Typography.Text strong>每日禁用时段</Typography.Text>
          <Typography.Paragraph type="secondary" style={{ fontSize: 12, marginTop: 0 }}>
            用于「白天不用、晚上才开机」这类场景。跨午夜直接填，例如 23 → 7。
            起止相同会被拒绝（那等于永久禁用，请用启用开关）。
          </Typography.Paragraph>
          <Space size="large" wrap>
            <Form.Item name="disabled_start" label="开始（0-23 点）">
              <Select allowClear options={hours()} />
            </Form.Item>
            <Form.Item name="disabled_end" label="结束（0-23 点）">
              <Select allowClear options={hours()} />
            </Form.Item>
            <Form.Item name="disabled_timezone" label="时区" initialValue="Asia/Shanghai">
              <Select
                style={{ width: 160 }}
                options={[
                  { value: 'Asia/Shanghai', label: 'Asia/Shanghai' },
                  { value: 'UTC', label: 'UTC' },
                ]}
              />
            </Form.Item>
          </Space>
          <Form.Item
            name="extra_body"
            label="节点级额外请求体（JSON）"
            extra='各后端关思考的参数不统一：LM Studio 用 reasoning_effort，DeepSeek 用 {"thinking":{"type":"disabled"}}。这里原样合并进请求。'
          >
            <Input.TextArea rows={3} style={{ fontFamily: 'monospace' }} />
          </Form.Item>
        </Form>
      </Modal>
    </Space>
  )
}

function hours() {
  return Array.from({ length: 24 }, (_, i) => ({ value: i, label: `${i}:00` }))
}

function safeJson(s?: string): Record<string, unknown> {
  if (!s || !s.trim()) return {}
  try {
    const v = JSON.parse(s)
    return typeof v === 'object' && v !== null ? v : {}
  } catch {
    return {}
  }
}

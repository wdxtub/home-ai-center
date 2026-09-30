import { useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import {
  Card, Table, Button, Modal, Form, Input, InputNumber, Tag, Space, Typography,
  App, Popconfirm, Tooltip, Select, Alert, Drawer, Descriptions,
} from 'antd'
import { PlusOutlined, EditOutlined, DeleteOutlined, ReloadOutlined, HistoryOutlined } from '@ant-design/icons'
import { api, type LlmNode, type NodeKey, type KeyEvent } from '../api'
import { yuanTime, cooldown } from '../format'

/** API Key 页：一个节点一把 key，可以有任意多把。
 *
 * 为什么要多把：不同组织 / 订阅账号的限额桶是独立的，
 * 一把撞了 5 小时限额就切下一把，而不是让整个节点停摆。
 */
export default function Keys() {
  const qc = useQueryClient()
  const { message } = App.useApp()
  const [nodeId, setNodeId] = useState<number | null>(null)
  const [editing, setEditing] = useState<NodeKey | null>(null)
  const [open, setOpen] = useState(false)
  const [form] = Form.useForm()
  const [events, setEvents] = useState<{ key: NodeKey; list: KeyEvent[] } | null>(null)

  const nodes = useQuery({
    queryKey: ['nodes'],
    queryFn: () => api.get<{ items: LlmNode[] }>('/nodes'),
    refetchInterval: 5000,
  })

  const active = nodeId ?? nodes.data?.items[0]?.id ?? null
  const keys = useQuery({
    queryKey: ['keys', active],
    queryFn: () => api.get<{ items: NodeKey[] }>(`/nodes/${active}/keys`),
    enabled: active != null,
    refetchInterval: 5000,
  })

  const create = useMutation({
    mutationFn: (v: Record<string, unknown>) => api.post(`/nodes/${active}/keys`, v),
    onSuccess: () => {
      message.success('已添加')
      setOpen(false)
      void qc.invalidateQueries({ queryKey: ['keys'] })
    },
    onError: (e: Error) => message.error(e.message),
  })

  const update = useMutation({
    mutationFn: (v: Record<string, unknown> & { id: number }) =>
      api.put(`/keys/${v.id}`, { ...v, id: undefined }),
    onSuccess: () => {
      message.success('已保存')
      setOpen(false)
      void qc.invalidateQueries({ queryKey: ['keys'] })
    },
    onError: (e: Error) => message.error(e.message),
  })

  const clearCd = useMutation({
    mutationFn: (id: number) => api.post(`/keys/${id}/clear-cooldown`),
    onSuccess: () => {
      message.success('已解除冷却')
      void qc.invalidateQueries({ queryKey: ['keys'] })
    },
  })

  const del = useMutation({
    mutationFn: (id: number) => api.del(`/keys/${id}`),
    onSuccess: () => void qc.invalidateQueries({ queryKey: ['keys'] }),
  })

  async function showHistory(k: NodeKey) {
    const r = await api.get<{ items: KeyEvent[] }>(`/keys/${k.id}/events`)
    setEvents({ key: k, list: r.items })
  }

  const items = keys.data?.items ?? []

  return (
    <Space direction="vertical" size={16} style={{ width: '100%' }}>
      <Alert
        type="info"
        showIcon
        message="多把 key 会按「最久未用优先」自动轮换"
        description="某一把撞上额度上限时，网关会把它放进冷却（默认 5 小时）并切到下一把；冷却状态落库，容器重启也不会立刻再撞一次。5 小时是「等」，401/欠费是「修」——后者需要你在这里手动解除。"
      />

      <Card
        title="API Key"
        extra={
          <Space>
            <Select
              style={{ width: 220 }}
              value={active ?? undefined}
              onChange={setNodeId}
              options={(nodes.data?.items ?? []).map((n) => ({ value: n.id, label: n.name }))}
              placeholder="选择节点"
            />
            <Button
              type="primary"
              icon={<PlusOutlined />}
              disabled={active == null}
              onClick={() => {
                setEditing(null)
                form.resetFields()
                form.setFieldsValue({ sort_order: items.length, enabled: true, rate_limit_scope: 'perKey' })
                setOpen(true)
              }}
            >
              添加 key
            </Button>
          </Space>
        }
      >
        <Table<NodeKey>
          rowKey="id"
          dataSource={items}
          pagination={false}
          scroll={{ x: 1100 }}
          columns={[
            {
              title: '标签',
              dataIndex: 'label',
              width: 150,
              render: (v, k) => (
                <Space direction="vertical" size={0}>
                  <Typography.Text strong>{v}</Typography.Text>
                  <Typography.Text type="secondary" style={{ fontSize: 12 }}>
                    {k.masked}
                  </Typography.Text>
                </Space>
              ),
            },
            {
              title: '状态',
              width: 190,
              render: (_, k) => (
                <Space direction="vertical" size={2}>
                  {k.state === 'active' && <Tag color="green">可用</Tag>}
                  {k.state === 'cooling' && (
                    <Tooltip title={k.matched_signal ?? ''}>
                      <Tag color="orange">冷却 {cooldown(k.cooldown_remaining ?? 0)}</Tag>
                    </Tooltip>
                  )}
                  {k.state === 'quarantined' && <Tag color="red">已隔离（人工）</Tag>}
                  {k.state === 'disabled' && <Tag>已停用</Tag>}
                  {!k.enabled && <Tag>开关关闭</Tag>}
                  {k.quota_class && k.state !== 'active' && (
                    <Typography.Text type="secondary" style={{ fontSize: 11 }}>
                      {k.matched_rule} · {k.reset_source} · 至 {yuanTime(k.cooldown_until)}
                    </Typography.Text>
                  )}
                </Space>
              ),
            },
            {
              title: '轮换统计',
              width: 130,
              render: (_, k) => (
                <Typography.Text type="secondary" style={{ fontSize: 12 }}>
                  429 × {k.count_429} · 轮换 × {k.count_rotations}
                  <br />
                  上次使用 {k.last_used_at ? yuanTime(k.last_used_at) : '—'}
                </Typography.Text>
              ),
            },
            {
              title: '软上限',
              width: 150,
              render: (_, k) =>
                k.soft_cap_tokens ? (
                  <Tooltip title="达到软上限后提前切换，不必等真的撞限额">
                    <Tag>
                      {k.window_tokens_used}/{k.soft_cap_tokens} tok
                    </Tag>
                  </Tooltip>
                ) : (
                  <Typography.Text type="secondary">未设</Typography.Text>
                ),
            },
            { title: '排序', dataIndex: 'sort_order', width: 70 },
            {
              title: '操作',
              fixed: 'right',
              width: 160,
              render: (_, k) => (
                <Space size={4}>
                  {k.state !== 'active' && (
                    <Tooltip title="解除冷却/隔离">
                      <Button
                        size="small"
                        icon={<ReloadOutlined />}
                        onClick={() => clearCd.mutate(k.id)}
                      />
                    </Tooltip>
                  )}
                  <Button size="small" icon={<HistoryOutlined />} onClick={() => showHistory(k)} />
                  <Button
                    size="small"
                    icon={<EditOutlined />}
                    onClick={() => {
                      setEditing(k)
                      form.setFieldsValue({
                        label: k.label,
                        sort_order: k.sort_order,
                        enabled: k.enabled,
                        rate_limit_scope: k.rate_limit_scope,
                        soft_cap_tokens: k.soft_cap_tokens,
                        soft_cap_window_ms: k.soft_cap_window_ms,
                      })
                      setOpen(true)
                    }}
                  />
                  <Popconfirm title="删除这把 key？" onConfirm={() => del.mutate(k.id)}>
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
        title={editing ? `编辑 ${editing.label}` : '添加 API Key'}
        onCancel={() => setOpen(false)}
        onOk={() => form.submit()}
        okText="保存"
        destroyOnHidden
      >
        <Form
          form={form}
          layout="vertical"
          onFinish={(v) => (editing ? update.mutate({ ...v, id: editing.id }) : create.mutate(v))}
        >
          {!editing && (
            <Form.Item name="secret" label="Key 明文" rules={[{ required: true }]} extra="只在保存时传输，列表里只回掩码">
              <Input.Password placeholder="sk-..." />
            </Form.Item>
          )}
          <Form.Item name="label" label="标签" rules={[{ required: true }]} extra="同一节点内唯一，用来区分不同订阅账号">
            <Input placeholder="主账号 / 备用账号" />
          </Form.Item>
          <Space size="large" wrap>
            <Form.Item name="sort_order" label="排序">
              <InputNumber min={0} max={99} />
            </Form.Item>
            <Form.Item name="enabled" label="启用" valuePropName="checked">
              <Select
                style={{ width: 90 }}
                options={[
                  { value: true, label: '是' },
                  { value: false, label: '否' },
                ]}
              />
            </Form.Item>
            <Form.Item name="rate_limit_scope" label="限额作用域">
              <Select
                style={{ width: 160 }}
                options={[
                  { value: 'perKey', label: '按 key（可轮换）' },
                  { value: 'account', label: '按账号（不轮换）' },
                ]}
              />
            </Form.Item>
          </Space>
          <Typography.Text strong>软上限（可选）</Typography.Text>
          <Typography.Paragraph type="secondary" style={{ fontSize: 12, marginTop: 0 }}>
            累计 token 接近上限时提前切走，主动避开 429。作用域是「按账号」时轮换无意义，此项不生效。
          </Typography.Paragraph>
          <Space size="large" wrap>
            <Form.Item name="soft_cap_tokens" label="token 上限">
              <InputNumber min={1000} step={1000} />
            </Form.Item>
            <Form.Item name="soft_cap_window_ms" label="窗口（分钟）">
              <InputNumber min={1} max={1440} />
            </Form.Item>
          </Space>
        </Form>
      </Modal>

      <Drawer
        open={!!events}
        onClose={() => setEvents(null)}
        width={640}
        title={`轮换记录 · ${events?.key.label ?? ''}`}
      >
        {events && (
          <>
            <Descriptions size="small" column={2} style={{ marginBottom: 16 }}>
              <Descriptions.Item label="当前状态">{events.key.state}</Descriptions.Item>
              <Descriptions.Item label="限额类型">{events.key.quota_class ?? '—'}</Descriptions.Item>
            </Descriptions>
            <Table<KeyEvent>
              rowKey="id"
              size="small"
              dataSource={events.list}
              pagination={false}
              locale={{ emptyText: '这把 key 还没有轮换记录' }}
              columns={[
                { title: '时间', dataIndex: 'created_at', width: 160, render: yuanTime },
                { title: '判定', dataIndex: 'quota_class', width: 100 },
                { title: '命中规则', dataIndex: 'matched_rule', width: 160 },
                { title: '信号', dataIndex: 'matched_signal', ellipsis: true },
                { title: '冷却', dataIndex: 'cooldown_ms', width: 90, render: (v: number) => cooldown(Math.round(v / 1000)) },
              ]}
            />
          </>
        )}
      </Drawer>
    </Space>
  )
}

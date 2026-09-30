import { useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import {
  Card, Table, Button, Modal, Form, Input, InputNumber, Tabs, Space, Typography, App,
  Popconfirm, Alert, Tag, Row, Col, Statistic, Tooltip,
} from 'antd'
import { DeleteOutlined, PlusOutlined, EditOutlined } from '@ant-design/icons'
import { api, type ModelPrice, type PriceList, type Route, type LlmNode, type BackupRecord } from '../api'
import { bytes, money, yuanTime } from '../format'

export default function Settings() {
  return (
    <Tabs
      items={[
        { key: 'pricing', label: '定价', children: <Pricing /> },
        { key: 'routes', label: '模型路由', children: <Routes /> },
        { key: 'backup', label: '备份', children: <Backup /> },
        { key: 'account', label: '管理员', children: <Admin /> },
      ]}
    />
  )
}

function Pricing() {
  const qc = useQueryClient()
  const { message } = App.useApp()
  const [open, setOpen] = useState(false)
  const [editing, setEditing] = useState<ModelPrice | null>(null)
  const [form] = Form.useForm()

  const q = useQuery({ queryKey: ['prices'], queryFn: () => api.get<PriceList>('/prices') })

  const save = useMutation({
    mutationFn: (v: ModelPrice) => api.post('/prices', v),
    onSuccess: () => {
      message.success('已保存')
      setOpen(false)
      void qc.invalidateQueries({ queryKey: ['prices'] })
    },
    onError: (e: Error) => message.error(e.message),
  })
  const del = useMutation({
    mutationFn: (m: string) => api.del(`/prices/${encodeURIComponent(m)}`),
    onSuccess: () => void qc.invalidateQueries({ queryKey: ['prices'] }),
  })

  const d = q.data

  return (
    <Space direction="vertical" size={16} style={{ width: '100%' }}>
      <Alert
        type="warning"
        showIcon
        message="没配单价的模型会被直接拒绝（402）"
        description="这是刻意的：允许免费调用等于账本对不上。配置以「微元 / 1k token」为单位，1 元 = 1,000,000 微元。缓存命中按 cached_input 单价计，没配就按普通输入价。"
      />
      {d && d.models_without_price.length > 0 && (
        <Alert
          type="error"
          showIcon
          message={`这些模型还没有单价：${d.models_without_price.join('、')}`}
        />
      )}
      <Row gutter={16}>
        <Col xs={24} md={8}>
          <Card size="small" title="全局默认值">
            <Statistic
              title="默认出图单价（微元/张）"
              value={d?.default_image_micro ?? 0}
              valueStyle={{ fontSize: 20 }}
            />
            <Typography.Text type="secondary" style={{ fontSize: 12 }}>
              工作流没单独配价时用这个
            </Typography.Text>
            <Button
              size="small"
              style={{ marginTop: 12 }}
              onClick={() => {
                let v = d?.default_image_micro ?? 0
                // 直接改，避免为一个小数字开整个设置表单
                const next = window.prompt('默认出图单价（微元/张）', String(v))
                if (next == null) return
                v = Number(next)
                if (!Number.isFinite(v) || v < 0) {
                  message.error('请填非负数')
                  return
                }
                void api
                  .put('/settings', { default_image_micro: v })
                  .then(() => {
                    message.success('已保存')
                    void qc.invalidateQueries({ queryKey: ['prices'] })
                  })
                  .catch((e: Error) => message.error(e.message))
              }}
            >
              修改
            </Button>
          </Card>
        </Col>
      </Row>

      <Card
        title="模型单价"
        extra={
          <Button
            type="primary"
            icon={<PlusOutlined />}
            onClick={() => {
              setEditing(null)
              form.resetFields()
              setOpen(true)
            }}
          >
            添加
          </Button>
        }
      >
        <Table<ModelPrice>
          rowKey="model_name"
          dataSource={d?.items ?? []}
          pagination={false}
          scroll={{ x: 700 }}
          columns={[
            { title: '模型', dataIndex: 'model_name', width: 200 },
            {
              title: '输入（微元/1k）',
              dataIndex: 'input_micro_per_1k',
              width: 140,
              render: (v: number) => <span>{money(v)}</span>,
            },
            {
              title: '输出（微元/1k）',
              dataIndex: 'output_micro_per_1k',
              width: 140,
              render: (v: number) => <span>{money(v)}</span>,
            },
            {
              title: '缓存输入',
              dataIndex: 'cached_input_micro_per_1k',
              width: 140,
              render: (v: number | null) => <span>{v == null ? '按输入价' : money(v)}</span>,
            },
            {
              title: '操作',
              width: 120,
              render: (_, r) => (
                <Space>
                  <Button
                    size="small"
                    icon={<EditOutlined />}
                    onClick={() => {
                      setEditing(r)
                      form.setFieldsValue(r)
                      setOpen(true)
                    }}
                  />
                  <Popconfirm title="删除定价？" onConfirm={() => del.mutate(r.model_name)}>
                    <Button size="small" danger icon={<DeleteOutlined />} />
                  </Popconfirm>
                </Space>
              ),
            },
          ]}
        />
        {(d?.items.length ?? 0) === 0 && (
          <Typography.Text type="secondary">
            还没有任何定价。先在「LLM 节点」里配好路由，模型名会出现在这里。
          </Typography.Text>
        )}
      </Card>

      <Modal
        open={open}
        title={editing ? `编辑 ${editing.model_name}` : '添加定价'}
        onCancel={() => setOpen(false)}
        onOk={() => form.submit()}
        okText="保存"
        destroyOnHidden
      >
        <Form form={form} layout="vertical" onFinish={(v) => save.mutate(v)}>
          <Form.Item name="model_name" label="模型名" rules={[{ required: true }]}>
            <Input placeholder="qwen3-8b" disabled={!!editing} />
          </Form.Item>
          <Space size="large" wrap>
            <Form.Item name="input_micro_per_1k" label="输入（微元/1k）" rules={[{ required: true }]}>
              <InputNumber min={0} step={100} />
            </Form.Item>
            <Form.Item name="output_micro_per_1k" label="输出（微元/1k）" rules={[{ required: true }]}>
              <InputNumber min={0} step={100} />
            </Form.Item>
            <Form.Item name="cached_input_micro_per_1k" label="缓存输入（可空）">
              <InputNumber min={0} step={100} />
            </Form.Item>
          </Space>
          <Typography.Text type="secondary" style={{ fontSize: 12 }}>
            1 元 = 1,000,000 微元。例：1 元/百万输入 token ⇒ 1000 微元/1k。
          </Typography.Text>
        </Form>
      </Modal>
    </Space>
  )
}

function Routes() {
  const qc = useQueryClient()
  const { message } = App.useApp()
  const [open, setOpen] = useState(false)
  const [form] = Form.useForm()

  const q = useQuery({
    queryKey: ['routes'],
    queryFn: () => api.get<{ items: Route[]; models: string[] }>('/routes'),
    refetchInterval: 10000,
  })
  const nodes = useQuery({ queryKey: ['nodes'], queryFn: () => api.get<{ items: LlmNode[] }>('/nodes') })

  const save = useMutation({
    mutationFn: (v: Record<string, unknown>) => api.post('/routes', v),
    onSuccess: () => {
      message.success('已保存')
      setOpen(false)
      void qc.invalidateQueries({ queryKey: ['routes'] })
    },
    onError: (e: Error) => message.error(e.message),
  })
  const del = useMutation({
    mutationFn: (id: number) => api.del(`/routes/${id}`),
    onSuccess: () => void qc.invalidateQueries({ queryKey: ['routes'] }),
  })

  return (
    <Card
      title="模型路由"
      extra={
        <Button
          type="primary"
          icon={<PlusOutlined />}
          onClick={() => {
            form.resetFields()
            form.setFieldsValue({ enabled: true, priority: 0 })
            setOpen(true)
          }}
        >
          添加路由
        </Button>
      }
    >
      <Alert
        type="info"
        showIcon
        style={{ marginBottom: 16 }}
        message="同一个模型可以挂多个节点"
        description="调度时按「并发占用率最低」选点，不是按路由顺序。只要有一个节点的 key 全部限额，那个节点会自动退出选点，不用在这里做任何配置。"
      />
      <Table<Route>
        rowKey="id"
        dataSource={q.data?.items ?? []}
        pagination={false}
        columns={[
          { title: '对外模型名', dataIndex: 'model_name', width: 200 },
          { title: '节点', dataIndex: 'node_name', width: 160 },
          {
            title: '上游模型名',
            dataIndex: 'upstream_model',
            width: 200,
            render: (v: string, r: Route) => (v === r.model_name ? <Tag>{v}</Tag> : <Typography.Text>{v}</Typography.Text>),
          },
          { title: '优先级', dataIndex: 'priority', width: 90 },
          {
            title: '状态',
            width: 90,
            render: (_, r) => (r.enabled ? <Tag color="green">启用</Tag> : <Tag>停用</Tag>),
          },
          {
            title: '操作',
            width: 90,
            render: (_, r) => (
              <Popconfirm title="删除这条路由？" onConfirm={() => del.mutate(r.id)}>
                <Button size="small" danger icon={<DeleteOutlined />} />
              </Popconfirm>
            ),
          },
        ]}
      />

      <Modal open={open} title="添加路由" onCancel={() => setOpen(false)} onOk={() => form.submit()} okText="保存" destroyOnHidden>
        <Form form={form} layout="vertical" onFinish={(v) => save.mutate(v)}>
          <Form.Item
            name="model_name"
            label="对外模型名"
            rules={[{ required: true }]}
            extra="客户端请求里写的名字"
          >
            <Input placeholder="qwen3-8b" />
          </Form.Item>
          <Form.Item name="node_id" label="节点" rules={[{ required: true }]}>
            <Input
              type="number"
              placeholder="节点 id"
              list="node-ids"
            />
            <datalist id="node-ids">
              {(nodes.data?.items ?? []).map((n) => (
                <option key={n.id} value={n.id}>
                  {n.name}
                </option>
              ))}
            </datalist>
          </Form.Item>
          <Form.Item
            name="upstream_model"
            label="上游模型名"
            rules={[{ required: true }]}
            extra="发给节点的真实模型名；与对外名相同时会直接用"
          >
            <Input placeholder="qwen3-8b" />
          </Form.Item>
          <Form.Item name="priority" label="优先级（并列时小的先用）">
            <InputNumber min={0} max={999} />
          </Form.Item>
        </Form>
      </Modal>
    </Card>
  )
}

function Backup() {
  const qc = useQueryClient()
  const { message, modal } = App.useApp()
  const q = useQuery({
    queryKey: ['backups'],
    queryFn: () => api.get<{ items: BackupRecord[]; dir: string }>('/backups'),
  })
  const run = useMutation({
    mutationFn: () => api.post<{ ok: boolean; path: string; size_bytes: number; error: string | null }>('/backups/run'),
    onSuccess: (r) => {
      if (r.ok) message.success(`备份完成：${bytes(r.size_bytes)}`)
      else message.error(`备份失败：${r.error}`)
      void qc.invalidateQueries({ queryKey: ['backups'] })
    },
    onError: (e: Error) => message.error(e.message),
  })
  const purge = useMutation({
    mutationFn: () => api.post<{ removed: number }>('/backups/purge'),
    onSuccess: (r) => {
      message.success(`已清理 ${r.removed} 个过期备份`)
      void qc.invalidateQueries({ queryKey: ['backups'] })
    },
  })

  return (
    <Card
      title="数据库备份"
      extra={
        <Space>
          <Button
            icon={<DeleteOutlined />}
            onClick={() =>
              modal.confirm({
                title: '清理 7 天前的备份？',
                content: '只删除本目录下 7 天以前的 hac-*.db，当前备份不受影响。',
                onOk: () => purge.mutate(),
              })
            }
          >
            清理过期
          </Button>
          <Button type="primary" loading={run.isPending} onClick={() => run.mutate()}>
            立即备份
          </Button>
        </Space>
      }
    >
      <Alert
        type="info"
        showIcon
        style={{ marginBottom: 16 }}
        message="每天自动备份，滚动保留 7 天"
        description={
          <span>
            备份目录：<code>{q.data?.dir ?? '—'}</code>（数据目录下的 <code>backups/</code>，
            挂载数据卷时就在宿主机上）。
            用 SQLite 的 <code>VACUUM INTO</code> 生成一致性快照——WAL 模式下直接拷贝
            数据库文件会得到「主库与 WAL 不匹配」的坏副本，这是最常见的备份翻车方式。
            写入顺序是先写 <code>.tmp</code> 再原子改名，中途失败不会留下「看起来正常其实是半截」的文件。
          </span>
        }
      />
      <Table<BackupRecord>
        rowKey="id"
        dataSource={q.data?.items ?? []}
        pagination={false}
        columns={[
          { title: '开始时间', dataIndex: 'started_at', width: 180, render: yuanTime },
          {
            title: '大小',
            dataIndex: 'size_bytes',
            width: 110,
            render: (v: number) => bytes(v),
          },
          {
            title: '结果',
            width: 110,
            render: (_, r) =>
              r.status === 'ok' ? (
                <Tag color="green">成功</Tag>
              ) : (
                <Tooltip title={r.error ?? ''}>
                  <Tag color="red">失败</Tag>
                </Tooltip>
              ),
          },
          { title: '文件', dataIndex: 'path', ellipsis: true },
        ]}
      />
    </Card>
  )
}

function Admin() {
  const { message } = App.useApp()
  const [form] = Form.useForm()
  const [open, setOpen] = useState(false)

  const change = useMutation({
    mutationFn: (v: { old_password: string; new_password: string }) => api.put('/auth/password', v),
    onSuccess: () => {
      message.success('口令已修改，所有会话已失效，请重新登录')
      setOpen(false)
      setTimeout(() => window.location.reload(), 1200)
    },
    onError: (e: Error) => message.error(e.message),
  })

  return (
    <Card
      title="管理员"
      extra={
        <Button
          icon={<EditOutlined />}
          onClick={() => {
            form.resetFields()
            setOpen(true)
          }}
        >
          修改口令
        </Button>
      }
    >
      <Typography.Paragraph>
        首次启动时用环境变量 <code>HOME_AI_ADMIN_TOKEN</code> 初始化管理员口令，默认为用户名{' '}
        <code>admin</code>。
      </Typography.Paragraph>
      <Typography.Text type="secondary">
        调用方的 API Key 与管理台口令是两套完全独立的凭证，互不通用。
      </Typography.Text>

      <Modal open={open} title="修改口令" onCancel={() => setOpen(false)} onOk={() => form.submit()} okText="确认">
        <Form form={form} layout="vertical" onFinish={(v) => change.mutate(v)}>
          <Form.Item name="old_password" label="原口令" rules={[{ required: true }]}>
            <Input.Password />
          </Form.Item>
          <Form.Item name="new_password" label="新口令" rules={[{ required: true, min: 8, message: '至少 8 个字符' }]}>
            <Input.Password />
          </Form.Item>
          <Typography.Text type="secondary" style={{ fontSize: 12 }}>
            修改后所有已登录会话会立即失效。
          </Typography.Text>
        </Form>
      </Modal>
    </Card>
  )
}

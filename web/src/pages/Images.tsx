import { useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import {
  Card, Table, Button, Modal, Form, Input, InputNumber, Switch, Tag, Space, Typography,
  App, Popconfirm, Tabs, Alert, Tooltip, Select,
} from 'antd'
import { PlusOutlined, EditOutlined, DeleteOutlined, PlayCircleOutlined, NodeIndexOutlined } from '@ant-design/icons'
import { api, type ComfyNode, type Workflow } from '../api'
import { money } from '../format'

export default function Images() {
  return (
    <Tabs
      items={[
        { key: 'endpoints', label: '端点', children: <Endpoints /> },
        { key: 'workflows', label: '工作流', children: <Workflows /> },
      ]}
    />
  )
}

function Endpoints() {
  const qc = useQueryClient()
  const { message } = App.useApp()
  const [open, setOpen] = useState(false)
  const [editing, setEditing] = useState<ComfyNode | null>(null)
  const [form] = Form.useForm()

  const q = useQuery({
    queryKey: ['comfy-nodes'],
    queryFn: () => api.get<{ items: ComfyNode[] }>('/comfy-nodes'),
    refetchInterval: 4000,
  })

  const save = useMutation({
    mutationFn: (v: Record<string, unknown> & { id?: number }) =>
      v.id ? api.put(`/comfy-nodes/${v.id}`, v) : api.post('/comfy-nodes', v),
    onSuccess: () => {
      message.success('已保存')
      setOpen(false)
      void qc.invalidateQueries({ queryKey: ['comfy-nodes'] })
    },
    onError: (e: Error) => message.error(e.message),
  })

  const del = useMutation({
    mutationFn: (id: number) => api.del(`/comfy-nodes/${id}`),
    onSuccess: () => void qc.invalidateQueries({ queryKey: ['comfy-nodes'] }),
  })

  return (
    <Card
      title="ComfyUI 端点"
      extra={
        <Button
          type="primary"
          icon={<PlusOutlined />}
          onClick={() => {
            setEditing(null)
            form.resetFields()
            form.setFieldsValue({ max_concurrency: 1, enabled: true, sort_order: 0 })
            setOpen(true)
          }}
        >
          新建端点
        </Button>
      }
    >
      <Alert
        type="info"
        showIcon
        style={{ marginBottom: 16 }}
        message="一端点一泳道"
        description="每个端点有自己的并发额度与队列，调度按「占用率最低」选。只有在提交任务之前才允许换端点；一旦 ComfyUI 收下任务，失败就是失败——重投会双倍扣费并产生重复图。"
      />
      <Table<ComfyNode>
        rowKey="id"
        dataSource={q.data?.items ?? []}
        pagination={false}
        columns={[
          {
            title: '名称',
            dataIndex: 'name',
            width: 160,
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
            title: '占用',
            width: 110,
            render: (_, n) => (
              <Tag>
                {n.active}/{n.max_concurrency}
                {n.waiting > 0 && ` (+${n.waiting})`}
              </Tag>
            ),
          },
          {
            title: '认证',
            width: 90,
            render: (_, n) => (n.has_password ? <Tag>Basic</Tag> : <Tag color="default">无</Tag>),
          },
          {
            title: '状态',
            width: 150,
            render: (_, n) => (
              <Space direction="vertical" size={2}>
                {!n.enabled && <Tag>已停用</Tag>}
                {n.in_disabled_window && <Tag color="blue">禁用时段</Tag>}
                {n.enabled && !n.in_disabled_window && <Tag color="green">正常</Tag>}
              </Space>
            ),
          },
          {
            title: '操作',
            width: 120,
            render: (_, n) => (
              <Space>
                <Button
                  size="small"
                  icon={<EditOutlined />}
                  onClick={() => {
                    setEditing(n)
                    form.setFieldsValue({
                      name: n.name,
                      base_url: n.base_url,
                      lan_base_url: n.lan_base_url,
                      max_concurrency: n.max_concurrency,
                      enabled: n.enabled,
                      sort_order: n.sort_order,
                    })
                    setOpen(true)
                  }}
                />
                <Popconfirm title="删除端点？" onConfirm={() => del.mutate(n.id)}>
                  <Button size="small" danger icon={<DeleteOutlined />} />
                </Popconfirm>
              </Space>
            ),
          },
        ]}
      />

      <Modal
        open={open}
        title={editing ? `编辑 ${editing.name}` : '新建端点'}
        onCancel={() => setOpen(false)}
        onOk={() => form.submit()}
        okText="保存"
        destroyOnHidden
      >
        <Form form={form} layout="vertical" onFinish={(v) => save.mutate(v)}>
          <Form.Item name="name" label="名称" rules={[{ required: true }]}>
            <Input placeholder="gpu-4090" />
          </Form.Item>
          <Form.Item name="base_url" label="地址" rules={[{ required: true }]} extra="例如 http://192.168.1.30:8188">
            <Input />
          </Form.Item>
          <Form.Item name="lan_base_url" label="内网地址（可选）">
            <Input />
          </Form.Item>
          <Space size="large" wrap>
            <Form.Item name="username" label="用户名">
              <Input placeholder="留空则不启用 Basic 认证" />
            </Form.Item>
            <Form.Item name="password" label="口令" extra={editing ? '留空表示不修改' : undefined}>
              <Input.Password />
            </Form.Item>
          </Space>
          <Space size="large" wrap>
            <Form.Item name="max_concurrency" label="最大并发" rules={[{ required: true }]}>
              <InputNumber min={1} max={64} />
            </Form.Item>
            <Form.Item name="sort_order" label="排序">
              <InputNumber min={0} max={99} />
            </Form.Item>
            <Form.Item name="enabled" label="启用" valuePropName="checked">
              <Switch />
            </Form.Item>
          </Space>
        </Form>
      </Modal>
    </Card>
  )
}

function Workflows() {
  const qc = useQueryClient()
  const { message } = App.useApp()
  const [open, setOpen] = useState(false)
  const [editing, setEditing] = useState<Workflow | null>(null)
  const [full, setFull] = useState<Record<string, unknown> | null>(null)
  const [dryInput, setDryInput] = useState('{\n  "prompt": "a cat"\n}')
  const [dryOut, setDryOut] = useState<string>('')
  const [form] = Form.useForm()

  const q = useQuery({
    queryKey: ['workflows'],
    queryFn: () => api.get<{ items: Workflow[] }>('/workflows'),
  })
  const nodes = useQuery({ queryKey: ['comfy-nodes'], queryFn: () => api.get<{ items: ComfyNode[] }>('/comfy-nodes') })

  const save = useMutation({
    mutationFn: (v: Record<string, unknown> & { id?: number }) =>
      v.id ? api.put(`/workflows/${v.id}`, v) : api.post('/workflows', v),
    onSuccess: () => {
      message.success('已保存')
      setOpen(false)
      void qc.invalidateQueries({ queryKey: ['workflows'] })
    },
    onError: (e: Error) => message.error(e.message),
  })

  const del = useMutation({
    mutationFn: (id: number) => api.del(`/workflows/${id}`),
    onSuccess: () => void qc.invalidateQueries({ queryKey: ['workflows'] }),
  })

  async function openDetail(w: Workflow) {
    const d = await api.get<{ comfy_workflow: string; param_slots: string }>(`/workflows/${w.id}`)
    setFull(JSON.parse(d.comfy_workflow || '{}'))
  }

  async function dryRun(w: Workflow, params: unknown) {
    const r = await api.post<{ ok: boolean; error?: string; workflow?: unknown; used?: string[] }>(
      `/workflows/${w.id}/dry-run`,
      { params },
    )
    setDryOut(
      r.ok
        ? `占位符填充结果（用到的参数：${(r.used ?? []).join(', ') || '无'}）\n\n${JSON.stringify(r.workflow, null, 2)}`
        : `渲染失败：${r.error}`,
    )
  }

  return (
    <>
      <Card
        title="出图工作流"
        extra={
          <Button
            type="primary"
            icon={<PlusOutlined />}
            onClick={() => {
              setEditing(null)
              form.resetFields()
              form.setFieldsValue({ mode: 'template', enabled: true, archive: false, param_slots: [], node_ids: [] })
              setOpen(true)
            }}
          >
            新建工作流
          </Button>
        }
      >
        <Alert
          type="info"
          showIcon
          style={{ marginBottom: 16 }}
          message="模板占位符语法"
          description={
            <span>
              在 ComfyUI workflow JSON 里写 <code>{'{{prompt}}'}</code> 或{' '}
              <code>{'{{width|1024}}'}</code>（管道后是默认值）。整串就是一个占位符时会保留 JSON 类型
              —— <code>{'{{seed}}'}</code> 会变成数字 <code>42</code> 而不是字符串 <code>"42"</code>。
              传了模板没消费的参数会直接报错，避免「设了 seed 却每次出图一样」这类问题被静默吞掉。
            </span>
          }
        />
        <Table<Workflow>
          rowKey="id"
          dataSource={q.data?.items ?? []}
          pagination={false}
          scroll={{ x: 900 }}
          columns={[
            {
              title: '名称',
              dataIndex: 'name',
              width: 150,
              render: (v, w) => (
                <Space direction="vertical" size={0}>
                  <Space>
                    <Typography.Text strong>{v}</Typography.Text>
                    <Tag color={w.mode === 'raw' ? 'purple' : 'blue'}>
                      {w.mode === 'raw' ? 'raw' : '模板'}
                    </Tag>
                    {!w.enabled && <Tag>已停用</Tag>}
                  </Space>
                  <Typography.Text type="secondary" style={{ fontSize: 12 }}>
                    槽位：{(w.param_slots ?? []).join(', ') || '无'}
                  </Typography.Text>
                </Space>
              ),
            },
            {
              title: '可用端点',
              width: 120,
              render: (_, w) =>
                (w.node_ids ?? []).length === 0 ? <Tag>全部</Tag> : <Tag>{w.node_ids.length} 个指定</Tag>,
            },
            {
              title: '单价',
              width: 100,
              render: (_, w) => <span>{money(w.price_micro)} 元/张</span>,
            },
            {
              title: '归档',
              width: 90,
              render: (_, w) => (w.archive ? <Tag color="green">落盘</Tag> : <Tag>仅返回</Tag>),
            },
            {
              title: '操作',
              width: 200,
              render: (_, w) => (
                <Space size={4}>
                  <Tooltip title="查看 workflow JSON">
                    <Button size="small" icon={<NodeIndexOutlined />} onClick={() => openDetail(w)} />
                  </Tooltip>
                  <Tooltip title="试跑（只填充，不提交）">
                    <Button
                      size="small"
                      icon={<PlayCircleOutlined />}
                      onClick={() => {
                        try {
                          void dryRun(w, JSON.parse(dryInput))
                        } catch {
                          message.error('参数不是合法 JSON')
                        }
                      }}
                    />
                  </Tooltip>
                  <Button
                    size="small"
                    icon={<EditOutlined />}
                    onClick={async () => {
                      const d = await api.get<Record<string, string>>(`/workflows/${w.id}`)
                      setEditing(w)
                      form.setFieldsValue({
                        name: w.name,
                        mode: w.mode,
                        node_ids: w.node_ids,
                        param_slots: w.param_slots,
                        price_micro: w.price_micro,
                        enabled: w.enabled,
                        archive: w.archive,
                        archive_retention_days: w.archive_retention_days,
                        comfy_workflow: d.comfy_workflow,
                      })
                      setOpen(true)
                    }}
                  />
                  <Popconfirm title="删除工作流？" onConfirm={() => del.mutate(w.id)}>
                    <Button size="small" danger icon={<DeleteOutlined />} />
                  </Popconfirm>
                </Space>
              ),
            },
          ]}
        />
      </Card>

      <Modal open={!!full} onCancel={() => setFull(null)} footer={null} width={720} title="ComfyUI workflow JSON">
        <pre
          style={{
            maxHeight: 520,
            overflow: 'auto',
            background: 'rgba(127,127,127,.08)',
            padding: 12,
            borderRadius: 8,
            fontSize: 12,
          }}
        >
          {JSON.stringify(full, null, 2)}
        </pre>
      </Modal>

      <Modal
        open={!!dryOut}
        onCancel={() => setDryOut('')}
        footer={
          <Button onClick={() => setDryOut('')} type="primary">
            关闭
          </Button>
        }
        width={720}
        title="模板试跑"
      >
        <Input.TextArea
          rows={4}
          value={dryInput}
          onChange={(e) => setDryInput(e.target.value)}
          style={{ fontFamily: 'monospace', marginBottom: 12 }}
        />
        <pre
          style={{
            maxHeight: 400,
            overflow: 'auto',
            background: 'rgba(127,127,127,.08)',
            padding: 12,
            borderRadius: 8,
            fontSize: 12,
            whiteSpace: 'pre-wrap',
          }}
        >
          {dryOut || '（选一个工作流点「试跑」）'}
        </pre>
      </Modal>

      <Modal
        open={open}
        title={editing ? `编辑 ${editing.name}` : '新建工作流'}
        onCancel={() => setOpen(false)}
        onOk={() => form.submit()}
        okText="保存"
        width={700}
        destroyOnHidden
      >
        <Form form={form} layout="vertical" onFinish={(v) => save.mutate(v)}>
          <Space size="large" wrap>
            <Form.Item name="name" label="名称" rules={[{ required: true }]}>
              <Input placeholder="txt2img" style={{ width: 200 }} />
            </Form.Item>
            <Form.Item name="mode" label="模式">
              <Select
                style={{ width: 160 }}
                options={[
                  { value: 'template', label: '模板（服务端占位符）' },
                  { value: 'raw', label: 'raw（客户端传完整 JSON）' },
                ]}
              />
            </Form.Item>
            <Form.Item name="price_micro" label="单价（微元 / 张）" extra="1 元 = 1000000 微元">
              <InputNumber min={0} step={1000} style={{ width: 200 }} />
            </Form.Item>
          </Space>
          <Form.Item name="node_ids" label="可用端点" extra="留空表示全部启用的端点都参与调度">
            <Select
              mode="multiple"
              options={(nodes.data?.items ?? []).map((n) => ({ value: n.id, label: n.name }))}
              placeholder="不选 = 全部"
            />
          </Form.Item>
          <Form.Item
            name="param_slots"
            label="允许的参���槽"
            extra="只允许注入这里列出的参数名，调用方传别的会被拒绝"
            rules={[{ required: true }]}
          >
            <Select mode="tags" placeholder="prompt / negative / seed / width" open={false} />
          </Form.Item>
          <Form.Item
            name="comfy_workflow"
            label="ComfyUI workflow（API 格式 JSON）"
            extra='形如 {"1":{"class_type":"TextEncode","inputs":{"text":"{{prompt}}"}}}'
          >
            <Input.TextArea rows={8} style={{ fontFamily: 'monospace' }} />
          </Form.Item>
          <Space size="large" wrap>
            <Form.Item name="enabled" label="启用" valuePropName="checked">
              <Switch />
            </Form.Item>
            <Form.Item
              name="archive"
              label="图片落盘归档"
              valuePropName="checked"
              extra="关掉就只在响应里返回 base64，不占磁盘"
            >
              <Switch />
            </Form.Item>
            <Form.Item name="archive_retention_days" label="归档保留天数">
              <InputNumber min={1} max={3650} placeholder="留空=永久" />
            </Form.Item>
          </Space>
        </Form>
      </Modal>
    </>
  )
}

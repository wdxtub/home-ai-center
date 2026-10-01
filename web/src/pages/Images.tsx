import { useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import {
  Card, Table, Button, Modal, Form, Input, InputNumber, Switch, Tag, Space, Typography,
  App, Popconfirm, Tabs, Alert, Tooltip, Select,
} from 'antd'
import { PlusOutlined, EditOutlined, DeleteOutlined, PlayCircleOutlined, NodeIndexOutlined } from '@ant-design/icons'
import { api, type ComfyNode, type Workflow, type WorkflowList } from '../api'
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
    queryFn: () => api.get<WorkflowList>('/workflows'),
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
    const d = await api.get<{ comfy_workflow: string }>(`/workflows/${w.id}`)
    setFull(JSON.parse(d.comfy_workflow || '{}'))
  }

  async function dryRun(w: Workflow, params: unknown) {
    const r = await api.post<{
      ok: boolean
      error?: string
      workflow?: unknown
      used?: string[]
      unresolved?: string[]
    }>(`/workflows/${w.id}/dry-run`, { params })

    // 未解析的占位符要顶在最前面说：它不会让试跑失败，但会让 ComfyUI
    // 把 "{{negative_prompt}}" 这串字面量当成提示词用，图废了还不报错。
    const unresolved = r.unresolved ?? []
    const warn =
      unresolved.length > 0
        ? `⚠️ 未解析的占位符：${unresolved.join(', ')}\n` +
          `   这些不在可传参数（prompt / size / seed）里，调用方也传不了。\n` +
          `   请在模板里给它们加默认值，例如 {{negative_prompt|}}。\n\n`
        : ''

    setDryOut(
      r.ok
        ? `${warn}占位符填充结果（用到的参数：${(r.used ?? []).join(', ') || '无'}）\n\n${JSON.stringify(r.workflow, null, 2)}`
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
              form.setFieldsValue({ mode: 'template', enabled: true, node_ids: [] })
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
          message="调用方选的是模型，不是工作流"
          description={
            <span>
              调用方在请求体顶层传 <code>model</code>，网关从模板里的 checkpoint
              文件名推出这个值并用它选工作流（只认 UnetLoader / CheckpointLoader
              这类主模型加载器，VAE 和 CLIP 不算）。<b>改 checkpoint 文件名就等于改
              model</b>，现有调用方会立刻 404。只有一个启用的模型时不传也能用，多于一个
              必须显式指定。可传参数只有 <code>prompt</code> / <code>size</code> /{' '}
              <code>seed</code>，占位符写 <code>{'{{prompt}}'}</code> 或{' '}
              <code>{'{{width|1024}}'}</code>（管道后是默认值）。
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
              width: 210,
              render: (v, w) => (
                <Space direction="vertical" size={0}>
                  <Space>
                    <Typography.Text strong>{v}</Typography.Text>
                    <Tag color={w.mode === 'raw' ? 'purple' : 'blue'}>
                      {w.mode === 'raw' ? 'raw' : '模板'}
                    </Tag>
                    {!w.enabled && <Tag>已停用</Tag>}
                  </Space>
                  <Space size={4} wrap>
                    {/* 调用方传的是 model，不是这个名字 */}
                    {w.mode === 'raw' ? (
                      <Typography.Text type="secondary" style={{ fontSize: 12 }}>
                        raw 通道不按模型选
                      </Typography.Text>
                    ) : w.model ? (
                      <Tag color="gold" style={{ fontSize: 12 }}>
                        model: {w.model}
                      </Tag>
                    ) : (
                      <Tooltip title={w.model_error}>
                        <Tag color="red" style={{ fontSize: 12 }}>
                          推不出模型名
                        </Tag>
                      </Tooltip>
                    )}
                  </Space>
                  <Typography.Text type="secondary" style={{ fontSize: 12 }}>
                    可传参数：{(w.allowed_params ?? []).join(', ')}
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
                        price_micro: w.price_micro,
                        enabled: w.enabled,
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
            name="comfy_workflow"
            label="ComfyUI workflow（API 格式 JSON）"
            extra={
              <>
                形如{' '}
                <code>{'{"1":{"class_type":"TextEncode","inputs":{"text":"{{prompt}}"}}}'}</code>
                。可传参数固定为 <code>prompt / size / seed</code>，其中 <code>size</code> 只有 <code>3x4</code>（768×1024）与 <code>16x9</code>（1024×576）两组，由网关展开成模板里的宽高占位符；
                别的值（反向提示词、步数…）要固定住就写成 <code>{'{{negative_prompt|}}'}</code>{' '}
                这种带默认值的占位符，否则试跑会报「未解析」。
              </>
            }
          >
            <Input.TextArea rows={8} style={{ fontFamily: 'monospace' }} />
          </Form.Item>
          <Space size="large" wrap>
            <Form.Item name="enabled" label="启用" valuePropName="checked">
              <Switch />
            </Form.Item>
          </Space>
        </Form>
      </Modal>
    </>
  )
}

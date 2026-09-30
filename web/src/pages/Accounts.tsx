import { useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import {
  Card, Table, Button, Modal, Form, Input, InputNumber, Switch, Tag, Space, Typography,
  App, Popconfirm, Drawer, Descriptions, Alert, InputNumber as Num, Tooltip,
} from 'antd'
import { PlusOutlined, EditOutlined, DeleteOutlined, KeyOutlined, HistoryOutlined } from '@ant-design/icons'
import { api, type Account, type BalanceTxn } from '../api'
import { money, yuan, yuanTime } from '../format'

interface AccForm {
  name: string
  enabled: boolean
  llm_max_concurrency: number
  llm_max_queue: number
  image_max_concurrency: number
  image_max_queue: number
}

const BLANK: AccForm = {
  name: '',
  enabled: true,
  llm_max_concurrency: 2,
  llm_max_queue: 8,
  image_max_concurrency: 1,
  image_max_queue: 4,
}

export default function Accounts() {
  const qc = useQueryClient()
  const { message, modal } = App.useApp()
  const [editing, setEditing] = useState<Account | null>(null)
  const [open, setOpen] = useState(false)
  const [form] = Form.useForm<AccForm>()
  const [topupFor, setTopupFor] = useState<Account | null>(null)
  const [topupAmt, setTopupAmt] = useState<number>(10)
  const [txns, setTxns] = useState<{ acc: Account; list: BalanceTxn[] } | null>(null)
  const [freshKey, setFreshKey] = useState<{ name: string; key: string } | null>(null)

  const accounts = useQuery({
    queryKey: ['accounts'],
    queryFn: () => api.get<{ items: Account[] }>('/accounts'),
    refetchInterval: 5000,
  })

  const create = useMutation({
    mutationFn: (v: AccForm) => api.post<{ id: number; api_key: string }>('/accounts', v),
    onSuccess: (r) => {
      setFreshKey({ name: form.getFieldValue('name'), key: r.api_key })
      setOpen(false)
      void qc.invalidateQueries({ queryKey: ['accounts'] })
    },
    onError: (e: Error) => message.error(e.message),
  })

  const update = useMutation({
    mutationFn: (v: AccForm & { id: number }) => api.put(`/accounts/${v.id}`, v),
    onSuccess: () => {
      message.success('已保存，并发额度立即生效')
      setOpen(false)
      void qc.invalidateQueries({ queryKey: ['accounts'] })
    },
    onError: (e: Error) => message.error(e.message),
  })

  const rotate = useMutation({
    mutationFn: (id: number) => api.post<{ api_key: string }>(`/accounts/${id}/key`, {}),
    onSuccess: (r) => {
      setFreshKey({ name: '（旧 key 已失效）', key: r.api_key })
      void qc.invalidateQueries({ queryKey: ['accounts'] })
    },
  })

  const topup = useMutation({
    mutationFn: (a: Account) =>
      api.post(`/accounts/${a.id}/topup`, {
        amount_micro: Math.round(topupAmt * 1_000_000),
        note: '管理台充值',
      }),
    onSuccess: () => {
      message.success('已充值')
      setTopupFor(null)
      void qc.invalidateQueries({ queryKey: ['accounts'] })
    },
    onError: (e: Error) => message.error(e.message),
  })

  const del = useMutation({
    mutationFn: (id: number) => api.del(`/accounts/${id}`),
    onSuccess: () => {
      message.success('已删除')
      void qc.invalidateQueries({ queryKey: ['accounts'] })
    },
  })

  async function showTxns(a: Account) {
    const r = await api.get<{ items: BalanceTxn[] }>(`/accounts/${a.id}/txns`)
    setTxns({ acc: a, list: r.items })
  }

  const items = accounts.data?.items ?? []

  return (
    <Space direction="vertical" size={16} style={{ width: '100%' }}>
      <Alert
        type="info"
        showIcon
        message="计费口径"
        description="预付费余额硬扣减：准入时按「最大可能费用」冻结，结束后按真实 token 结算并退差。余额不足直接返回 402，不会透支。在途冻结金额会显示在「可用」里。"
      />

      <Card
        title="账号"
        extra={
          <Button
            type="primary"
            icon={<PlusOutlined />}
            onClick={() => {
              setEditing(null)
              form.setFieldsValue(BLANK)
              setOpen(true)
            }}
          >
            新建账号
          </Button>
        }
      >
        <Table<Account>
          rowKey="id"
          dataSource={items}
          pagination={false}
          scroll={{ x: 1000 }}
          columns={[
            {
              title: '名称',
              dataIndex: 'name',
              width: 160,
              render: (v, a) => (
                <Space direction="vertical" size={0}>
                  <Space>
                    <Typography.Text strong>{v}</Typography.Text>
                    {!a.enabled && <Tag>已停用</Tag>}
                  </Space>
                  <Typography.Text type="secondary" style={{ fontSize: 12 }}>
                    {a.api_key_prefix}…
                  </Typography.Text>
                </Space>
              ),
            },
            {
              title: '余额',
              width: 160,
              render: (_, a) => (
                <Space direction="vertical" size={0}>
                  <Typography.Text strong>{money(a.balance_micro)} 元</Typography.Text>
                  {a.held_micro > 0 && (
                    <Typography.Text type="warning" style={{ fontSize: 12 }}>
                      冻结 {money(a.held_micro)}（可用 {money(a.available_micro)}）
                    </Typography.Text>
                  )}
                </Space>
              ),
            },
            {
              title: 'LLM 并发',
              width: 100,
              render: (_, a) => <Tag>{a.llm_max_concurrency} / 排队 {a.llm_max_queue}</Tag>,
            },
            {
              title: '出图并发',
              width: 100,
              render: (_, a) => <Tag>{a.image_max_concurrency} / 排队 {a.image_max_queue}</Tag>,
            },
            { title: '创建', dataIndex: 'created_at', width: 160, render: yuanTime },
            {
              title: '操作',
              fixed: 'right',
              width: 190,
              render: (_, a) => (
                <Space size={4}>
                  <Button size="small" onClick={() => setTopupFor(a)}>
                    充值
                  </Button>
                  <TooltipBtn title="查看流水" onClick={() => showTxns(a)}>
                    <HistoryOutlined />
                  </TooltipBtn>
                  <TooltipBtn
                    title="换 key（旧 key 立即失效）"
                    onClick={() =>
                      modal.confirm({
                        title: `给「${a.name}」换一把新 key？`,
                        content: '旧 key 会立刻失效，需要同步更新所有调用方的配置。',
                        okText: '换',
                        onOk: () => rotate.mutate(a.id),
                      })
                    }
                  >
                    <KeyOutlined />
                  </TooltipBtn>
                  <Button
                    size="small"
                    icon={<EditOutlined />}
                    onClick={() => {
                      setEditing(a)
                      form.setFieldsValue({
                        name: a.name,
                        enabled: a.enabled,
                        llm_max_concurrency: a.llm_max_concurrency,
                        llm_max_queue: a.llm_max_queue,
                        image_max_concurrency: a.image_max_concurrency,
                        image_max_queue: a.image_max_queue,
                      })
                      setOpen(true)
                    }}
                  />
                  <Popconfirm
                    title="删除账号？"
                    description="它的调用明细会一并删除，无法恢复。"
                    onConfirm={() => del.mutate(a.id)}
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
        title={editing ? `编辑 ${editing.name}` : '新建账号'}
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
          <Form.Item name="name" label="名称" rules={[{ required: true }]}>
            <Input placeholder="wife-laptop" />
          </Form.Item>
          <Typography.Text strong>并发额度</Typography.Text>
          <Typography.Paragraph type="secondary" style={{ fontSize: 12, marginTop: 0 }}>
            排队上限 = 队列里最多等多少个请求。超了直接返回 429，不无限堆积。
          </Typography.Paragraph>
          <Space size="large" wrap>
            <Form.Item name="llm_max_concurrency" label="LLM 并发" rules={[{ required: true }]}>
              <Num min={1} max={64} />
            </Form.Item>
            <Form.Item name="llm_max_queue" label="LLM 排队">
              <Num min={0} max={256} />
            </Form.Item>
            <Form.Item name="image_max_concurrency" label="出图并发">
              <Num min={1} max={64} />
            </Form.Item>
            <Form.Item name="image_max_queue" label="出图排队">
              <Num min={0} max={256} />
            </Form.Item>
          </Space>
          <Form.Item name="enabled" label="启用" valuePropName="checked">
            <Switch />
          </Form.Item>
        </Form>
      </Modal>

      <Modal
        open={!!topupFor}
        title={`给「${topupFor?.name}」充值`}
        onCancel={() => setTopupFor(null)}
        onOk={() => topupFor && topup.mutate(topupFor)}
        okText="确认充值"
      >
        <InputNumber
          style={{ width: '100%' }}
          addonBefore="金额（元）"
          min={0.000001}
          step={1}
          value={topupAmt}
          onChange={(v) => setTopupAmt(v ?? 0)}
        />
        <Typography.Paragraph type="secondary" style={{ marginTop: 12, marginBottom: 0 }}>
          当前余额 {yuan(topupFor?.balance_micro ?? 0)}，充值后{' '}
          {yuan((topupFor?.balance_micro ?? 0) + Math.round(topupAmt * 1_000_000))}。
          填写负数表示扣减。
        </Typography.Paragraph>
      </Modal>

      <Modal
        open={!!freshKey}
        title="新 key（只显示这一次）"
        onCancel={() => setFreshKey(null)}
        footer={[
          <Button key="copy" onClick={() => navigator.clipboard.writeText(freshKey?.key ?? '')}>
            复制
          </Button>,
          <Button key="ok" type="primary" onClick={() => setFreshKey(null)}>
            我已保存
          </Button>,
        ]}
        closable={false}
        maskClosable={false}
      >
        <Alert
          type="warning"
          showIcon
          message="库里只存哈希，明文无法找回"
          style={{ marginBottom: 12 }}
        />
        <Typography.Paragraph copyable={{ text: freshKey?.key ?? '' }}>
          <Typography.Text code style={{ fontSize: 15 }}>
            {freshKey?.key}
          </Typography.Text>
        </Typography.Paragraph>
        <Typography.Text type="secondary">
          调用方式：<code>Authorization: Bearer &lt;key&gt;</code>（OpenAI 客户端）或{' '}
          <code>x-api-key: &lt;key&gt;</code>（Anthropic 客户端）
        </Typography.Text>
      </Modal>

      <Drawer
        open={!!txns}
        onClose={() => setTxns(null)}
        width={680}
        title={`余额流水 · ${txns?.acc.name ?? ''}`}
      >
        {txns && (
          <>
            <Descriptions size="small" column={2} style={{ marginBottom: 16 }}>
              <Descriptions.Item label="当前余额">{yuan(txns.acc.balance_micro)}</Descriptions.Item>
              <Descriptions.Item label="在途冻结">{yuan(txns.acc.held_micro)}</Descriptions.Item>
            </Descriptions>
            <Table<BalanceTxn>
              rowKey="id"
              size="small"
              dataSource={txns.list}
              pagination={{ pageSize: 20 }}
              columns={[
                { title: '时间', dataIndex: 'created_at', width: 160, render: yuanTime },
                { title: '类型', dataIndex: 'kind', width: 90 },
                {
                  title: '金额',
                  dataIndex: 'amount_micro',
                  width: 120,
                  render: (v: number) => (
                    <Typography.Text type={v >= 0 ? 'success' : 'danger'}>
                      {v >= 0 ? '+' : ''}
                      {money(v)}
                    </Typography.Text>
                  ),
                },
                { title: '余额', dataIndex: 'balance_after', width: 120, render: (v: number) => money(v) },
                { title: '备注', dataIndex: 'note', ellipsis: true },
              ]}
            />
          </>
        )}
      </Drawer>
    </Space>
  )
}

function TooltipBtn({ title, onClick, children }: { title: string; onClick: () => void; children: React.ReactNode }) {
  return (
    <Tooltip title={title}>
      <Button size="small" icon={children as never} onClick={onClick} />
    </Tooltip>
  )
}

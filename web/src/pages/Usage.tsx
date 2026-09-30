import { useState } from 'react'
import { useQuery } from '@tanstack/react-query'
import {
  Card, Table, Tag, Space, Typography, Select, DatePicker, Input, Row, Col, Statistic, Space as AntSpace,
} from 'antd'
import dayjs, { type Dayjs } from 'dayjs'
import { api, type LogRow, type SummaryRow, type Account } from '../api'
import { money, ms, yuanTime } from '../format'

const RANGES: Record<string, [number, number]> = {
  今天: [0, 0],
  '近 7 天': [6, 0],
  '近 30 天': [29, 0],
}

export default function Usage() {
  const [accountId, setAccountId] = useState<number | undefined>()
  const [status, setStatus] = useState<string | undefined>()
  const [protocol, setProtocol] = useState<string | undefined>()
  const [target, setTarget] = useState<string | undefined>()
  const [range, setRange] = useState<[Dayjs, Dayjs] | null>(null)
  const [page, setPage] = useState(1)

  const accounts = useQuery({ queryKey: ['accounts'], queryFn: () => api.get<{ items: Account[] }>('/accounts') })

  const from = range ? range[0].startOf('day').unix() : undefined
  const to = range ? range[1].endOf('day').unix() : undefined

  const logs = useQuery({
    queryKey: ['logs', accountId, status, protocol, target, from, to, page],
    queryFn: () => {
      const p = new URLSearchParams({ page: String(page), page_size: '50' })
      if (accountId != null) p.set('account_id', String(accountId))
      if (status) p.set('status', status)
      if (protocol) p.set('protocol', protocol)
      if (target) p.set('target', target)
      if (from != null) p.set('from', String(from))
      if (to != null) p.set('to', String(to))
      return api.get<{ items: LogRow[]; total: number }>(`/logs?${p}`)
    },
  })

  const summary = useQuery({
    queryKey: ['summary', accountId, range],
    queryFn: () => {
      const p = new URLSearchParams()
      if (accountId != null) p.set('account_id', String(accountId))
      if (range) {
        p.set('from_day', range[0].format('YYYY-MM-DD'))
        p.set('to_day', range[1].format('YYYY-MM-DD'))
      }
      return api.get<{ items: SummaryRow[] }>(`/usage/summary?${p}`)
    },
  })

  const rows = summary.data?.items ?? []
  const totals = rows.reduce(
    (a, r) => ({
      cost: a.cost + r.cost_micro,
      requests: a.requests + r.requests,
      errors: a.errors + r.errors,
      tokens: a.tokens + r.prompt_tokens + r.completion_tokens,
      images: a.images + r.image_count,
    }),
    { cost: 0, requests: 0, errors: 0, tokens: 0, images: 0 },
  )

  // 按天聚合成趋势
  const byDay = new Map<string, { day: string; cost_micro: number; requests: number; tokens: number }>()
  for (const r of rows) {
    const cur = byDay.get(r.day) ?? { day: r.day, cost_micro: 0, requests: 0, tokens: 0 }
    cur.cost_micro += r.cost_micro
    cur.requests += r.requests
    cur.tokens += r.prompt_tokens + r.completion_tokens
    byDay.set(r.day, cur)
  }
  const trend = [...byDay.values()].sort((a, b) => a.day.localeCompare(b.day))

  return (
    <Space direction="vertical" size={16} style={{ width: '100%' }}>
      <Card size="small">
        <AntSpace wrap>
          <Select
            allowClear
            style={{ width: 160 }}
            placeholder="全部账号"
            value={accountId}
            onChange={(v) => {
              setAccountId(v)
              setPage(1)
            }}
            options={(accounts.data?.items ?? []).map((a) => ({ value: a.id, label: a.name }))}
          />
          <Select
            allowClear
            style={{ width: 120 }}
            placeholder="全部状态"
            value={status}
            onChange={(v) => {
              setStatus(v)
              setPage(1)
            }}
            options={[
              { value: 'ok', label: '成功' },
              { value: 'error', label: '失败' },
            ]}
          />
          <Select
            allowClear
            style={{ width: 140 }}
            placeholder="全部协议"
            value={protocol}
            onChange={(v) => {
              setProtocol(v)
              setPage(1)
            }}
            options={[
              { value: 'chat', label: 'Chat' },
              { value: 'responses', label: 'Responses' },
              { value: 'messages', label: 'Anthropic' },
            ]}
          />
          <Input
            allowClear
            style={{ width: 180 }}
            placeholder="模型 / 工作流名"
            value={target}
            onChange={(e) => {
              setTarget(e.target.value || undefined)
              setPage(1)
            }}
          />
          <DatePicker.RangePicker
            presets={Object.entries(RANGES).map(([k, v]) => ({
              label: k,
              value: [dayjs().subtract(v[0], 'day'), dayjs().subtract(v[1], 'day')] as [Dayjs, Dayjs],
            }))}
            onChange={(v) => {
              setRange(v as [Dayjs, Dayjs] | null)
              setPage(1)
            }}
          />
        </AntSpace>
      </Card>

      <Row gutter={[16, 16]}>
        <Col xs={12} md={6}>
          <Card size="small">
            <Statistic title="区间消耗" value={money(totals.cost)} suffix="元" precision={4} />
          </Card>
        </Col>
        <Col xs={12} md={6}>
          <Card size="small">
            <Statistic title="请求数" value={totals.requests} suffix={`（失败 ${totals.errors}）`} />
          </Card>
        </Col>
        <Col xs={12} md={6}>
          <Card size="small">
            <Statistic title="Token" value={totals.tokens} />
          </Card>
        </Col>
        <Col xs={12} md={6}>
          <Card size="small">
            <Statistic title="图片" value={totals.images} suffix="张" />
          </Card>
        </Col>
      </Row>

      {trend.length > 1 && (
        <Card title="每日趋势" size="small">
          <DayChart data={trend} />
        </Card>
      )}

      <Card title="调用明细" size="small">
        <Typography.Paragraph type="secondary" style={{ fontSize: 12, marginTop: 0 }}>
          **只记录元数据，不存 prompt / completion 正文**——这是产品决策：正文是最占空间的部分，
          而排查问题时需要的「谁、什么时候、调了哪个模型、多少钱」这些都在这里。
        </Typography.Paragraph>
        <Table<LogRow>
          rowKey="id"
          size="small"
          dataSource={logs.data?.items ?? []}
          scroll={{ x: 1400 }}
          pagination={{
            current: page,
            pageSize: 50,
            total: logs.data?.total ?? 0,
            onChange: setPage,
            showSizeChanger: false,
          }}
          columns={[
            { title: '时间', dataIndex: 'created_at', width: 160, render: yuanTime, fixed: 'left' },
            {
              title: '类型',
              dataIndex: 'kind',
              width: 80,
              render: (v: string) => <Tag color={v === 'llm' ? 'blue' : 'purple'}>{v}</Tag>,
            },
            {
              title: '协议',
              dataIndex: 'protocol',
              width: 90,
              render: (v: string | null) =>
                v ? <Tag>{v === 'messages' ? 'anthropic' : v}</Tag> : <Tag>—</Tag>,
            },
            { title: '目标', dataIndex: 'target', width: 150 },
            {
              title: '节点',
              dataIndex: 'node_name',
              width: 130,
              render: (v: string | null) => v ?? '—',
            },
            {
              title: '方式',
              width: 100,
              render: (_, r) => (
                <Space size={2}>
                  {r.stream && <Tag>流式</Tag>}
                  {r.rotated_count > 0 && <Tag color="orange">轮换 {r.rotated_count}</Tag>}
                </Space>
              ),
            },
            {
              title: '用量',
              width: 190,
              render: (_, r) => (
                <Typography.Text type="secondary" style={{ fontSize: 12 }}>
                  {r.kind === 'image'
                    ? `${r.image_count} 张图`
                    : `↑${r.prompt_tokens} ↓${r.completion_tokens}`}
                  {r.cached_tokens > 0 && ` (缓存 ${r.cached_tokens})`}
                  <br />
                  {r.tokens_source === 'estimated' && (
                    <Tag color="orange" style={{ fontSize: 11 }}>
                      上游未给 usage，按估算计费
                    </Tag>
                  )}
                </Typography.Text>
              ),
            },
            {
              title: '排队/延迟',
              width: 130,
              render: (_, r) => (
                <Typography.Text type="secondary" style={{ fontSize: 12 }}>
                  {ms(r.queue_wait_ms)} / {ms(r.latency_ms)}
                </Typography.Text>
              ),
            },
            {
              title: '费用',
              dataIndex: 'cost_micro',
              width: 100,
              render: (v: number) => <span>{money(v)}</span>,
            },
            {
              title: '结果',
              width: 110,
              render: (_, r) =>
                r.status === 'ok' ? (
                  <Tag color="green">成功</Tag>
                ) : (
                  <Tooltipish title={r.error_message ?? r.error_kind ?? ''}>
                    <Tag color="red">失败</Tag>
                  </Tooltipish>
                ),
            },
          ]}
        />
      </Card>
    </Space>
  )
}

function Tooltipish({ title, children }: { title: string; children: React.ReactNode }) {
  return <span title={title}>{children}</span>
}

function DayChart({ data }: { data: { day: string; cost_micro: number; requests: number; tokens: number }[] }) {
  // recharts 体积不小且只在这一页用，动态载入避免拖慢首屏
  const [C, setC] = useState<any>(null)
  useState(() => {
    void import('recharts').then((m) => {
      setC({
        Area: m.AreaChart,
        Bar: m.BarChart,
        XAxis: m.XAxis,
        YAxis: m.YAxis,
        Tooltip: m.Tooltip,
        CartesianGrid: m.CartesianGrid,
        Legend: m.Legend,
        ResponsiveContainer: m.ResponsiveContainer,
      })
    })
  })
  if (!C) return <Typography.Text type="secondary">图表加载中…</Typography.Text>
  const { AreaChart, XAxis, YAxis, Tooltip, CartesianGrid, ResponsiveContainer, Area } = C
  return (
    <ResponsiveContainer width="100%" height={220}>
      <AreaChart data={data}>
        <CartesianGrid strokeDasharray="3 3" opacity={0.2} />
        <XAxis dataKey="day" fontSize={12} />
        <YAxis fontSize={12} />
        <Tooltip formatter={(v: number) => money(v)} />
        <Area type="monotone" dataKey="cost_micro" stroke="#4f7cff" fill="#4f7cff33" name="费用(微元)" />
      </AreaChart>
    </ResponsiveContainer>
  )
}

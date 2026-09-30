import { useQuery } from '@tanstack/react-query'
import { Row, Col, Card, Statistic, Table, Tag, Progress, Typography, Space, Alert } from 'antd'
import { api, type Overview, type RuntimeView, type SummaryRow } from '../api'
import { yuan, money, cooldown } from '../format'

/** 总览页：一眼看清「今天花了多少、谁在跑、有没有人在排队」。 */
export default function Overview() {
  const ov = useQuery({
    queryKey: ['overview'],
    queryFn: () => api.get<Overview>('/overview'),
    refetchInterval: 5000,
  })
  const rt = useQuery({
    queryKey: ['runtime'],
    queryFn: () => api.get<RuntimeView>('/runtime'),
    refetchInterval: 2000,
  })
  const sum = useQuery({
    queryKey: ['summary-recent'],
    queryFn: () => api.get<{ items: SummaryRow[] }>('/usage/summary'),
  })

  const r = ov.data
  const cooling = r ? r.nodes.total - r.nodes.healthy : 0

  return (
    <Space direction="vertical" size={16} style={{ width: '100%' }}>
      {cooling > 0 && (
        <Alert
          type="warning"
          showIcon
          message={`${cooling} 个节点正在冷却退避中`}
          description="网关会在冷却结束后自动重新纳入调度；持续冷却说明节点真的有问题，去「LLM 节点」页看最后一条错误。"
        />
      )}

      <Row gutter={[16, 16]}>
        <Col xs={12} md={6}>
          <Card>
            <Statistic
              title="今日消耗"
              value={money(r?.today.cost_micro ?? 0)}
              suffix="元"
              precision={4}
            />
            <Typography.Text type="secondary">
              {r?.today.requests ?? 0} 次请求 · {r?.today.images ?? 0} 张图
              {r && r.today.errors > 0 && ` · ${r.today.errors} 次失败`}
            </Typography.Text>
          </Card>
        </Col>
        <Col xs={12} md={6}>
          <Card>
            <Statistic
              title="本月消耗"
              value={money(r?.month.cost_micro ?? 0)}
              suffix="元"
              precision={4}
            />
            <Typography.Text type="secondary">
              {r?.month.requests ?? 0} 次 · {r?.month.tokens ?? 0} tokens
            </Typography.Text>
          </Card>
        </Col>
        <Col xs={12} md={6}>
          <Card>
            <Statistic title="账号总余额" value={money(r?.balance_micro ?? 0)} suffix="元" precision={4} />
            <Typography.Text type="secondary">
              在途冻结 {yuan(r?.held_micro ?? 0)}
            </Typography.Text>
          </Card>
        </Col>
        <Col xs={12} md={6}>
          <Card>
            <Statistic
              title="资源"
              value={`${r?.nodes.healthy ?? 0}/${r?.nodes.total ?? 0}`}
              suffix="节点在线"
            />
            <Typography.Text type="secondary">
              {r?.comfy_nodes ?? 0} 个出图端点 · {r?.models ?? 0} 个模型 · {r?.workflows ?? 0} 个工作流
            </Typography.Text>
          </Card>
        </Col>
      </Row>

      <Card title="实时占用" size="small">
        <RuntimeTables rt={rt.data} />
      </Card>

      <Card title="最近用量（按天）" size="small">
        <Table<SummaryRow>
          size="small"
          rowKey={(x) => `${x.day}-${x.account_id}-${x.target}-${x.node_id}`}
          dataSource={(sum.data?.items ?? []).slice(0, 20)}
          pagination={false}
          columns={[
            { title: '日期', dataIndex: 'day', width: 120 },
            { title: '目标', dataIndex: 'target' },
            { title: '请求', dataIndex: 'requests', width: 90 },
            { title: '失败', dataIndex: 'errors', width: 80 },
            { title: '输入', dataIndex: 'prompt_tokens', width: 100 },
            { title: '输出', dataIndex: 'completion_tokens', width: 100 },
            { title: '图片', dataIndex: 'image_count', width: 80 },
            {
              title: '费用',
              dataIndex: 'cost_micro',
              width: 120,
              render: (v: number) => <span>{money(v)} 元</span>,
            },
          ]}
        />
      </Card>
    </Space>
  )
}

function RuntimeTables({ rt }: { rt?: RuntimeView }) {
  if (!rt) return <Typography.Text type="secondary">加载中…</Typography.Text>
  return (
    <Row gutter={[24, 24]}>
      <Col xs={24} lg={12}>
        <Typography.Text strong>LLM 节点</Typography.Text>
        <Table
          size="small"
          rowKey="id"
          pagination={false}
          dataSource={rt.nodes}
          columns={[
            { title: '名称', dataIndex: 'name' },
            {
              title: '占用',
              width: 170,
              render: (_, n) => (
                <Progress
                  percent={Math.round((n.active / Math.max(1, n.max_concurrency)) * 100)}
                  size="small"
                  status={n.in_disabled_window ? 'normal' : undefined}
                />
              ),
            },
            { title: '等待', dataIndex: 'waiting', width: 70 },
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
              width: 110,
              render: (_, n) => {
                if (n.in_disabled_window) return <Tag>禁用时段</Tag>
                if (!n.healthy) return <Tag color="red">冷却 {cooldown(n.cooldown_secs_remaining)}</Tag>
                return <Tag color="green">正常</Tag>
              },
            },
          ]}
        />
        {rt.nodes.some((n) => n.last_error) && (
          <Typography.Text type="secondary" style={{ fontSize: 12 }}>
            最后一条错误：{rt.nodes.find((n) => n.last_error)?.last_error}
          </Typography.Text>
        )}
      </Col>
      <Col xs={24} lg={12}>
        <Typography.Text strong>出图端点 / 账号</Typography.Text>
        <Table
          size="small"
          rowKey={(x) => `c${x.id}`}
          pagination={false}
          dataSource={rt.comfy_nodes}
          locale={{ emptyText: '未配置出图端点' }}
          columns={[
            { title: '端点', dataIndex: 'name' },
            {
              title: '占用',
              width: 170,
              render: (_, n) => (
                <Progress
                  percent={Math.round((n.active / Math.max(1, n.max_concurrency)) * 100)}
                  size="small"
                />
              ),
            },
            { title: '等待', dataIndex: 'waiting', width: 70 },
          ]}
        />
        <Table
          size="small"
          style={{ marginTop: 12 }}
          rowKey={(x) => `a${x.id}`}
          pagination={false}
          dataSource={rt.accounts}
          locale={{ emptyText: '未配置账号' }}
          columns={[
            { title: '账号', dataIndex: 'name' },
            { title: '进行中', dataIndex: 'active', width: 80 },
            { title: '排队', dataIndex: 'waiting', width: 70 },
            {
              title: '余额',
              width: 110,
              render: (_, a) => <span>{money(a.balance_micro)} 元</span>,
            },
            {
              title: '冻结',
              width: 100,
              render: (_, a) => <span>{money(a.held_micro)}</span>,
            },
          ]}
        />
        <Typography.Text type="secondary" style={{ fontSize: 12 }}>
          页面每 2 秒自动刷新
        </Typography.Text>
      </Col>
    </Row>
  )
}

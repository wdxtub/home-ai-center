import { useState } from 'react'
import { Card, Form, Input, Button, Typography, Alert } from 'antd'
import { LockOutlined, UserOutlined } from '@ant-design/icons'
import { api, setToken } from '../api'

export default function Login({ onDone }: { onDone: () => void }) {
  const [err, setErr] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)

  async function submit(v: { username: string; password: string }) {
    setBusy(true)
    setErr(null)
    try {
      const r = await api.post<{ token: string }>('/auth/login', v)
      setToken(r.token)
      onDone()
    } catch (e) {
      setErr(e instanceof Error ? e.message : '登录失败')
    } finally {
      setBusy(false)
    }
  }

  return (
    <div style={{ display: 'grid', placeItems: 'center', minHeight: '100vh' }}>
      <Card style={{ width: 360 }}>
        <Typography.Title level={4} style={{ textAlign: 'center', marginBottom: 24 }}>
          家庭 AI 网关 · 管理台
        </Typography.Title>
        {err && <Alert type="error" message={err} style={{ marginBottom: 16 }} showIcon />}
        <Form layout="vertical" onFinish={submit}>
          <Form.Item name="username" label="用户名" initialValue="admin">
            <Input prefix={<UserOutlined />} autoComplete="username" />
          </Form.Item>
          <Form.Item name="password" label="口令">
            <Input.Password prefix={<LockOutlined />} autoComplete="current-password" />
          </Form.Item>
          <Button type="primary" htmlType="submit" block loading={busy}>
            登录
          </Button>
        </Form>
      </Card>
    </div>
  )
}

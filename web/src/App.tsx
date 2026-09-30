import { useEffect, useState } from 'react'
import { Routes, Route, Navigate, useLocation, useNavigate } from 'react-router-dom'
import { Layout, Menu, Spin, Button } from 'antd'
import {
  DashboardOutlined,
  ApiOutlined,
  KeyOutlined,
  UserOutlined,
  PictureOutlined,
  NodeIndexOutlined,
  BarChartOutlined,
  SettingOutlined,
  LogoutOutlined,
} from '@ant-design/icons'
import { api, getToken, setToken } from './api'
import Login from './pages/Login'
import Overview from './pages/Overview'
import Nodes from './pages/Nodes'
import Keys from './pages/Keys'
import Accounts from './pages/Accounts'
import Images from './pages/Images'
import Usage from './pages/Usage'
import Settings from './pages/Settings'

const NAV = [
  { key: '/', icon: <DashboardOutlined />, label: '总览' },
  { key: '/nodes', icon: <ApiOutlined />, label: 'LLM 节点' },
  { key: '/keys', icon: <KeyOutlined />, label: 'API Key' },
  { key: '/accounts', icon: <UserOutlined />, label: '账号' },
  { key: '/images', icon: <PictureOutlined />, label: '出图' },
  { key: '/usage', icon: <BarChartOutlined />, label: '用量明细' },
  { key: '/settings', icon: <SettingOutlined />, label: '设置' },
]

export default function App() {
  const [authed, setAuthed] = useState<boolean | null>(null)
  const loc = useLocation()
  const nav = useNavigate()

  useEffect(() => {
    setAuthed(!!getToken())
  }, [])

  async function logout() {
    try {
      await api.post('/auth/logout')
    } finally {
      setToken(null)
      setAuthed(false)
    }
  }

  if (authed === null) {
    return (
      <div style={{ display: 'grid', placeItems: 'center', height: '100vh' }}>
        <Spin size="large" />
      </div>
    )
  }
  if (!authed) return <Login onDone={() => setAuthed(true)} />

  return (
    <Layout style={{ minHeight: '100vh' }}>
      <Layout.Sider width={200} theme="dark">
        <div
          style={{
            height: 56,
            display: 'flex',
            alignItems: 'center',
            justifyContent: 'center',
            color: '#fff',
            fontWeight: 600,
            letterSpacing: 0.5,
          }}
        >
          <NodeIndexOutlined style={{ marginRight: 8 }} />
          家庭 AI 网关
        </div>
        <Menu
          theme="dark"
          mode="inline"
          selectedKeys={[loc.pathname]}
          items={NAV}
          onClick={({ key }) => nav(key)}
        />
      </Layout.Sider>
      <Layout>
        <Layout.Header
          style={{
            display: 'flex',
            justifyContent: 'flex-end',
            alignItems: 'center',
            background: 'transparent',
            paddingInline: 0,
          }}
        >
          <Button type="text" icon={<LogoutOutlined />} onClick={logout}>
            退出
          </Button>
        </Layout.Header>
        <Layout.Content style={{ padding: 20, overflow: 'auto' }}>
          <Routes>
            <Route path="/" element={<Overview />} />
            <Route path="/nodes" element={<Nodes />} />
            <Route path="/keys" element={<Keys />} />
            <Route path="/accounts" element={<Accounts />} />
            <Route path="/images" element={<Images />} />
            <Route path="/usage" element={<Usage />} />
            <Route path="/settings" element={<Settings />} />
            <Route path="*" element={<Navigate to="/" replace />} />
          </Routes>
        </Layout.Content>
      </Layout>
    </Layout>
  )
}

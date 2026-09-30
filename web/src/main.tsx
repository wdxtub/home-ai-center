import { ConfigProvider, theme, App as AntApp } from 'antd'
import zhCN from 'antd/locale/zh_CN'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { BrowserRouter } from 'react-router-dom'
import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import App from './App'

const qc = new QueryClient({
  defaultOptions: {
    queries: {
      // 运行时面板要看得见变化，余额和用量也要跟着动，所以不做过期
      staleTime: 3000,
      refetchOnWindowFocus: true,
      retry: 1,
    },
  },
})

export function Root() {
  return (
    <ConfigProvider
      locale={zhCN}
      theme={{
        algorithm: theme.darkAlgorithm,
        token: {
          colorPrimary: '#4f7cff',
          borderRadius: 8,
          fontSize: 14,
        },
      }}
    >
      <AntApp>
        <QueryClientProvider client={qc}>
          <BrowserRouter>
            <App />
          </BrowserRouter>
        </QueryClientProvider>
      </AntApp>
    </ConfigProvider>
  )
}

// 入口必须自己把 React 挂上去。漏掉这一行的话
// `tsc` 与 `vite build` 都会通过、构建产物也正常生成，
// 但页面全白——只能靠真的打开页面才能发现。
const el = document.getElementById('root')
if (!el) throw new Error('index.html 缺少 #root 容器')
createRoot(el).render(
  <StrictMode>
    <Root />
  </StrictMode>,
)

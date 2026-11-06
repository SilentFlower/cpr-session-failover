// @env browser

/** 管理页只使用当前实例的宿主桥，不访问管理 Cookie 或任意 HTTP 地址 */
interface PluginBridge {
  readonly version: 2
  request: (input: { method: 'GET', path: string, query: string }) => Promise<{
    status: number
    contentType: string
    body: ArrayBuffer
  }>
}

declare global {
  /** 当前隔离页面可使用的宿主桥入口 */
  interface Window {
    readonly codexProxyPlugin?: PluginBridge
  }
}

/**
 * 读取已注册管理路由的原始 JSON 响应
 * @param path 当前插件的相对路由
 * @param query 不带问号的查询参数
 * @return 插件业务数据，宿主或插件错误交给调用方展示
 */
export async function request(path: string, query: URLSearchParams): Promise<unknown> {
  const host = window.codexProxyPlugin
  if (!host || host.version !== 2)
    throw new Error('请从 CPR 的插件扩展入口打开此页')

  const reply = await host.request({
    method: 'GET',
    path,
    query: query.toString(),
  })
  if (reply.contentType.split(';', 1)[0]?.trim().toLowerCase() !== 'application/json')
    throw new Error('切换记录返回了不支持的内容类型')

  let value: unknown
  try {
    value = JSON.parse(new TextDecoder().decode(reply.body))
  }
  catch (error) {
    throw new Error('切换记录返回了无效 JSON', { cause: error })
  }
  if (reply.status < 200 || reply.status >= 300)
    throw new Error(`切换记录读取失败（HTTP ${reply.status}）`)
  return value
}

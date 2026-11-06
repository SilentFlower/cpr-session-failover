// @env browser

/** 管理页只使用当前实例的宿主桥，不访问管理 Cookie 或任意 HTTP 地址 */
interface PluginBridge {
  readonly version: 2
  request: (input: {
    method: 'GET' | 'POST'
    path: string
    query: string
    contentType?: string
    body?: string
  }) => Promise<{ status: number, contentType: string, body: ArrayBuffer }>
}

declare global {
  /** 当前隔离页面可使用的宿主桥入口 */
  interface Window {
    readonly codexProxyPlugin?: PluginBridge
  }
}

/** 判断插件 JSON 对象，保留原始业务字段 */
export function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

/**
 * 调用当前插件已注册路由，不自动重试写入
 * @param path 当前插件相对路由
 * @param query 不带问号的查询参数
 * @param body 可选 JSON 正文，提供时使用 POST
 * @return 插件原始业务数据
 */
export async function request(path: string, query = new URLSearchParams(), body?: unknown): Promise<unknown> {
  const host = window.codexProxyPlugin
  if (!host || host.version !== 2)
    throw new Error('请从 CPR 的插件扩展入口打开此页')
  const reply = await host.request({
    method: body === undefined
      ? 'GET'
      : 'POST',
    path,
    query: query.toString(),
    ...(body === undefined
      ? {}
      : {
          contentType: 'application/json',
          body: JSON.stringify(body),
        }),
  })
  if (reply.contentType.split(';', 1)[0]?.trim().toLowerCase() !== 'application/json')
    throw new Error('插件返回了不支持的内容类型')
  let value: unknown
  try {
    value = JSON.parse(new TextDecoder().decode(reply.body))
  }
  catch (error) {
    throw new Error('插件返回了无效 JSON', { cause: error })
  }
  if (reply.status < 200 || reply.status >= 300) {
    const message = isObject(value) && isObject(value.error) && typeof value.error.message === 'string'
      ? value.error.message
      : `插件请求失败（HTTP ${reply.status}）`
    throw new Error(message)
  }
  return value
}

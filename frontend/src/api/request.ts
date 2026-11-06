// @env browser
import { readonly, shallowRef } from 'vue'

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

const writeState = shallowRef<'ready' | 'saving' | 'saved' | 'uncertain'>('ready')
const activeReads = new Set<Promise<unknown>>()

/** 配置发布后页面目标已失效，所有入口共用一次写入及重新连接状态 */
export const configurationWriteState = readonly(writeState)

/** 区分服务端明确拒绝与无法确认的写入结果 */
class ResponseError extends Error {
  /**
   * 保留插件路由的 HTTP 状态，未知结果不能当作安全重试
   * @param status 插件路由返回的状态码
   * @param message 安全的业务提示
   */
  constructor(readonly status: number, message: string) {
    super(message)
  }
}

/** 判断插件 JSON 对象，保留原始业务字段 */
export function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

/**
 * 读取当前目标的管理路由，配置发布期间不再向旧目标发送请求
 * @param path 当前插件相对路由
 * @param query 不带问号的查询参数
 * @return 插件原始业务数据
 */
export async function request(path: string, query = new URLSearchParams()): Promise<unknown> {
  if (writeState.value !== 'ready')
    throw new Error('当前页面需要重新连接，请点击右上角「刷新页面」')
  const operation = invoke(path, query)
  activeReads.add(operation)
  try {
    return await operation
  }
  finally {
    activeReads.delete(operation)
  }
}

/**
 * 等待读取结束后独占提交配置，不重试写入，也不用旧目标查询触发宿主刷新
 * @param path 当前插件配置写入路由
 * @param body 完整 JSON 正文
 * @param decode 所属 API 模块提供的成功响应校验
 * @return 已确认的写入结果，未知结果要求重新连接后核对
 */
export async function writeRequest<T>(path: string, body: unknown, decode: (value: unknown) => T): Promise<T> {
  if (writeState.value !== 'ready')
    throw new Error('当前页面需要重新连接，请点击右上角「刷新页面」')
  writeState.value = 'saving'
  try {
    // 先阻止新读取，再排空旧读取，避免保存同时仍有历史或范围目录访问旧版本
    await Promise.allSettled([...activeReads])
    const value = decode(await invoke(path, new URLSearchParams(), body))
    writeState.value = 'saved'
    return value
  }
  catch (cause) {
    writeState.value = cause instanceof ResponseError && [400, 409].includes(cause.status)
      ? 'ready'
      : 'uncertain'
    throw cause
  }
}

async function invoke(path: string, query: URLSearchParams, body?: unknown): Promise<unknown> {
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
    throw new ResponseError(reply.status, message)
  }
  return value
}

import { isObject, request, writeRequest } from '../request'

/** 与 CPR 原生实例共用的阶段规则，开关以规则是否存在表示 */
export interface Binding {
  contribution: string
  stage: string
  order: number
  failurePolicy: 'reject' | 'delegate'
  providerIds: string[]
  models: string[]
  clientKeyIds: string[]
  accountGroupIds: string[]
  identityBindings: { principal: string, clientKeyId: string }[]
  event?: string | null
}

/** 当前制品提供的三个阶段 */
export interface Definition {
  contribution: string
  stage: string
  label: string
  providerScope: boolean
}

/** 设置快照及宿主乐观锁版本 */
export interface Settings {
  revision: number
  configuration: { waitSeconds: number }
  bindings: Binding[]
  definitions: Definition[]
}

/** 范围目录只含资源标识、名称及可用状态，不含认证明文 */
export interface Resource {
  id: string
  name: string
  enabled: boolean
}

/** 范围目录的下一页由宿主游标决定 */
export interface Resources {
  items: Resource[]
  nextCursor: string | null
}

function isBinding(value: unknown): value is Binding {
  return isObject(value) && typeof value.contribution === 'string' && typeof value.stage === 'string'
    && Number.isInteger(value.order) && ['reject', 'delegate'].includes(String(value.failurePolicy))
    && [
      'providerIds',
      'models',
      'clientKeyIds',
      'accountGroupIds',
    ].every(key => Array.isArray(value[key]) && value[key].every(item => typeof item === 'string'))
    && Array.isArray(value.identityBindings) && value.identityBindings.length === 0
}

/**
 * 读取原生实例当前设置
 * @return 已核对合同的参数、规则及版本
 */
export async function getSettings(): Promise<Settings> {
  const value = await request('settings')
  if (!isObject(value) || !Number.isSafeInteger(value.revision) || !isObject(value.configuration)
    || !Number.isInteger(value.configuration.waitSeconds)
    || !Array.isArray(value.bindings) || !value.bindings.every(isBinding)
    || !Array.isArray(value.definitions) || value.definitions.length !== 3
    || !value.definitions.every(item => isObject(item) && typeof item.contribution === 'string'
      && typeof item.stage === 'string' && typeof item.label === 'string' && typeof item.providerScope === 'boolean')) {
    throw new Error('插件设置格式不符合当前合同')
  }
  return value as unknown as Settings
}

/**
 * 保存完整设置，由宿主校验、持久化并重新发布实例
 * @param source 最近读取的版本
 * @param configuration 当前参数草稿
 * @param bindings 当前阶段规则草稿
 * @return 保存确认，结果未知时通过宿主刷新核对而不自动重试
 */
export async function saveSettings(source: Settings, configuration: Settings['configuration'], bindings: Binding[]): Promise<void> {
  await writeRequest('settings', { expectedRevision: source.revision, configuration, bindings }, (value) => {
    if (!isObject(value) || value.saved !== true)
      throw new Error('无法确认保存结果，请点击右上角「刷新页面」后核对')
  })
}

/**
 * 分页读取范围选项
 * @param kind Key 或账号分组
 * @param cursor 宿主返回的后续游标，首轮为空
 * @return 当前页及后续游标
 */
export async function getResources(kind: 'keys' | 'groups', cursor = ''): Promise<Resources> {
  const value = await request('resources', new URLSearchParams({ kind, cursor }))
  if (!isObject(value) || !Array.isArray(value.items)
    || !value.items.every(item => isObject(item) && typeof item.id === 'string' && typeof item.name === 'string' && typeof item.enabled === 'boolean')
    || (value.nextCursor !== null && typeof value.nextCursor !== 'string')) {
    throw new Error('请求范围选项格式无效')
  }
  return value as unknown as Resources
}

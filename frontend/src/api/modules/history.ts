import { request } from '../request'

/** 对应插件持久历史中的一条记录，空值保留未知含义 */
export interface HistoryRecord {
  id: string
  requestId: string
  attemptIndex: number
  sessionHash: string
  clientKeyId: string
  model: string | null
  decisionAtMs: number
  decisionTimeUtc: string | null
  admittedTimeUtc: string | null
  completedTimeUtc: string | null
  schedulerWaitMs: number
  stage: string
  reason: string
  proposedAccountId: string | null
  actualAccountId: string | null
  fromAccountId: string | null
  admittedAtMs: number | null
  completedAtMs: number | null
  outcome: string | null
  errorCode: string | null
  requestErrorCode: string | null
  accountSelectionWaitMs: number | null
  accountQueueWaitMs: number | null
  bindingChanged: boolean | null
  evidenceStatus: string
  terminalStatus: string
}

/** 历史查询复用 CLI 的数据，不维护页面自己的切换账本 */
export interface HistoryResponse {
  records: HistoryRecord[]
  configuration: { waitSeconds: number, historyLimit: number }
}

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function isRecord(value: unknown): value is HistoryRecord {
  if (!isObject(value))
    return false
  const text = [
    'id',
    'requestId',
    'sessionHash',
    'clientKeyId',
    'stage',
    'reason',
    'evidenceStatus',
    'terminalStatus',
  ]
  const numbers = ['attemptIndex', 'decisionAtMs', 'schedulerWaitMs']
  const nullableText = [
    'decisionTimeUtc',
    'admittedTimeUtc',
    'completedTimeUtc',
    'model',
    'proposedAccountId',
    'actualAccountId',
    'fromAccountId',
    'outcome',
    'errorCode',
    'requestErrorCode',
  ]
  const nullableNumbers = [
    'admittedAtMs',
    'completedAtMs',
    'accountSelectionWaitMs',
    'accountQueueWaitMs',
  ]
  return text.every(key => typeof value[key] === 'string')
    && numbers.every(key => typeof value[key] === 'number' && Number.isFinite(value[key]))
    && nullableText.every(key => value[key] === null || typeof value[key] === 'string')
    && nullableNumbers.every(key => value[key] === null || (typeof value[key] === 'number' && Number.isFinite(value[key])))
    && (value.bindingChanged === null || typeof value.bindingChanged === 'boolean')
}

/**
 * 查询当前保留的切换记录，最多读取宿主允许的 500 条
 * @param requestId 精确匹配的请求 ID，空字符串表示不限
 * @return 从新到旧的记录与当前实例配置
 */
export async function getHistory(requestId: string): Promise<HistoryResponse> {
  const query = new URLSearchParams({ limit: '500' })
  if (requestId)
    query.set('requestId', requestId)
  const value = await request('history', query)
  if (!isObject(value) || !Array.isArray(value.records) || value.records.length > 500
    || !value.records.every(isRecord) || !isObject(value.configuration)
    || typeof value.configuration.waitSeconds !== 'number' || typeof value.configuration.historyLimit !== 'number') {
    throw new Error('切换记录格式不符合当前插件合同')
  }
  return value as unknown as HistoryResponse
}

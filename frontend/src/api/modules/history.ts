import { isObject, request } from '../request'

/** 对应插件持久历史中的一条记录，空值保留未知含义 */
export interface HistoryRecord {
  routingScope: string | null
  routingGroupRefs: string[]
  routingGroupNamesSnapshot: string[]
  id: string
  requestId: string
  attemptIndex: number
  sessionHash: string
  clientKeyId: string
  model: string | null
  decisionAtMs: number
  decisionTimeDisplay: string | null
  admittedTimeDisplay: string | null
  completedTimeDisplay: string | null
  schedulerWaitMs: number
  stage: string
  reason: string
  proposedAccountId: string | null
  actualAccountId: string | null
  fromAccountId: string | null
  originalAccountId?: string
  fromAccountName: string | null
  fromAccountEmail: string | null
  originalAccountName: string | null
  originalAccountEmail: string | null
  proposedAccountName: string | null
  proposedAccountEmail: string | null
  actualAccountName: string | null
  actualAccountEmail: string | null
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
  configuration: { waitSeconds: number }
  pagination: { page: number, pageSize: number, total: number }
  asOf: number
  storage: { incompleteWrites: number, automaticDeletion: false, lastWriteError: string | null }
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
    'routingScope',
    'decisionTimeDisplay',
    'admittedTimeDisplay',
    'completedTimeDisplay',
    'model',
    'proposedAccountId',
    'actualAccountId',
    'fromAccountId',
    'fromAccountName',
    'fromAccountEmail',
    'originalAccountName',
    'originalAccountEmail',
    'proposedAccountName',
    'proposedAccountEmail',
    'actualAccountName',
    'actualAccountEmail',
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
  return [value.routingGroupRefs, value.routingGroupNamesSnapshot].every(groups => Array.isArray(groups) && groups.every(group => typeof group === 'string'))
    && text.every(key => typeof value[key] === 'string')
    && numbers.every(key => typeof value[key] === 'number' && Number.isFinite(value[key]))
    && nullableText.every(key => value[key] === null || typeof value[key] === 'string')
    && nullableNumbers.every(key => value[key] === null || (typeof value[key] === 'number' && Number.isFinite(value[key])))
    && (value.originalAccountId === undefined || typeof value.originalAccountId === 'string')
    && (value.bindingChanged === null || typeof value.bindingChanged === 'boolean')
}

/** 历史读取选项，时间锚点用于翻页时保持记录集合 */
export interface HistoryInput {
  requestId: string
  result: string
  page: number
  limit: number
  asOf?: number
}

/**
 * 分页查询全部保留历史，不改变保留策略
 * @param input 请求筛选、当前页及可选时间锚点
 * @return 从新到旧的一页记录与匹配总数
 */
export async function getHistory(input: HistoryInput): Promise<HistoryResponse> {
  const query = new URLSearchParams({
    page: String(input.page),
    limit: String(input.limit),
    result: input.result,
  })
  if (input.requestId)
    query.set('requestId', input.requestId)
  if (input.asOf !== undefined)
    query.set('asOf', String(input.asOf))
  const value = await request('history', query)
  if (!isObject(value) || !Array.isArray(value.records)
    || !value.records.every(isRecord) || !isObject(value.configuration)
    || typeof value.configuration.waitSeconds !== 'number' || !isObject(value.pagination)
    || !['page', 'pageSize', 'total'].every(key => Number.isSafeInteger(value.pagination && (value.pagination as Record<string, unknown>)[key]))
    || !Number.isSafeInteger(value.asOf) || !isObject(value.storage)
    || !Number.isSafeInteger(value.storage.incompleteWrites) || value.storage.automaticDeletion !== false
    || (value.storage.lastWriteError !== null && typeof value.storage.lastWriteError !== 'string')) {
    throw new Error('切换记录格式不符合当前插件合同')
  }
  return value as unknown as HistoryResponse
}

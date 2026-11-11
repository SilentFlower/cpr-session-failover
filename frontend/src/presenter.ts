import type { HistoryRecord } from './api/modules/history'

const stages: Record<string, string> = {
  proposed: '已提出切换',
  admitted: '已取得账号',
  admitted_by_trace: '诊断确认取得账号',
  candidate_unavailable: '候选账号不可用',
  different_account: '取得了其他账号',
  blocked: '未主动迁移',
}

const reasons: Record<string, string> = {
  wait_threshold: '选号等待达到阈值',
  native_queue_replay: '原生续写排队超时后重发',
  native_continuation: '原生续写绑定原账号',
  item_reference: '输入引用原账号上下文',
  prewarm: '预热请求',
  account_queue_disabled: '账号排队已关闭',
  queue_budget_too_short: '总排队时限不大于迁移阈值',
}

/**
 * 展示宿主已经确认的事实，缺少改绑证据时不显示为成功
 * @param record 切换记录
 * @return 用于状态标签的中文说明与通用语义颜色
 */
export function bindingResult(record: HistoryRecord): { text: string, type: 'success' | 'info' | 'warning' | 'default' } {
  if (record.bindingChanged === true)
    return { text: '已确认改绑', type: 'success' }
  if (record.bindingChanged === false) {
    // 此字段验证当前迁移意图，目标被替代时不能推断会话从未发生其他改绑
    if (record.stage === 'blocked')
      return { text: '未主动迁移', type: 'default' }
    return { text: '计划未命中', type: 'default' }
  }
  if (record.evidenceStatus === 'unavailable')
    return { text: '改绑证据缺失', type: 'warning' }
  return { text: '待核实改绑', type: 'info' }
}

/**
 * 展示调度阶段，不从阶段推断绑定是否变化
 * @param stage 持久记录的阶段码
 * @return 中文名称，未知值保留原码
 */
export function stageText(stage: string): string {
  return stages[stage] ?? stage
}

/**
 * 展示迁移或保护原因
 * @param reason 记录的原因码
 * @return 中文说明，未知值保留原码
 */
export function reasonText(reason: string): string {
  return reasons[reason] ?? reason
}

/**
 * 格式化持续时长，不混淆未知值与零等待
 * @param milliseconds 已知毫秒数或未知空值
 * @return 秒数或未知标记
 */
export function duration(milliseconds: number | null): string {
  return milliseconds === null
    ? '未知'
    : `${(milliseconds / 1000).toLocaleString('zh-CN', { maximumFractionDigits: 2 })} 秒`
}

/**
 * 展示请求终态，尚未查询成功时明确保留缺口
 * @param record 切换记录
 * @return 中文终态或原始结果码
 */
export function outcomeText(record: HistoryRecord): string {
  if (record.terminalStatus !== 'known') {
    return record.terminalStatus === 'unavailable'
      ? '终态不可用'
      : '等待终态'
  }
  const labels: Record<string, string> = {
    succeeded: '成功',
    failed: '失败',
    cancelled: '已取消',
    incomplete: '未完成',
    running: '进行中',
  }
  return record.outcome === null
    ? '未知'
    : labels[record.outcome] ?? record.outcome
}

/**
 * 仅展示本地化相对时间，绝对时间由服务端提供
 * @param timestamp 记录的 UTC 毫秒
 * @param now 当前刷新时刻的 UTC 毫秒
 * @return 相对时间
 */
export function relativeTime(timestamp: number, now: number): string {
  const seconds = Math.max(0, Math.floor((now - timestamp) / 1000))
  const formatter = new Intl.RelativeTimeFormat('zh-CN', { numeric: 'auto' })
  if (seconds < 60)
    return formatter.format(-seconds, 'second')
  if (seconds < 3600)
    return formatter.format(-Math.floor(seconds / 60), 'minute')
  if (seconds < 86400)
    return formatter.format(-Math.floor(seconds / 3600), 'hour')
  return formatter.format(-Math.floor(seconds / 86400), 'day')
}

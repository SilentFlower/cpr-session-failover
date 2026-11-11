import type { HistoryRecord } from './api/modules/history'

/**
 * 展示触发请求发生时的分组快照
 * @param record 宿主核实的请求范围与分组
 * @return 分组名称、稳定 ID 或明确的未知范围
 */
export function groupText(record: HistoryRecord): string {
  if (record.routingScope === 'all')
    return '全部账号'
  const groups = record.routingGroupNamesSnapshot.length
    ? record.routingGroupNamesSnapshot
    : record.routingGroupRefs
  return groups.length
    ? groups.join('、')
    : record.routingScope === 'groups'
      ? '未关联分组'
      : '未记录'
}

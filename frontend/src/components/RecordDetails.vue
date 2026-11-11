<script setup lang="ts">
import type { HistoryRecord } from '../api/modules/history'
import { computed } from 'vue'
import { duration, outcomeText, reasonText, stageText } from '../presenter'

const props = defineProps<{ record: HistoryRecord }>()

/**
 * 在详情中保留账号名称与完整 ID
 * @param name 宿主当前保存的名称
 * @param id 历史中记录的账号 ID
 * @param empty 缺少账号事实时的说明
 * @return 可直接显示的账号信息
 */
function account(name: string | null, id: string | null | undefined, empty = '未知') {
  return name && id
    ? `${name}（${id}）`
    : id ?? empty
}

const fields = computed(() => [
  { label: '请求 ID', value: props.record.requestId },
  {
    label: '尝试序号',
    value: String(props.record.attemptIndex),
  },
  {
    label: '前绑定账号',
    value: account(props.record.fromAccountName, props.record.fromAccountId, '未知'),
  },
  ...(props.record.originalAccountId
    ? [
        {
          label: '续写原账号',
          value: account(props.record.originalAccountName, props.record.originalAccountId),
        },
      ]
    : []),
  {
    label: '计划账号',
    value: account(props.record.proposedAccountName, props.record.proposedAccountId, '无'),
  },
  {
    label: '实际取得账号',
    value: account(props.record.actualAccountName, props.record.actualAccountId, '尚未确认'),
  },
  {
    label: '调度阶段',
    value: stageText(props.record.stage),
  },
  {
    label: '原因',
    value: reasonText(props.record.reason),
  },
  { label: '模型', value: props.record.model ?? '未知' },
  { label: 'Key ID', value: props.record.clientKeyId },
  { label: '会话摘要', value: props.record.sessionHash },
  { label: '触发时间（北京时间）', value: props.record.decisionTimeDisplay ?? '未知' },
  { label: '取得账号时间（北京时间）', value: props.record.admittedTimeDisplay ?? '尚未确认' },
  { label: '请求结束时间（北京时间）', value: props.record.completedTimeDisplay ?? '尚未确认' },
  {
    label: '插件选号等待',
    value: duration(props.record.schedulerWaitMs),
  },
  {
    label: '本体选号总耗时',
    value: duration(props.record.accountSelectionWaitMs),
  },
  {
    label: '本体账号队列等待',
    value: duration(props.record.accountQueueWaitMs),
  },
  {
    label: '请求结果',
    value: outcomeText(props.record),
  },
  { label: '请求错误码', value: props.record.requestErrorCode ?? '无已知错误' },
  { label: '尝试错误码', value: props.record.errorCode ?? '无已知错误' },
  { label: '改绑证据', value: props.record.evidenceStatus },
  { label: '终态查询', value: props.record.terminalStatus },
])
</script>

<template>
  <section :id="`record-${record.id}`" aria-label="切换详情" class="space-y-4 rounded-z bg-z-fill-alter p-4">
    <p v-if="record.bindingChanged === null" class="text-z-sm text-z-text-secondary">
      {{ record.evidenceStatus === 'unavailable' ? '缺少改绑证据，不能据此判断是否切换成功' : '改绑证据由后台补齐，取得账号不等于已核实改绑' }}
    </p>
    <p v-else-if="record.bindingChanged === false && record.stage !== 'blocked'" class="text-z-sm text-z-text-secondary">
      此记录未确认本次计划的改绑，实际取得其他账号时不能据此断言没有发生其他改绑
    </p>
    <dl class="grid grid-cols-1 gap-x-6 gap-y-4 sm:grid-cols-2 xl:grid-cols-3">
      <div v-for="field in fields" :key="field.label" class="min-w-0 space-y-1">
        <dt class="text-z-xs text-z-text-secondary">
          {{ field.label }}
        </dt>
        <dd class="m-0 text-z-sm break-all text-z-text">
          {{ field.value }}
        </dd>
      </div>
    </dl>
  </section>
</template>

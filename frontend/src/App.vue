<script setup lang="ts">
import type { TableColumn } from '@codex-proxy/ui'
import type { HistoryRecord, HistoryResponse } from './api/modules/history'
import { ZButton, ZCard, ZConfigProvider, ZEmpty, ZInput, ZPagination, ZSelect, ZTable, ZTag } from '@codex-proxy/ui'
import { zhHans } from '@codex-proxy/ui/locale'
import { computed, onBeforeUnmount, onMounted, ref, shallowRef, watch } from 'vue'
import { getHistory } from './api/modules/history'
import { configurationWriteState } from './api/request'
import RecordDetails from './components/RecordDetails.vue'
import SettingsDialog from './components/SettingsDialog.vue'
import { groupText } from './groups'
import { bindingResult, duration, outcomeText, relativeTime, stageText } from './presenter'

const settingsOpen = ref(false)
const requestsBlocked = computed(() => configurationWriteState.value !== 'ready')
const reconnectRequired = computed(() => ['saved', 'uncertain'].includes(configurationWriteState.value))
const snapshot = shallowRef<HistoryResponse>()
const requestId = ref('')
const appliedRequestId = ref('')
const resultFilter = ref('all')
const loading = ref(false)
const error = ref('')
const page = ref(1)
const pageSize = ref(20)
const expanded = ref<string[]>([])
const now = ref(Date.now())
let mounted = true

const resultOptions = [
  { label: '全部改绑结果', value: 'all' },
  { label: '已确认改绑', value: 'changed' },
  { label: '待核实或证据缺失', value: 'unknown' },
  { label: '未确认本次改绑', value: 'unchanged' },
]

const records = computed(() => snapshot.value?.records ?? [])
const changedCount = computed(() => records.value.filter(record => record.bindingChanged === true).length)
const total = computed(() => snapshot.value?.pagination.total ?? 0)
const metrics = computed(() => [
  {
    label: '匹配记录',
    value: snapshot.value ? total.value : '—',
    color: 'text-z-primary',
  },
  {
    label: '本页已改绑',
    value: snapshot.value ? changedCount.value : '—',
    color: 'text-z-success',
  },
  {
    label: '本页待核实',
    value: snapshot.value
      ? records.value.filter(row => row.bindingChanged === null).length
      : '—',
    color: 'text-z-warning',
  },
])

const columns: TableColumn<HistoryRecord>[] = [
  {
    key: 'decisionTimeDisplay',
    label: '触发时间',
    kind: 'datetime',
    minWidth: 176,
  },
  {
    key: 'requestId',
    label: '请求 / 模型',
    kind: 'identity',
    minWidth: 190,
  },
  { key: 'routingGroups', label: '触发分组', minWidth: 150 },
  {
    key: 'stage',
    label: '调度阶段',
    kind: 'status',
    minWidth: 130,
  },
  {
    key: 'bindingChanged',
    label: '改绑结果',
    kind: 'status',
    minWidth: 130,
  },
  {
    key: 'accounts',
    label: '账号路径',
    kind: 'identity',
    minWidth: 190,
  },
  {
    key: 'schedulerWaitMs',
    label: '选号等待',
    kind: 'numeric',
    minWidth: 100,
  },
  {
    key: 'outcome',
    label: '请求结果',
    kind: 'status',
    minWidth: 110,
  },
  {
    key: 'actions',
    label: '详情',
    kind: 'actions',
    width: 80,
  },
]

watch(resultFilter, () => void load(appliedRequestId.value, true))
watch(pageSize, () => void load(appliedRequestId.value, true))
watch(page, () => void load(appliedRequestId.value))

async function load(id: string, resetPage = false) {
  if (loading.value || requestsBlocked.value)
    return
  loading.value = true
  error.value = ''
  try {
    const response = await getHistory({
      requestId: id,
      result: resultFilter.value,
      page: resetPage ? 1 : page.value,
      limit: pageSize.value,
      asOf: resetPage ? undefined : snapshot.value?.asOf,
    })
    if (!mounted)
      return
    snapshot.value = response
    appliedRequestId.value = id
    now.value = Date.now()
    expanded.value = []
    page.value = response.pagination.page
  }
  catch (reason) {
    if (mounted) {
      error.value = reason instanceof Error
        ? reason.message
        : '切换记录读取失败'
      page.value = snapshot.value?.pagination.page ?? 1
    }
  }
  finally {
    if (mounted)
      loading.value = false
  }
}

function search() {
  void load(requestId.value.trim(), true)
}

function reset() {
  requestId.value = ''
  resultFilter.value = 'all'
  void load('', true)
}

function toggleDetails(id: string) {
  expanded.value = expanded.value.includes(id)
    ? []
    : [id]
}

onMounted(() => void load(''))
onBeforeUnmount(() => {
  mounted = false
})
</script>

<template>
  <ZConfigProvider :locale="zhHans">
    <!-- 弹窗打开时只裁剪超出可视区的背景，记录和筛选状态继续保留 -->
    <main class="space-y-4 text-z-text" :class="{ 'overflow-clip': settingsOpen }" :style="settingsOpen ? { maxHeight: 'var(--plugin-viewport-height, 100dvh)' } : undefined">
      <SettingsDialog v-model="settingsOpen" />
      <p v-if="reconnectRequired && !settingsOpen" role="status" class="rounded-z bg-z-info-container p-3 text-z-sm text-z-info-on-container">
        {{ configurationWriteState === 'saved' ? '设置已保存并生效' : '保存结果待确认' }}，点击右上角「刷新页面」继续操作
      </p>
      <section aria-label="切换概览" class="grid grid-cols-3 gap-2 sm:gap-4">
        <ZCard v-for="metric in metrics" :key="metric.label">
          <p class="text-z-xs text-z-text-secondary sm:text-z-sm">
            {{ metric.label }}
          </p>
          <p class="mt-1 text-2xl font-emphasis tabular-nums" :class="metric.color">
            {{ metric.value }}
          </p>
        </ZCard>
      </section>
      <ZCard class="min-w-0">
        <div class="mb-3 flex flex-wrap items-baseline justify-between gap-2">
          <h2 class="text-z-base font-emphasis">
            切换记录
          </h2>
          <span class="text-z-xs text-z-text-secondary">北京时间 · UTC+8</span>
        </div>
        <form class="mb-4 flex flex-wrap items-center gap-2" @submit.prevent="search">
          <ZInput
            v-model="requestId"
            aria-label="请求 ID"
            placeholder="按请求 ID 精确查询"
            class="w-full sm:max-w-80"
            :disabled="loading || requestsBlocked"
            maxlength="256"
          />
          <ZSelect v-model="resultFilter" aria-label="改绑结果" :disabled="loading || requestsBlocked" :options="resultOptions" class="w-full sm:w-48" />
          <ZButton type="primary" native-type="submit" :disabled="loading || requestsBlocked">
            查询
          </ZButton>
          <ZButton :disabled="loading || requestsBlocked || (!requestId && resultFilter === 'all' && !appliedRequestId)" @click="reset">
            重置
          </ZButton>
          <ZButton :disabled="requestsBlocked" class="sm:ml-auto" @click="settingsOpen = true">
            插件设置
          </ZButton>
          <ZButton :loading="loading" :disabled="requestsBlocked" @click="load(appliedRequestId, true)">
            刷新
          </ZButton>
        </form>

        <p v-if="snapshot?.storage.incompleteWrites" role="status" class="mb-3 text-z-sm text-z-warning">
          {{ snapshot.storage.incompleteWrites }} 次未完成写入未计入列表
        </p>
        <p v-if="snapshot?.storage.lastWriteError" role="alert" class="mb-4 rounded-z bg-z-warning-container p-3 text-z-warning-on-container">
          {{ snapshot.storage.lastWriteError }}
        </p>
        <div v-if="error" role="alert" class="mb-4 flex flex-wrap items-center gap-3 rounded-z bg-z-error-container p-3 text-z-error-on-container">
          <span>{{ error }}</span>
          <ZButton size="small" :disabled="loading || requestsBlocked" @click="load(appliedRequestId, true)">
            重新读取
          </ZButton>
        </div>

        <ZTable
          :columns="columns"
          :data="records"
          row-key="id"
          :loading="loading"
          :expanded-row-keys="expanded"
          size="small"
        >
          <template #decisionTimeDisplay="{ row }">
            <div class="min-w-0 space-y-1">
              <div class="whitespace-nowrap text-z-sm tabular-nums">
                {{ row.decisionTimeDisplay ?? '未知' }}
              </div>
              <div class="text-z-xs text-z-text-secondary">
                {{ relativeTime(row.decisionAtMs, now) }}
              </div>
            </div>
          </template>
          <template #requestId="{ row }">
            <div class="min-w-0 space-y-1">
              <div class="truncate font-mono text-z-xs" :title="row.requestId">
                {{ row.requestId }}
              </div>
              <div class="truncate text-z-xs text-z-text-secondary">
                {{ row.model ?? '未知模型' }}
              </div>
            </div>
          </template>
          <template #routingGroups="{ row }">
            <span class="block truncate text-z-xs" :title="groupText(row)">{{ groupText(row) }}</span>
          </template>
          <template #stage="{ row }">
            {{ stageText(row.stage) }}
          </template>
          <template #bindingChanged="{ row }">
            <ZTag :type="bindingResult(row).type" size="small">
              {{ bindingResult(row).text }}
            </ZTag>
          </template>
          <template #accounts="{ row }">
            <dl class="grid grid-cols-[auto_minmax(0,1fr)] gap-x-2 gap-y-1 text-z-xs">
              <dt class="text-z-text-secondary">
                {{ row.fromAccountId || !row.originalAccountId ? '前绑定' : '续写原账号' }}
              </dt>
              <dd class="m-0 min-w-0 space-y-1" :title="row.fromAccountId ?? row.originalAccountId">
                <span class="block truncate font-emphasis">{{ row.fromAccountId ? (row.fromAccountName ?? row.fromAccountId) : (row.originalAccountName ?? row.originalAccountId ?? '缺少证据') }}</span>
                <span v-if="row.fromAccountId ? (row.fromAccountEmail && row.fromAccountEmail !== row.fromAccountName) : (row.originalAccountEmail && row.originalAccountEmail !== row.originalAccountName)" class="block truncate text-z-text-secondary" :title="(row.fromAccountId ? row.fromAccountEmail : row.originalAccountEmail) ?? undefined">
                  {{ row.fromAccountId ? row.fromAccountEmail : row.originalAccountEmail }}
                </span>
              </dd>
              <dt class="text-z-text-secondary">
                实际取得
              </dt>
              <dd class="m-0 min-w-0 space-y-1" :title="row.actualAccountId ?? undefined">
                <span class="block truncate font-emphasis">{{ row.actualAccountName ?? row.actualAccountId ?? '尚未确认' }}</span>
                <span v-if="row.actualAccountEmail && row.actualAccountEmail !== row.actualAccountName" class="block truncate text-z-text-secondary" :title="row.actualAccountEmail">
                  {{ row.actualAccountEmail }}
                </span>
              </dd>
            </dl>
          </template>
          <template #schedulerWaitMs="{ row }">
            {{ duration(row.schedulerWaitMs) }}
          </template>
          <template #outcome="{ row }">
            <span class="block truncate" :title="outcomeText(row)">{{ outcomeText(row) }}</span>
          </template>
          <template #actions="{ row }">
            <ZButton variant="link" size="small" class="whitespace-nowrap" :aria-expanded="expanded.includes(row.id)" :aria-controls="`record-${row.id}`" @click="toggleDetails(row.id)">
              {{ expanded.includes(row.id) ? '收起' : '展开' }}
            </ZButton>
          </template>
          <template #expanded="{ row }">
            <RecordDetails :record="row" />
          </template>
          <template #empty>
            <ZEmpty
              :title="error ? '暂时无法读取记录' : '暂无匹配的切换记录'"
              :description="error ? '可重新读取，已有历史不会因此清除' : '仅在提出切换或遇到迁移保护时记录，正常直接取得账号的请求不会出现在这里'"
              surface="none"
            />
          </template>
        </ZTable>

        <ZPagination
          v-if="total"
          v-model:current-page="page"
          v-model:page-size="pageSize"
          :total="total"
          :page-sizes="[10, 20, 50]"
          :disabled="loading || requestsBlocked"
          class="mt-4"
        />
      </ZCard>
    </main>
  </ZConfigProvider>
</template>

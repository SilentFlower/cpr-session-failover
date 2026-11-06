<script setup lang="ts">
import type { TableColumn } from '@codex-proxy/ui'
import type { HistoryRecord, HistoryResponse } from './api/modules/history'
import { ZButton, ZCard, ZConfigProvider, ZEmpty, ZInput, ZPagination, ZSelect, ZTable, ZTag } from '@codex-proxy/ui'
import { zhHans } from '@codex-proxy/ui/locale'
import { computed, onBeforeUnmount, onMounted, ref, shallowRef, watch } from 'vue'
import { getHistory } from './api/modules/history'
import RecordDetails from './components/RecordDetails.vue'
import { bindingResult, duration, outcomeText, relativeTime, stageText } from './presenter'

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
const filtered = computed(() => records.value.filter((record) => {
  switch (resultFilter.value) {
    case 'changed': return record.bindingChanged === true
    case 'unknown': return record.bindingChanged === null
    case 'unchanged': return record.bindingChanged === false
    default: return true
  }
}))
const changedCount = computed(() => records.value.filter(record => record.bindingChanged === true).length)
const rows = computed(() => filtered.value.slice((page.value - 1) * pageSize.value, page.value * pageSize.value))

const columns: TableColumn<HistoryRecord>[] = [
  {
    key: 'decisionTimeUtc',
    label: '触发时间（UTC）',
    kind: 'datetime',
    minWidth: 210,
  },
  {
    key: 'requestId',
    label: '请求 / 模型',
    kind: 'identity',
    minWidth: 190,
  },
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
    label: '前账号 → 实际账号',
    kind: 'identity',
    minWidth: 180,
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

watch([resultFilter, pageSize], () => {
  page.value = 1
  expanded.value = []
})
watch(() => filtered.value.length, (length) => {
  page.value = Math.min(page.value, Math.max(1, Math.ceil(length / pageSize.value)))
})

async function load(id: string, resetPage = false) {
  if (loading.value)
    return
  loading.value = true
  error.value = ''
  try {
    const response = await getHistory(id)
    if (!mounted)
      return
    snapshot.value = response
    appliedRequestId.value = id
    now.value = Date.now()
    expanded.value = []
    if (resetPage)
      page.value = 1
  }
  catch (reason) {
    if (mounted) {
      error.value = reason instanceof Error
        ? reason.message
        : '切换记录读取失败'
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
    <main class="space-y-4 text-z-text">
      <ZCard>
        <form class="flex flex-wrap items-center gap-3" @submit.prevent="search">
          <ZInput
            v-model="requestId"
            aria-label="请求 ID"
            placeholder="按请求 ID 精确查询"
            class="w-full sm:max-w-80"
            :disabled="loading"
            maxlength="256"
          />
          <ZSelect v-model="resultFilter" aria-label="改绑结果" :options="resultOptions" class="w-full sm:w-48" />
          <ZButton type="primary" native-type="submit" :disabled="loading">
            查询
          </ZButton>
          <ZButton :disabled="loading || (!requestId && resultFilter === 'all' && !appliedRequestId)" @click="reset">
            重置
          </ZButton>
          <ZButton :loading="loading" class="sm:ml-auto" @click="load(appliedRequestId)">
            刷新
          </ZButton>
        </form>

        <div v-if="snapshot" class="mt-4 flex flex-wrap gap-x-5 gap-y-2 text-z-sm text-z-text-secondary" aria-live="polite">
          <span>选号等待阈值 <strong class="font-emphasis text-z-text">{{ snapshot.configuration.waitSeconds }} 秒</strong></span>
          <span>当前查询 {{ records.length }} 条</span>
          <span>已确认改绑 {{ changedCount }} 条</span>
          <span>最多保留 {{ snapshot.configuration.historyLimit }} 条，受存储容量限制</span>
        </div>
      </ZCard>

      <ZCard>
        <div v-if="error" role="alert" class="mb-4 flex flex-wrap items-center gap-3 rounded-z bg-z-error-container p-3 text-z-error-on-container">
          <span>{{ error }}</span>
          <ZButton size="small" :disabled="loading" @click="load(appliedRequestId)">
            重新读取
          </ZButton>
        </div>

        <ZTable
          :columns="columns"
          :data="rows"
          row-key="id"
          :loading="loading"
          :expanded-row-keys="expanded"
          size="small"
        >
          <template #decisionTimeUtc="{ row }">
            <div class="min-w-0 space-y-1">
              <div class="truncate text-z-xs" :title="row.decisionTimeUtc ?? undefined">
                {{ row.decisionTimeUtc ?? '未知' }}
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
          <template #stage="{ row }">
            {{ stageText(row.stage) }}
          </template>
          <template #bindingChanged="{ row }">
            <ZTag :type="bindingResult(row).type" size="small">
              {{ bindingResult(row).text }}
            </ZTag>
          </template>
          <template #accounts="{ row }">
            <div class="space-y-1">
              <div class="truncate text-z-xs" :title="`${row.fromAccountId ?? '未知'} → ${row.actualAccountId ?? '尚未确认'}`">
                {{ row.fromAccountId ?? '未知' }} → {{ row.actualAccountId ?? '尚未确认' }}
              </div>
              <div v-if="row.proposedAccountId" class="truncate text-z-xs text-z-text-secondary" :title="row.proposedAccountId">
                计划 {{ row.proposedAccountId }}
              </div>
            </div>
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
          v-if="filtered.length"
          v-model:current-page="page"
          v-model:page-size="pageSize"
          :total="filtered.length"
          :page-sizes="[10, 20, 50]"
          :disabled="loading"
          class="mt-4"
        />
      </ZCard>
    </main>
  </ZConfigProvider>
</template>

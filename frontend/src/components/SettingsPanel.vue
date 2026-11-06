<script setup lang="ts">
import type { Binding, Definition, Resource, Settings } from '../api/modules/settings'
import { ZButton, ZCard, ZCheckbox, ZFormItem, ZInputNumber, ZSelect, ZSwitch, ZTextarea } from '@codex-proxy/ui'
import { computed, onBeforeUnmount, onMounted, reactive, ref, shallowRef } from 'vue'
import { getResources, getSettings, saveSettings } from '../api/modules/settings'

const source = shallowRef<Settings>()
const draft = ref<{ configuration: Settings['configuration'], bindings: Binding[] }>()
const loading = ref(false)
const saving = ref(false)
const error = ref('')
const notice = ref('')
const restricted = ref(new Set<string>())
const catalogs = reactive<Record<'keys' | 'groups', { items: Resource[], next: string | null, loaded: boolean, loading: boolean, error: string }>>({
  keys: {
    items: [],
    next: null,
    loaded: false,
    loading: false,
    error: '',
  },
  groups: {
    items: [],
    next: null,
    loaded: false,
    loading: false,
    error: '',
  },
})
const failures = [
  { value: 'reject', label: '拒绝请求' },
  { value: 'delegate', label: '继续下一个处理器' },
]
const disabled = computed(() => loading.value || saving.value)
const dirty = computed(() => source.value && draft.value && JSON.stringify(draft.value) !== JSON.stringify({ configuration: source.value.configuration, bindings: source.value.bindings }))
const valid = computed(() => {
  if (!draft.value || !source.value)
    return false
  const wait = draft.value.configuration.waitSeconds
  return Number.isInteger(wait) && wait >= 1 && wait <= 110
    && source.value.definitions.every((definition) => {
      const current = binding(definition)
      return !current || (Number.isInteger(current.order) && current.order >= -2147483648 && current.order <= 2147483647
        && (!restricted.value.has(key(definition)) || hasScope(current)))
    })
})
const partial = computed(() => draft.value && source.value && source.value.definitions.some(item => !binding(item)))
let mounted = true

function key(definition: Definition) {
  return `${definition.contribution}:${definition.stage}`
}
function binding(definition: Definition) {
  return draft.value?.bindings.find(item => item.contribution === definition.contribution && item.stage === definition.stage)
}
function hasScope(value: Binding) {
  return [
    value.clientKeyIds,
    value.accountGroupIds,
    value.models,
    value.providerIds,
  ].some(items => items.length > 0)
}
function resetDraft() {
  if (!source.value)
    return
  draft.value = JSON.parse(JSON.stringify({ configuration: source.value.configuration, bindings: source.value.bindings }))
  restricted.value = new Set(source.value.definitions.filter((item) => {
    const current = binding(item)
    return current && hasScope(current)
  }).map(key))
  notice.value = ''
}
async function load() {
  if (disabled.value)
    return
  loading.value = true
  error.value = ''
  notice.value = ''
  try {
    const value = await getSettings()
    if (!mounted)
      return
    source.value = value
    resetDraft()
  }
  catch (cause) {
    if (mounted) {
      error.value = cause instanceof Error
        ? cause.message
        : '插件设置读取失败'
    }
  }
  finally {
    if (mounted)
      loading.value = false
  }
}
function toggle(definition: Definition, enabled: boolean) {
  if (!draft.value)
    return
  if (enabled) {
    draft.value.bindings.push({
      contribution: definition.contribution,
      stage: definition.stage,
      order: 0,
      failurePolicy: 'reject',
      providerIds: [],
      models: [],
      clientKeyIds: [],
      accountGroupIds: [],
      identityBindings: [],
    })
    restricted.value.delete(key(definition))
  }
  else {
    draft.value.bindings = draft.value.bindings.filter(item => item !== binding(definition))
  }
}
function setGlobal(definition: Definition, all: boolean) {
  const current = binding(definition)
  if (!current)
    return
  if (all) {
    restricted.value.delete(key(definition))
    current.clientKeyIds = []
    current.accountGroupIds = []
    current.models = []
    current.providerIds = []
  }
  else {
    restricted.value.add(key(definition))
    void loadResources('keys')
    void loadResources('groups')
  }
}
function setNames(definition: Definition, field: 'models' | 'providerIds', text: string) {
  const current = binding(definition)
  if (current) {
    current[field] = [
      ...new Set(text.split(/[\n,]/).map(item => item.trim()).filter(Boolean)),
    ]
  }
}
function choices(kind: 'keys' | 'groups', selected: string[]) {
  const catalog = catalogs[kind]
  const known = new Set(catalog.items.map(item => item.id))
  return [
    ...selected.filter(id => !known.has(id)).map(id => ({ value: id, label: id })),
    ...catalog.items.map(item => ({
      value: item.id,
      label: item.name,
      description: item.enabled ? undefined : '已停用',
      disabled: !item.enabled && !selected.includes(item.id),
    })),
  ]
}
async function loadResources(kind: 'keys' | 'groups', more = false) {
  const catalog = catalogs[kind]
  if (catalog.loading || (catalog.loaded && !more) || (more && !catalog.next))
    return
  catalog.loading = true
  catalog.error = ''
  try {
    const value = await getResources(kind, more
      ? catalog.next ?? ''
      : '')
    if (!mounted)
      return
    catalog.items = more
      ? [
          ...catalog.items,
          ...value.items.filter(item => !catalog.items.some(existing => existing.id === item.id)),
        ]
      : value.items
    catalog.next = value.nextCursor
    catalog.loaded = true
  }
  catch (cause) {
    if (mounted) {
      catalog.error = cause instanceof Error
        ? cause.message
        : '范围选项读取失败'
    }
  }
  finally {
    if (mounted)
      catalog.loading = false
  }
}
function openAdvanced(event: Event) {
  if ((event.target as HTMLDetailsElement).open && restricted.value.size) {
    void loadResources('keys')
    void loadResources('groups')
  }
}
async function save() {
  if (!source.value || !draft.value || disabled.value || !valid.value || !dirty.value)
    return
  saving.value = true
  error.value = ''
  notice.value = ''
  try {
    await saveSettings(source.value, draft.value.configuration, draft.value.bindings)
    if (!mounted)
      return
    notice.value = '已保存，正在重新加载插件页面'
    // 保存会发布新实例，下一次旧目标查询由宿主识别并刷新 iframe
    // 不导航或重载隔离页面，否则会触发宿主的页面保护
    const current = await getSettings()
    if (mounted) {
      source.value = current
      resetDraft()
      notice.value = '设置已保存'
    }
  }
  catch (cause) {
    if (mounted) {
      error.value = cause instanceof Error
        ? cause.message
        : '无法确认保存结果，请重新读取设置'
    }
  }
  finally {
    if (mounted)
      saving.value = false
  }
}
onMounted(() => void load())
onBeforeUnmount(() => {
  mounted = false
})
</script>

<template>
  <ZCard>
    <div class="mb-4 flex flex-wrap items-center gap-3">
      <span class="font-emphasis">插件设置</span>
      <span v-if="dirty" class="text-z-xs text-z-text-secondary">有未保存修改</span>
      <ZButton class="sm:ml-auto" size="small" :loading="loading" :disabled="saving" @click="load">
        重新读取设置
      </ZButton>
    </div>
    <div v-if="error" role="alert" class="mb-4 rounded-z bg-z-error-container p-3 text-z-error-on-container">
      {{ error }}
    </div>
    <div v-if="notice" role="status" class="mb-4 text-z-sm text-z-text-secondary">
      {{ notice }}
    </div>
    <form v-if="draft && source" class="space-y-4" @submit.prevent="save">
      <ZFormItem label="迁移等待阈值（秒）" description="从首次选号开始计时，达到阈值尝试换号，须小于本体总排队超时">
        <ZInputNumber v-model="draft.configuration.waitSeconds" aria-label="迁移等待阈值（秒）" :min="1" :max="110" :precision="0" :disabled="disabled" class="w-full sm:max-w-64" />
      </ZFormItem>
      <details class="rounded-z bg-z-fill-alter p-4" @toggle="openAdvanced">
        <summary class="cursor-pointer font-emphasis">
          高级设置
        </summary>
        <p class="my-3 text-z-xs text-z-text-secondary">
          顺序数值越小越先执行，0 为默认值，同阶段内生效
        </p>
        <div class="grid gap-4">
          <article v-for="definition in source.definitions" :key="key(definition)" class="space-y-4 rounded-z bg-z-bg-container p-4">
            <div>
              <ZSwitch :model-value="Boolean(binding(definition))" :label="definition.label" :disabled="disabled" @update:model-value="toggle(definition, $event)" />
            </div>
            <template v-if="binding(definition)">
              <div>
                <ZCheckbox :model-value="!restricted.has(key(definition))" label="适用于所有请求" :disabled="disabled" @update:model-value="setGlobal(definition, $event)" />
              </div>
              <div v-if="restricted.has(key(definition))" class="grid min-w-0 gap-4 sm:grid-cols-2">
                <ZFormItem v-for="kind in (['keys', 'groups'] as const)" :key="kind" :label="kind === 'keys' ? 'API Key 范围' : '账号分组范围'">
                  <ZSelect v-model="binding(definition)![kind === 'keys' ? 'clientKeyIds' : 'accountGroupIds']" :aria-label="`${definition.label} ${kind === 'keys' ? 'API Key 范围' : '账号分组范围'}`" :options="choices(kind, binding(definition)![kind === 'keys' ? 'clientKeyIds' : 'accountGroupIds'])" multiple filterable collapse-tags :max-collapse-tags="1" :loading="catalogs[kind].loading" :disabled="disabled" placeholder="不限" class="w-full min-w-0" />
                  <p v-if="catalogs[kind].error" role="alert" class="mt-2 text-z-xs text-z-error">
                    {{ catalogs[kind].error }}
                  </p>
                  <ZButton v-if="catalogs[kind].error || catalogs[kind].next" size="small" variant="link" :loading="catalogs[kind].loading" :disabled="disabled" @click="loadResources(kind, Boolean(catalogs[kind].next))">
                    {{ catalogs[kind].error ? '重新读取选项' : '加载更多选项' }}
                  </ZButton>
                </ZFormItem>
                <ZFormItem label="模型范围" description="每行一个完整模型名，精确匹配">
                  <ZTextarea :model-value="binding(definition)!.models.join('\n')" :aria-label="`${definition.label} 模型范围`" :rows="2" :disabled="disabled" placeholder="不限" @update:model-value="setNames(definition, 'models', $event)" />
                </ZFormItem>
                <ZFormItem v-if="definition.providerScope" label="Provider 范围" description="每行一个标识，例如 openai">
                  <ZTextarea :model-value="binding(definition)!.providerIds.join('\n')" :aria-label="`${definition.label} Provider 范围`" :rows="2" :disabled="disabled" placeholder="不限" @update:model-value="setNames(definition, 'providerIds', $event)" />
                </ZFormItem>
                <p class="text-z-xs text-z-text-secondary sm:col-span-2">
                  至少选择一项，同一维度内任选其一，不同维度须同时满足
                </p>
              </div>
              <div class="grid gap-4 sm:grid-cols-2">
                <ZFormItem label="执行顺序">
                  <ZInputNumber v-model="binding(definition)!.order" :aria-label="`${definition.label} 执行顺序`" :min="-2147483648" :max="2147483647" :precision="0" :disabled="disabled" class="w-full" />
                </ZFormItem>
                <ZFormItem label="插件失败时">
                  <ZSelect v-model="binding(definition)!.failurePolicy" :aria-label="`${definition.label} 插件失败时`" :options="failures" :disabled="disabled" class="w-full" />
                </ZFormItem>
              </div>
            </template>
          </article>
        </div>
        <p v-if="partial" class="mt-3 text-z-sm text-z-warning">
          完整换号与记录需要三个阶段同时启用，并确保请求范围相互覆盖
        </p>
      </details>
      <div class="flex flex-wrap items-center gap-3">
        <ZButton type="primary" native-type="submit" :loading="saving" :disabled="loading || !dirty || !valid">
          保存设置
        </ZButton>
        <ZButton :disabled="disabled || !dirty" @click="resetDraft">
          撤销修改
        </ZButton>
        <span class="text-z-xs text-z-text-secondary">与 CPR 原生插件设置同步，保存后重新加载页面</span>
      </div>
    </form>
    <p v-else-if="loading" role="status" class="text-z-sm text-z-text-secondary">
      正在读取插件设置
    </p>
  </ZCard>
</template>

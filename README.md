# CPR 会话排队迁移

在严格会话绑定下，账号选择持续等待达到阈值后，选择可用账号，并由 CPR 同步迁移会话绑定。后续请求跟随新账号，旧账号恢复空闲不会自动切回

这是独立插件，不修改 CPR 本体，不直接读取或写入宿主数据库、Redis 或凭据

## 安装

要求 CPR **>=3.41.0**，发行包提供 Linux x86_64 版本，运行环境需 glibc >=2.34

1. 打开 CPR「插件管理」→「安装」→「GitHub」
2. 仓库填写 `SilentFlower/cpr-session-failover`，选择稳定版附件
3. 核对附件和 SHA-256，接受完整信任后安装
4. 从左侧插件导航打开「会话切换记录」，点击工具栏的「插件设置」配置等待阈值，默认 **15 秒**
5. 本体应保留账号排队名额，最终排队超时必须大于此值

也可以从 [Releases](https://github.com/SilentFlower/cpr-session-failover/releases) 下载 `.tar.gz` 上传安装。GitHub 自动生成的源码压缩包不是安装包

插件命中的模型请求会使用请求级严格绑定，不改变全局设置。默认作用范围不限 Key，可在「高级设置」收窄到指定 Key、分组或模型；请求中间件、attempt 和账号调度须使用相同范围

升级保留现有配置，已保存的 5 秒不会被覆盖。要采用 15 秒，在扩展页的「插件设置」中修改迁移等待阈值并保存

## 等待与改绑

```text
会话绑定 A → A 繁忙 → 等待达到阈值 → 申请可用 B
                                      ↓
                          CPR 取得 B 名额并原子改绑
                                      ↓
                              后续会话请求跟随 B
```

- 计时从每次尝试首次进入账号选择开始，包含其后的排队及重新选号
- 成功取得账号名额即终止这一轮计时；上游错误后的新选号尝试重新计时
- 本插件介入响应流建立前的选号；响应流交付后的恢复由 CPR 处理
- **不检测上游首事件、首推理、首字或首正文超时**，不会因为开始生成后迟迟无输出而换号重发
- 同一会话同时只提出一个迁移意图；确认取得新账号后，其余等待者重新计算阈值
- 新账号名额申请失败时继续交给宿主重读容量；没有可用备选则继续原等待
- 本体总排队预算、Key 准入、账号资格和队列顺序仍然生效，插件不扩大这些上限
- 队列满可能在达到阈值前被本体拒绝；关闭账号排队或总预算不足时会留下阻止记录
- 已在旧账号执行的请求不被强制中断，其完成不会把绑定改回旧账号

目标账号按当前并发占用比例、权重和会话稳定散列选择；账号资格与真实租约由宿主最终裁决

## 原生续写边界

带 `previous_response_id` 的增量请求固定在原账号。带 `item_reference` 的请求和预热也不参与跨号恢复

在 CPR「插件管理」中为本实例启用 WebSocket 消息中间件，然后让 Codex 重新连接，可恢复根线程的原生续写排队超时。该阶段按宿主规则使用空范围，实际恢复仍受插件请求中间件和调度的适用范围约束

原账号在宿主排队时限内持续满载时，插件将发送前的 `concurrency_queue_timeout` 转成 `previous_response_not_found`，让支持该协议的客户端重发完整上下文。同一 Key、根线程、轮次和模型的重发在 30 秒内有一次优先选择备选账号的机会，由宿主取得租约并改绑。无可立即使用的备选账号时，重发交回宿主调度

恢复从宿主排队超时触发，需要开启账号排队。宿主的排队失败换号可以关闭。两个功能同时开启时，普通选号先按插件的迁移阈值尝试换号，达到宿主的排队时限后按宿主设置处理

没有显式会话身份的请求原样交给宿主处理，不根据 `prompt_cache_key` 或认证头推测会话。换号可能降低缓存命中

## 设置

| 参数 | 默认 | 范围 | 含义 |
| --- | ---: | --- | --- |
| `waitSeconds` | 15 | 1～110 秒 | 本次账号选择的持续等待阈值，须小于本体总排队超时 |

工具栏按钮打开的「插件设置」弹窗与 CPR 原生设置共用同一份配置。「高级设置」标签可调整三个阶段的开关、执行顺序、失败处理，以及 Key、分组、模型和适用阶段的 Provider 范围

- 执行顺序越小越先执行，0 为默认值，只比较同阶段处理器
- 失败处理为「拒绝请求」或「继续下一个处理器」，沿用 CPR 原生规则
- 空范围表示不限，同一维度内任选其一，不同维度同时满足
- 完整迁移与记录需要三个阶段同时启用，且请求范围相互覆盖
- 保存成功后显示确认，点击页面标题右侧的「刷新页面」按钮重新连接，再继续查询或修改，已有历史保留
- 并发修改会拒绝覆盖，可重新读取设置后确认，写入结果不确定时须刷新页面核对，不自动重试

CPR 未公开真实入队起点和绑定版本，纯插件使用首次选号后的持续等待计时；触发时刻还受宿主重评估、RPC 和日志保存耗时影响。协调范围是同一插件进程；多个 CPR 节点之间的绑定一致性由宿主原子操作保证，但插件不提供跨节点迁移去重

## 查看切换记录

从插件详情或左侧插件导航打开「会话切换记录」，支持按请求 ID 精确查询、按改绑结果筛选、分页和手动刷新。展开记录可查看完整账号 ID、会话摘要、等待耗时及错误分类

[记录页](https://github.com/SilentFlower/cpr-session-failover/releases/download/v0.3.2/records-light.png) · [设置弹窗](https://github.com/SilentFlower/cpr-session-failover/releases/download/v0.3.2/settings-dialog.png) · [高级设置](https://github.com/SilentFlower/cpr-session-failover/releases/download/v0.3.2/settings-advanced.png) · [记录详情](https://github.com/SilentFlower/cpr-session-failover/releases/download/v0.3.2/records-details.png)，截图使用隔离实例中的模拟记录

列表优先显示账号名称，详情保留完整 ID；名称在刷新时读取宿主当前账号资料，名称为空时使用邮箱，账号被删除或查询失败时保留 ID

页面时间由插件后端按北京时间（UTC+8）显示。仅在提出迁移或触发迁移保护时记录，直接取得账号的普通请求不会出现在列表中。匹配总数覆盖所有保留历史，已确认改绑数量仅统计当前页，均不能代表真实累计切换次数

也可在运行 CPR 的环境执行：

```bash
codex-proxy-rs plugin <配置ID> history --limit 50 --page 1
codex-proxy-rs plugin <配置ID> history --request-id req_xxx
```

管理员也可以通过 `GET /api/admin/plugins/extensions` 取得插件的 `instanceId`、`artifactSha256` 和 `revision`，再请求：

```text
GET /api/admin/plugins/extensions/{instanceId}/{artifactSha256}/{revision}/api/history?limit=50&page=1&requestId=req_xxx
```

记录保存在宿主插件私有状态，重启后可读取。正常运行日志同时发送给 `host.log`，普通日志可能被宿主限频，排查时以持久历史为准

历史不设置默认保留条数，不按条数或时间自动删除。查询通过 `page` 和 `limit` 分页，`limit` 是单页大小（1～500），不限制历史总数。`pagination.total` 返回匹配总数，后续页沿用首轮的 `asOf` 可固定时间范围，手动刷新会读取新记录。改绑结果使用 `result=all|changed|unchanged|unknown`

宿主实际配额为每个命名空间最多 16 MiB、10,000 个状态对象，单个状态值最多 256 KiB。索引与记录共同占用配额，不能把对象数当成可保存日志数。配额耗尽时已有记录保留，插件不发起无法完整记账的迁移，日志及页面显示写入失败

升级兼容已有 `audit/journal` 历史，新增记录使用 `history` 命名空间。未完成的写入单独计数，维护只补齐已经保存的记录，不重放选号。新版记录写入后，旧包缺少对应命名空间，CPR 可能拒绝直接回退，升级前按宿主方式备份私有数据

| 字段 | 含义 |
| --- | --- |
| `requestId`、`attemptIndex` | 关联 CPR 模型请求及尝试 |
| `sessionHash`、`clientKeyId`、`model` | 带 Key 隔离的会话摘要、认证后的 Key ID 和模型 |
| `schedulerWaitMs` | 插件计算的本轮持续选号等待 |
| `accountSelectionWaitMs` | 本体账号选择总耗时，含选择过程及账号排队 |
| `accountQueueWaitMs` | 本体诊断记录的账号队列等待，缺失保持空值 |
| `proposedAccountId` | 插件计划选择的目标，不能据此判断成功 |
| `actualAccountId`、`admittedAtMs` | 宿主 attempt 确认已经取得的实际账号与时间 |
| `fromAccountId`、`bindingChanged` | 从对应尝试的宿主诊断核实的前账号，以及该迁移意图是否得到改绑证据确认 |
| `originalAccountId` | 原生续写已确认的原账号，用于显示续写路径，独立于改绑证据 |
| `outcome`、`requestErrorCode` | 最终请求结果和已知错误分类 |
| `errorCode` | 取得账号后该尝试返回的错误分类 |
| `evidenceStatus` | `pending` 待补齐，`verified` 已核实，`unavailable` 缺少证据，`not_required` 为受限请求 |
| `terminalStatus` | `pending` 待查询终态，`known` 已取得终态，`unavailable` 无法取得终态 |

| `stage` | 含义 |
| --- | --- |
| `proposed` | 已持久化切换意图，尚未确认取得目标账号 |
| `admitted` | attempt 入口确认实际取得账号名额 |
| `admitted_by_trace` | 后台从本体时间线补回实际账号确认 |
| `candidate_unavailable` | 目标账号快照失效，交回宿主重新评估 |
| `different_account` | 本体实际取得其他账号，原迁移意图未命中 |
| `blocked` | 原生续写、预热或排队配置使本次请求不能主动迁移 |

**`bindingChanged=true` 才表示该意图已经由诊断核实改绑，`null` 表示未知**。固定账号请求或关键路由事实缺失时，不把调度偏好冒充原绑定

后台维护只对切换记录查询终态和脱敏诊断，不订阅全站请求或逐帧 WebSocket 事件。每批最多检查三条记录，长请求不会阻塞其他记录。前账号和改绑事实从对应请求的诊断补齐，不根据候选账号猜测；时间线被淘汰、诊断缺失或维护调用失败时会保留缺口。日志写入失败时不发起本次迁移，继续宿主原等待

记录不保存请求正文、完整会话 ID、Key 明文、令牌、认证头或原始诊断包

停用插件会停止后续超时迁移，保留已有绑定和私有历史；不会自动把会话迁回旧账号

## 开发与验证

详见 [开发说明](docs/development.md)。SDK 固定到公开 Git 提交，安装包兼容性由宿主版本、RPC 和能力版本共同校验；验证范围见 [验证记录](docs/validation.md)

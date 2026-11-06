# 开发与构建

Rust 仅通过公开 `gateway-plugin-sdk` 接入宿主，固定版本为 `789609dd53663b323e43567d2476f7f83b6b77a1`，不链接宿主 Core、Store 或 Provider

工具链为 Rust 1.99.1、Node.js 26.13.0 和 pnpm 12.6.0，`rust-toolchain.toml` 会选择 Rust 版本。页面使用 Vue 3 与固定发布包 `@codex-proxy/ui` v0.8.0，依赖和锁文件由 `frontend/` 管理

```bash
cargo fmt --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --locked
pnpm --dir frontend install --frozen-lockfile
pnpm --dir frontend lint
pnpm --dir frontend build
```

## 打包

先准备与当前 SDK 合同一致的 `cpr-plugin` CLI，再执行：

```bash
CPR_PLUGIN_CLI=/path/to/cpr-plugin bash scripts/package.sh
```

默认目标为 `x86_64-unknown-linux-gnu`，产物为 `dist/` 下的 `.tar.gz` 和 `.tar.gz.sha256`。脚本先构建页面和 Rust 二进制，再通过 CLI 将 `frontend/dist` 映射为包内 `web/` 资源

`scripts/licenses.py` 根据锁定的 Rust 依赖和前端运行依赖生成 `THIRD_PARTY_LICENSES.txt`，随安装包交付

## 代码职责

| 文件 | 职责 |
| --- | --- |
| `identity.rs` | 读取显式逻辑会话身份，生成带 Key 作用域的协调摘要，保护原生续写 |
| `policy.rs` | 每轮选号计时、同进程会话迁移意图协调和候选选择 |
| `audit.rs` | 意图、实际账号及终态记录，逐条持久记录、索引分页、宿主状态 CAS 与旧历史读取 |
| `diagnostics.rs` | 只读宿主诊断，按请求、尝试和实际账号补齐前账号证据 |
| `settings.rs` | 当前实例的原生配置读写、版本检查与范围目录 |
| `main.rs` | SDK 处理器组合、请求级严格绑定、取消回收、历史查询和维护 |
| `frontend/src/api/` | 宿主桥请求封装、历史与设置路由的响应合同校验 |
| `frontend/src/App.vue` | 历史筛选、分页、刷新与展开详情 |
| `frontend/src/components/SettingsDialog.vue` | 按需打开参数和阶段规则、原生目录选择与保存结果确认 |
| `frontend/src/presenter.ts` | 阶段、改绑证据及耗时的纯展示投影 |

页面在宿主隔离 iframe 内运行，仅通过 `window.codexProxyPlugin.request` 访问当前实例的 `history`、`settings` 和 `resources` 路由，不使用管理 Cookie 或直接请求宿主 HTTP。页面资源、主题和尺寸由宿主加载与同步，生产包不包含模拟数据

设置复用公开 UI 的 `ZDialog` 与 `ZSegmented`，默认页面只展示记录，打开弹窗才读取设置。弹窗打开期间背景记录保持渲染，只裁剪超出宿主可视高度的部分，关闭后恢复正常文档流，保留筛选、分页和展开状态。页面桥目标固定到实例 revision，保存配置会使旧目标失效。请求层在写入前阻止新读取并等待在途读取结束，成功后停止所有旧目标请求，由用户通过宿主标题旁的刷新按钮重新连接，不通过 409 或 iframe 导航触发刷新

明确的 400／409 拒绝允许修正或重新读取，其他失败和无效保存响应保持结果未知，刷新后核对实际配置，不重试写入。配置发布状态由请求层持有，设置弹窗和记录查询共用这一份状态

`pnpm --dir frontend dev` 提供源码预览，业务查询需要宿主桥。已安装插件读取包内资源，源码修改后必须重新构建、打包并切换版本

宿主绑定始终归 CPR 所有。插件只记录协调意图，不维护第二份会话账号绑定；显式选号返回后，宿主复核资格、取得租约并原子改绑。插件不会直接重试模型或绕过原生续写限制

历史使用每条记录的精确 CAS 与分页索引，不以条数或字节预算淘汰旧记录。索引先预约、记录再写入、索引最后提交，取消不产生索引不可达的记录。维护轮转检查未完成的预约并恢复已存在的正文，不会重放选号。旧 `audit/journal` 只读，更新其中一条记录时先复制该条到新存储，查询按 ID 合并去重

`settings.rs` 只使用握手确认的当前实例、制品和清单贡献项，复用宿主 `/api/admin/plugins/instances` 与 `instances/update` 的校验、revision 和发布流程。页面不传目标实例、密钥或任意 URL，设置不另存进私有状态。参数 schema 通过精确 `patternProperties` 接受旧配置的 `historyLimit` 整数字段，运行时忽略该值，下一次保存参数时移除

日志必须先持久化才能返回迁移决定。父调用取消时同步清除内存协调位置，不启动脱离父调用的回调；记录不确定时保留 `proposed` 或未知证据状态

请求中间件只修改本次有效运行设置中的亲和模式，正文和头部原样交还 SDK，响应流继续使用宿主惰性正文与取消机制

## 发布

提交使用英文 Conventional Commits。版本同时更新 `Cargo.toml`、`Cargo.lock`、`plugin.json` 和前端包信息，构建对应版本的安装包后创建同名 Git tag

GitHub Release 附带 `.tar.gz` 和校验文件；稳定版使用非草稿、非预发行版本，CPR 才能通过留空标签查询最新稳定版。自动源码压缩包不能代替插件附件

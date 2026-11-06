# 开发与构建

项目仅依赖公开 `gateway-plugin-sdk`，固定版本为 `789609dd53663b323e43567d2476f7f83b6b77a1`，不链接宿主 Core、Store 或 Provider

工具链为 Rust 1.99.1，`rust-toolchain.toml` 会选择该版本

```bash
cargo fmt --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --locked
```

## 打包

先准备与当前 SDK 合同一致的 `cpr-plugin` CLI，再执行：

```bash
CPR_PLUGIN_CLI=/path/to/cpr-plugin bash scripts/package.sh
```

默认目标为 `x86_64-unknown-linux-gnu`，产物为 `dist/` 下的 `.tar.gz` 和 `.tar.gz.sha256`。CLI 只校验与打包，Rust 二进制由脚本先行构建

`scripts/licenses.py` 根据锁定的目标依赖生成 `THIRD_PARTY_LICENSES.txt`，随安装包交付

## 代码职责

| 文件 | 职责 |
| --- | --- |
| `identity.rs` | 读取显式逻辑会话身份，生成带 Key 作用域的协调摘要，保护原生续写 |
| `policy.rs` | 每轮选号计时、同进程会话迁移意图协调和候选选择 |
| `audit.rs` | 意图、实际账号及终态记录，宿主私有状态 CAS 和有限保留 |
| `diagnostics.rs` | 只读宿主诊断，按请求、尝试和实际账号补齐前账号证据 |
| `main.rs` | SDK 处理器组合、请求级严格绑定、取消回收、历史查询和维护 |

宿主绑定始终归 CPR 所有。插件只记录协调意图，不维护第二份会话账号绑定；显式选号返回后，宿主复核资格、取得租约并原子改绑。插件不会直接重试模型或绕过原生续写限制

日志必须先持久化才能返回迁移决定。父调用取消时同步清除内存协调位置，不启动脱离父调用的回调；记录不确定时保留 `proposed` 或未知证据状态

请求中间件只修改本次有效运行设置中的亲和模式，正文和头部原样交还 SDK，响应流继续使用宿主惰性正文与取消机制

## 发布

提交使用英文 Conventional Commits。版本同时更新 `Cargo.toml`、`Cargo.lock` 和 `plugin.json`，构建对应版本的安装包后创建同名 Git tag

GitHub Release 附带 `.tar.gz` 和校验文件；稳定版使用非草稿、非预发行版本，CPR 才能通过留空标签查询最新稳定版。自动源码压缩包不能代替插件附件

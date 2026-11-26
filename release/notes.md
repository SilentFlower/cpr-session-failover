# v0.4.7

降低插件处理 WebSocket 消息和大正文 SSE 请求的 CPU 开销。RPC 使用缓冲读写，原生续写重发标识按 WebSocket 请求的需要计算。

安装包适用于 Linux x86_64，兼容 CPR 3.52.1 及以上版本。

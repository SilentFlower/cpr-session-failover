//! 严格会话排队迁移的策略、脱敏身份与持久切换记录

pub mod audit;
pub mod diagnostics;
pub mod identity;
pub mod policy;

use gateway_plugin_sdk::{ErrorCode, PluginFault};

/// 创建插件自身的安全错误，不携带请求或宿主原始响应
/// @param message 面向管理员的中文说明
/// @return 领域拒绝错误
pub fn fault(message: impl Into<String>) -> PluginFault {
    PluginFault::new(ErrorCode::Rejected, message)
}

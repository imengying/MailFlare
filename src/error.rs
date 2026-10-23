//! 统一的错误类型。
//!
//! 所有 API handler 都返回 `Result<Response, AppError>`，由入口统一转换成
//! JSON 错误响应；这样 handler 内部可以直接用 `?`，不必层层手写错误响应。

use serde::Serialize;
use worker::Response;

/// 业务错误。刻意保持精简：只有前端真正需要区分的几种语义。
#[derive(Debug)]
pub enum AppError {
    /// 400：请求参数/请求体不合法
    BadRequest(String),
    /// 401：未登录或 token 失效
    Unauthorized(String),
    /// 403：Turnstile 校验失败、密码错误、登录被限流等
    Forbidden(String),
    /// 404：资源不存在
    NotFound,
    /// 405：路径存在但方法不支持
    MethodNotAllowed(String),
    /// 500：内部错误（D1/R2/网络等）
    Internal(String),
}

impl AppError {
    /// 供日志与响应使用的短消息，避免把内部细节直接抛给前端。
    fn message(&self) -> String {
        match self {
            Self::BadRequest(m)
            | Self::Unauthorized(m)
            | Self::Forbidden(m)
            | Self::MethodNotAllowed(m) => m.clone(),
            Self::NotFound => "资源不存在".to_string(),
            Self::Internal(_) => "服务器内部错误".to_string(),
        }
    }

    fn status(&self) -> u16 {
        match self {
            Self::BadRequest(_) => 400,
            Self::Unauthorized(_) => 401,
            Self::Forbidden(_) => 403,
            Self::NotFound => 404,
            Self::MethodNotAllowed(_) => 405,
            Self::Internal(_) => 500,
        }
    }

    /// 转换成 JSON 响应，附带 `cache-control: no-store`（错误不该被缓存）。
    pub fn into_response(self) -> Response {
        // 内部错误把细节打到 Worker 日志里，响应只给通用文案。
        if let Self::Internal(detail) = &self {
            worker::console_error!("[mailflare] internal error: {detail}");
        }

        #[derive(Serialize)]
        struct Body {
            ok: bool,
            error: String,
        }

        let body = Body {
            ok: false,
            error: self.message(),
        };

        let headers = worker::Headers::new();
        let _ = headers.set("cache-control", "no-store");
        match worker::ResponseBuilder::new()
            .with_headers(headers)
            .from_json(&body)
        {
            Ok(resp) => resp.with_status(self.status()),
            // 理论上不会走到这里（内存耗尽时才有可能）。`ResponseBuilder::empty()`
            // 是不返回 Result 的，用它兜底可以保证绝不 panic —— Worker 里一次
            // panic 会中断整个请求，连日志都难看清。
            Err(_) => worker::ResponseBuilder::new().with_status(500).empty(),
        }
    }
}

impl From<worker::Error> for AppError {
    fn from(e: worker::Error) -> Self {
        Self::Internal(format!("worker error: {e}"))
    }
}

impl From<serde_json::Error> for AppError {
    fn from(e: serde_json::Error) -> Self {
        Self::Internal(format!("json error: {e}"))
    }
}

impl From<url::ParseError> for AppError {
    fn from(e: url::ParseError) -> Self {
        Self::BadRequest(format!("非法 URL: {e}"))
    }
}

/// 便捷别名。
pub type AppResult<T> = Result<T, AppError>;

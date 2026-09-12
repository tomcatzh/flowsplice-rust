# 用 FlowSplice 接入自己的业务

FlowSplice 0.4.0 提供两个业务 SDK 入口：

| 依赖 | 用途 |
| --- | --- |
| `flowsplice-home-core` | 在业务服务端接收授权连接 |
| `flowsplice-travel-core` | 在业务客户端完成注册、发现和连接 |

业务消息格式、请求处理、应用权限和界面由你的项目实现。

## 添加依赖

新建自己的 Rust 项目，按角色选用下面的 Git 依赖；同时实现两端才需要两项。这里使用 `0.4.0` 标签，不是 crates.io 安装方式。

```toml
[dependencies]
flowsplice-home-core = { git = "https://github.com/tomcatzh/flowsplice-rust", tag = "0.4.0" }
flowsplice-travel-core = { git = "https://github.com/tomcatzh/flowsplice-rust", tag = "0.4.0", default-features = false }
```

Travel 关闭默认特性后，可作为无内置 Web 界面的客户端运行。下面的 Rust 片段还使用普通依赖 `anyhow = "1"`、`serde_json = "1"` 和启用 `rt-multi-thread`、`macros`、`io-util` 的 Tokio 1。

Cargo 会获取 Git 仓库源码，再构建所选包的依赖图；无需把你的项目放入 FlowSplice workspace。两个入口仍有内部传递依赖，不代表整个构建只有两个包。应用应提交自己的 `Cargo.lock`。

## 接入前准备

需要可用的 FlowSplice Server、Relay，以及管理员配置好的业务 Home。Home 使用已有的证书、私钥、部署信任、授权缓存和业务服务授权文件；`HomeRuntime::load` 不负责注册或签发它们。

客户端需要业务描述符、Relay 地址和通过独立可信渠道取得的部署根公钥。注册需要审批，成功安装后再启动客户端。保留安装目录和身份文件，不要每次启动重新注册。配置、私钥和真实部署材料应保存在应用自己的数据目录。

## 业务 Home

从 `flowsplice_home_core` 直接取得 `HomeRuntimeConfig`、`Service`、`ServiceProtocol`、`SocketServices` 和监听器类型。配置中的服务 ID、协议必须与已获批的服务一致。

下面接收一个 TCP 业务连接后关闭，展示完整运行时生命周期。`config` 由调用方读取管理员配置；业务处理回调必须自行处理消息边界、超时及授权生命周期。

```rust
use anyhow::Result;
use flowsplice_home_core::{BoxStream, HomeRuntime, HomeRuntimeConfig, ServicePeer, SocketServices};
use std::{future::Future, sync::Arc};

pub async fn serve_one<F, Fut>(
    config: HomeRuntimeConfig,
    service_id: String,
    handle: F,
) -> Result<()>
where
    F: FnOnce(BoxStream, ServicePeer) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let services = Arc::new(SocketServices::default());
    let mut listener = services.bind_tcp(service_id, 16)?;
    let runtime = Arc::new(HomeRuntime::load(config, services)?);
    let running = Arc::clone(&runtime);
    let mut task = tokio::spawn(async move { running.run_serving().await });
    let work = async {
        let (stream, peer) = listener.accept().await?;
        handle(stream, peer).await
    };
    let result = tokio::select! {
        finished = &mut task => {
            runtime.shutdown().await;
            return finished?;
        }
        result = work => result,
    };
    runtime.shutdown().await;
    task.await??;
    result
}
```

持续服务时把接收与业务任务管理放在应用循环中，并提供停止信号。`run_serving()` 必须与接收循环并发执行；退出时调用 `shutdown().await`，再等待 serving 任务结束。

UDP 使用 `bind_udp`；`accept()` 返回数据报接口和认证对端。`ServicePeer` 提供身份及 `lifetime`：业务操作应检查 `is_active()`，等待期间可监听 `ended()`。缓冲区里还有数据不表示授权仍然有效。

## 业务 Travel Client

两个注册入口共用 `business::BusinessEnrollmentOptions`：填写客户端 ID、安装目录、Relay IP 与端口、部署根公钥内容、私钥密码和审批等待秒数。`root` 参数也是公钥内容，不是文件路径。

选择与业务授权范围对应的一种模式：

- 固定目标：解析 `BusinessDescriptor`，调用 `business::enroll(options, descriptor, on_progress).await`；以后用相同描述符调用 `TravelCore::start_business`，连接返回的 `approved.binding`。
- 服务类别：解析 `ServiceClassDescriptor`，调用 `service_class::enroll(options, descriptor, label, on_progress).await`；以后调用 `TravelCore::start_service_class`，再用 `service_class_targets(&approved).await` 取得已验证目标。由应用选择目标，构造 `ServiceBinding`。发现结果可能暂时为空。

这两个异步函数展示注册调用，参数由应用提供：

```rust
pub async fn enroll_exact(
    options: flowsplice_travel_core::business::BusinessEnrollmentOptions,
    descriptor_json: &str,
) -> anyhow::Result<()> {
    let descriptor: flowsplice_travel_core::BusinessDescriptor =
        serde_json::from_str(descriptor_json)?;
    flowsplice_travel_core::business::enroll(options, descriptor, |_| {}).await
}

pub async fn enroll_class(
    options: flowsplice_travel_core::business::BusinessEnrollmentOptions,
    descriptor_json: &str,
    label: String,
) -> anyhow::Result<()> {
    let descriptor: flowsplice_travel_core::ServiceClassDescriptor =
        serde_json::from_str(descriptor_json)?;
    flowsplice_travel_core::service_class::enroll(options, descriptor, label, |_| {}).await
}
```

以下固定目标示例要求已经完成安装，而且描述符指定 TCP 服务。回调拿到的是进程内异步流，业务结束后关闭运行时：

```rust
pub async fn use_exact<F, Fut>(
    config: &std::path::Path,
    password: &str,
    root: &str,
    descriptor: &flowsplice_travel_core::BusinessDescriptor,
    handle: F,
) -> anyhow::Result<()>
where
    F: FnOnce(flowsplice_travel_core::SocketStream) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    let (runtime, approved) = flowsplice_travel_core::TravelCore::start_business(
        config, password, root, descriptor,
    ).await?;
    let result = async {
        let stream = runtime.connect_tcp(&approved.binding).await?;
        handle(stream).await
    }.await;
    runtime.shutdown().await;
    result
}
```

UDP 服务使用 `connect_udp`。这些连接不需要客户端物理监听端口或本地端口映射。同一运行时可以服务多个业务连接；应用退出时调用 `shutdown()`。

上述片段是接入函数，不是自带证书和部署环境的可运行服务。首次验证应在自己的测试部署中完成注册审批、业务收发和关闭；编译通过不能代替真实连接测试。
